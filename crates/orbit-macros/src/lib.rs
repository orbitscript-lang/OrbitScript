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
//! the function, and pushes what it returns (`orbit::stack::Results`). A
//! parameter of type `&mut Stack<'_>` gets the stack itself and takes no
//! argument.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{
    Error, FnArg, ItemFn, LitStr, Path, ReturnType, Token, Type, TypePath, TypeReference,
    parse_macro_input,
};

#[proc_macro_attribute]
pub fn native(attr: TokenStream, item: TokenStream) -> TokenStream {
    let name = parse_macro_input!(attr as Options).name;
    let f = parse_macro_input!(item as ItemFn);
    let generated = expand(&f, name).unwrap_or_else(Error::into_compile_error);
    quote!(#f #generated).into()
}

/// The attribute's arguments: nothing, or `name = "lib.f"`.
struct Options {
    name: Option<LitStr>,
}

impl syn::parse::Parse for Options {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Options> {
        if input.is_empty() {
            return Ok(Options { name: None });
        }
        let key: syn::Ident = input.parse()?;
        if key != "name" {
            return Err(Error::new(
                key.span(),
                "#[native] takes only `name = \"lib.f\"`",
            ));
        }
        input.parse::<Token![=]>()?;
        let name = input.parse()?;
        if !input.is_empty() {
            return Err(input.error("#[native] takes only `name = \"lib.f\"`"));
        }
        Ok(Options { name: Some(name) })
    }
}

/// Whether `ty` is `&mut Stack<..>` (any path ending in `Stack`). Types
/// can't be resolved in a macro, so this goes by the name.
fn is_stack(ty: &Type) -> bool {
    let Type::Reference(TypeReference {
        mutability: Some(_),
        elem,
        ..
    }) = ty
    else {
        return false;
    };
    matches!(&**elem, Type::Path(TypePath { qself: None, path: Path { segments, .. } })
        if segments.last().is_some_and(|s| s.ident == "Stack"))
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

fn expand(f: &ItemFn, name: Option<LitStr>) -> syn::Result<proc_macro2::TokenStream> {
    let sig = &f.sig;
    let unsupported = sig
        .constness
        .as_ref()
        .map(|t| t.span())
        .or(sig.asyncness.as_ref().map(|t| t.span()))
        .or(sig.unsafety.as_ref().map(|t| t.span()))
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
    if types.iter().any(|t| is_stack(t))
        && let Some(t) = types.iter().find(|t| !is_stack(t) && borrows(t))
    {
        return Err(Error::new(
            t.span(),
            "this function takes the stack, so its arguments can't borrow from it: \
             use an owned type (`Vec<u8>`, `String`) or an index (`Any`)",
        ));
    }

    let ident = &sig.ident;
    let vis = &f.vis;
    let lua_name = name.unwrap_or_else(|| LitStr::new(&ident.to_string(), ident.span()));
    let mut checks = Vec::new();
    let mut gets = Vec::new();
    let mut args = Vec::new();
    let mut arg = 0i32;
    for (k, ty) in types.iter().enumerate() {
        if is_stack(ty) {
            args.push(quote!(&mut *__s));
            continue;
        }
        arg += 1;
        let (c, v) = (format_ident!("__c{k}"), format_ident!("__v{k}"));
        // Spanned at the type, so a type that isn't an `Arg` is reported
        // there.
        checks.push(quote_spanned! {ty.span()=>
            let #c = <#ty as ::orbit::stack::Arg>::check(__s, #arg)?;
        });
        gets.push(quote_spanned! {ty.span()=>
            let #v = <#ty as ::orbit::stack::Arg>::get(&*__s, #arg, #c);
        });
        args.push(quote!(#v));
    }
    let push = match &sig.output {
        // `impl Trait` can't be named here.
        ReturnType::Type(_, ty) if !matches!(**ty, Type::ImplTrait(_)) => {
            quote_spanned!(ty.span()=> <#ty as ::orbit::stack::Results>::push_results)
        }
        _ => quote_spanned!(Span::call_site()=> ::orbit::stack::Results::push_results),
    };

    Ok(quote! {
        #[allow(non_camel_case_types)]
        #[doc(hidden)]
        #vis struct #ident {}

        impl ::orbit::stack::Native for #ident {
            const NAME: &'static str = #lua_name;

            #[inline]
            fn call(
                __s: &mut ::orbit::stack::Stack<'_>,
            ) -> ::orbit::stack::StackResult<::orbit::stack::Return> {
                #(#checks)*
                let __r = {
                    #(#gets)*
                    #ident(#(#args),*)
                };
                #push(__r, __s)
            }
        }
    })
}
