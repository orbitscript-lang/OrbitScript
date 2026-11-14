//! `#[native]`: a Rust function as a stack-API native.
//!
//! ```ignore
//! #[orbit::native(name = "os.difftime")]
//! fn difftime(t2: i64, t1: i64) -> f64 {
//!     t2 as f64 - t1 as f64
//! }
//! // later: s.push_native::<difftime>();
//! ```
//!
//! The function stays as written. Next to it the attribute adds a type of
//! the same name implementing `orbit::stack::Native`, whose `call` reads
//! argument `i` as the `i`th parameter's type (`orbit::stack::Arg`), calls
//! the function, and pushes what it returns (`orbit::stack::Results`). Two
//! kinds of parameter take no argument: `&mut Stack<'_>` gets the stack
//! itself, and `State<T>` the isolate's per-isolate state of type `T`
//! (`orbit::stack::State`). A panic in the function is caught at its entry
//! and raised as a script error, as a `Call` closure's is.
//!
//! Options, comma-separated:
//! - `name = "lib.f"`: the native's name in messages, and (its last part)
//!   the field it is registered under. The function's name by default.
//! - `crate = path`: where the `orbit` API is, `::orbit` by default; for
//!   code that reaches it under another name.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{
    Error, FnArg, ItemFn, LitStr, Path, ReturnType, Safety, Token, Type, TypePath, TypeReference,
    parse_macro_input,
};

#[proc_macro_attribute]
pub fn native(attr: TokenStream, item: TokenStream) -> TokenStream {
    let options = parse_macro_input!(attr as Options);
    let f = parse_macro_input!(item as ItemFn);
    let generated = expand(&f, options).unwrap_or_else(Error::into_compile_error);
    quote!(#f #generated).into()
}

/// The attribute's options: `name = "lib.f"` and `crate = path`, each at
/// most once, in any order.
struct Options {
    name: Option<LitStr>,
    krate: Option<Path>,
}

const USAGE: &str = "#[native] takes `name = \"lib.f\"` and `crate = path`";

impl syn::parse::Parse for Options {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Options> {
        let mut options = Options {
            name: None,
            krate: None,
        };
        while !input.is_empty() {
            if input.peek(Token![crate]) {
                let key = input.parse::<Token![crate]>()?;
                input.parse::<Token![=]>()?;
                if options.krate.is_some() {
                    return Err(Error::new(key.span, "`crate` given twice"));
                }
                options.krate = Some(input.call(Path::parse_mod_style)?);
            } else {
                let key: syn::Ident = input.parse()?;
                if key != "name" {
                    return Err(Error::new(key.span(), USAGE));
                }
                input.parse::<Token![=]>()?;
                if options.name.is_some() {
                    return Err(Error::new(key.span(), "`name` given twice"));
                }
                options.name = Some(input.parse()?);
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        Ok(options)
    }
}

/// Whether `ty` is a path whose last segment is `name`. Types can't be
/// resolved in a macro, so parameters are recognized by name.
fn is_named(ty: &Type, name: &str) -> bool {
    matches!(ty, Type::Path(TypePath { qself: None, path: Path { segments, .. }, .. })
        if segments.last().is_some_and(|s| s.ident == name))
}

/// Whether `ty` is `&mut Stack<..>` (any path ending in `Stack`).
fn is_stack(ty: &Type) -> bool {
    match ty {
        Type::Reference(TypeReference {
            mutability: Some(_),
            elem,
            ..
        }) => is_named(elem, "Stack"),
        _ => false,
    }
}

/// Whether `ty` is `State<T>` (any path ending in `State`).
fn is_state(ty: &Type) -> bool {
    is_named(ty, "State")
}

/// Whether `ty` borrows anything (`&[u8]`, `Option<&str>`).
fn borrows(ty: &Type) -> bool {
    struct Finder(bool);
    impl<'ast> syn::visit::Visit<'ast> for Finder {
        fn visit_type_reference(&mut self, _: &'ast TypeReference) {
            self.0 = true;
        }
    }
    let mut finder = Finder(false);
    syn::visit::visit_type(&mut finder, ty);
    finder.0
}

fn expand(f: &ItemFn, options: Options) -> syn::Result<proc_macro2::TokenStream> {
    let krate = match options.krate {
        Some(path) => quote!(#path),
        None => quote!(::orbit),
    };
    let sig = &f.sig;
    let unsupported = sig
        .constness
        .as_ref()
        .map(|t| t.span())
        .or(sig.asyncness.as_ref().map(|t| t.span()))
        .or(match &sig.safety {
            Safety::Default => None,
            safety => Some(safety.span()),
        })
        .or(sig.abi.as_ref().map(|a| a.span()));
    if let Some(span) = unsupported {
        return Err(Error::new(span, "#[native] goes on a plain `fn`"));
    }
    if !sig.generics.params.is_empty() {
        return Err(Error::new(
            sig.generics.span(),
            "#[native] functions can't be generic: a native is one function per type",
        ));
    }
    if let Some(v) = &sig.variadic {
        return Err(Error::new(v.span(), "use `orbit::stack::Rest` for `...`"));
    }

    let mut types = Vec::new();
    for input in &sig.inputs {
        match input {
            FnArg::Receiver(r) => {
                return Err(Error::new(r.span(), "#[native] functions take no `self`"));
            }
            FnArg::Typed(p) => types.push(&*p.ty),
        }
    }
    // Borrowed arguments are read through a shared borrow of the stack, so
    // the body can't also have the stack mutably.
    if types.iter().any(|t| is_stack(t)) {
        if let Some(t) = types.iter().find(|t| !is_stack(t) && borrows(t)) {
            return Err(Error::new(
                t.span(),
                "this function takes the stack, so its arguments can't borrow from it: \
             use an owned type (`Vec<u8>`, `String`) or an index (`Any`)",
            ));
        }
    }

    let ident = &sig.ident;
    let vis = &f.vis;
    let lua_name = options
        .name
        .unwrap_or_else(|| LitStr::new(&ident.to_string(), ident.span()));
    let mut checks = Vec::new();
    let mut gets = Vec::new();
    let mut args = Vec::new();
    let mut arg = 0i32;
    for (k, ty) in types.iter().enumerate() {
        if is_stack(ty) {
            args.push(quote!(&mut *__s));
            continue;
        }
        let v = format_ident!("__v{k}");
        if is_state(ty) {
            // Read before the arguments, which may borrow the stack.
            checks.push(quote_spanned! {ty.span()=>
                let #v = <#ty>::of(__s);
            });
            args.push(quote!(#v));
            continue;
        }
        arg += 1;
        let c = format_ident!("__c{k}");
        // Spanned at the type, so a type that isn't an `Arg` is reported
        // there.
        checks.push(quote_spanned! {ty.span()=>
            let #c = <#ty as #krate::stack::Arg>::check(__s, #arg)?;
        });
        gets.push(quote_spanned! {ty.span()=>
            let #v = <#ty as #krate::stack::Arg>::get(&*__s, #arg, #c);
        });
        args.push(quote!(#v));
    }
    let push = match &sig.output {
        // `impl Trait` can't be named here.
        ReturnType::Type(_, ty) if !matches!(**ty, Type::ImplTrait(_)) => {
            quote_spanned!(ty.span()=> <#ty as #krate::stack::Results>::push_results)
        }
        _ => quote_spanned!(Span::call_site()=> #krate::stack::Results::push_results),
    };

    Ok(quote! {
        #[allow(non_camel_case_types)]
        #[doc(hidden)]
        #vis struct #ident {}

        impl #krate::stack::Native for #ident {
            const NAME: &'static str = #lua_name;

            #[inline]
            fn call(
                __s: &mut #krate::stack::Stack<'_>,
            ) -> #krate::stack::StackResult<#krate::stack::Return> {
                #krate::stack::catch_panic(__s, |__s: &mut #krate::stack::Stack<'_>| {
                    #(#checks)*
                    let __r = {
                        #(#gets)*
                        #ident(#(#args),*)
                    };
                    #push(__r, __s)
                })
            }
        }
    })
}
