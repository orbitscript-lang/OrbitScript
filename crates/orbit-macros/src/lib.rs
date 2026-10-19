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
//!
//! Hand-rolled over `proc_macro`'s token trees, so it adds no build
//! dependency: it only needs the signature's shape, never the body.

use proc_macro::{Delimiter, Spacing, TokenStream, TokenTree};

#[proc_macro_attribute]
pub fn native(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand(attr, item.clone()) {
        Ok(generated) => {
            let mut out = item;
            out.extend(generated);
            out
        }
        Err(message) => {
            let mut out = item;
            out.extend(compile_error(&message));
            out
        }
    }
}

fn compile_error(message: &str) -> TokenStream {
    format!("::core::compile_error!({message:?});")
        .parse()
        .expect("a compile_error! call parses")
}

/// One parameter: its type's tokens, and whether it is the stack.
struct Param {
    ty: String,
    stack: bool,
}

fn expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let name_override = parse_attr(attr)?;
    let mut tokens = item.into_iter().peekable();
    let mut vis = String::new();
    // Attributes, then the visibility, then `fn`.
    loop {
        match tokens.next() {
            Some(TokenTree::Punct(p)) if p.as_char() == '#' => {
                tokens.next();
            }
            Some(TokenTree::Ident(i)) if i.to_string() == "pub" => {
                vis.push_str("pub");
                if let Some(TokenTree::Group(g)) = tokens.peek()
                    && g.delimiter() == Delimiter::Parenthesis
                {
                    vis.push_str(&g.to_string());
                    tokens.next();
                }
            }
            Some(TokenTree::Ident(i)) if i.to_string() == "fn" => break,
            Some(other) => {
                return Err(format!(
                    "#[native] goes on a plain `fn`, not `{other}` (no const, async, unsafe or extern)"
                ));
            }
            None => return Err("#[native] goes on a function".into()),
        }
    }
    let Some(TokenTree::Ident(name)) = tokens.next() else {
        return Err("#[native]: expected the function's name".into());
    };
    let name = name.to_string();
    let params = match tokens.next() {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => {
            parse_params(g.stream())?
        }
        Some(TokenTree::Punct(p)) if p.as_char() == '<' => {
            return Err(
                "#[native] functions can't be generic: a native is one function per type".into(),
            );
        }
        _ => return Err("#[native]: expected the parameter list".into()),
    };

    // Borrowed arguments are read through a shared borrow of the stack, so
    // the body can't have the stack mutably at the same time. Say so here
    // rather than leave rustc to point at the attribute.
    if params.iter().any(|p| p.stack)
        && let Some(p) = params.iter().find(|p| !p.stack && p.ty.contains('&'))
    {
        return Err(format!(
            "#[native] fn `{name}` takes the stack, so its arguments can't borrow from it: \
             make `{}` owned (`Vec<u8>`, `String`) or an index (`Any`)",
            p.ty
        ));
    }

    let lua_name = name_override.unwrap_or_else(|| name.clone());
    let mut checks = String::new();
    let mut gets = String::new();
    let mut call_args = Vec::new();
    let mut arg = 0;
    for (k, p) in params.iter().enumerate() {
        if p.stack {
            call_args.push("&mut *__s".to_string());
            continue;
        }
        arg += 1;
        let ty = &p.ty;
        checks.push_str(&format!(
            "let __c{k} = <{ty} as ::orbit::stack::Arg>::check(__s, {arg})?;\n"
        ));
        gets.push_str(&format!(
            "let __v{k} = <{ty} as ::orbit::stack::Arg>::get(&*__s, {arg}, __c{k});\n"
        ));
        call_args.push(format!("__v{k}"));
    }
    let call_args = call_args.join(", ");
    let generated = format!(
        "#[allow(non_camel_case_types)]
        #[doc(hidden)]
        {vis} struct {name} {{}}

        impl ::orbit::stack::Native for {name} {{
            const NAME: &'static str = {lua_name:?};

            #[inline]
            fn call(__s: &mut ::orbit::stack::Stack<'_>)
                -> ::orbit::stack::StackResult<::orbit::stack::Return>
            {{
                {checks}
                let __r = {{
                    {gets}
                    {name}({call_args})
                }};
                ::orbit::stack::Results::push_results(__r, __s)
            }}
        }}"
    );
    generated
        .parse()
        .map_err(|e| format!("#[native] generated bad code: {e:?}"))
}

/// `name = "lib.f"`, or nothing.
fn parse_attr(attr: TokenStream) -> Result<Option<String>, String> {
    let tokens: Vec<TokenTree> = attr.into_iter().collect();
    match &tokens[..] {
        [] => Ok(None),
        [
            TokenTree::Ident(k),
            TokenTree::Punct(eq),
            TokenTree::Literal(v),
        ] if k.to_string() == "name" && eq.as_char() == '=' => {
            let v = v.to_string();
            match v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                Some(s) if !s.contains('\\') => Ok(Some(s.to_string())),
                _ => Err("#[native(name = \"...\")] takes a plain string".into()),
            }
        }
        _ => Err("#[native] takes nothing or `name = \"lib.f\"`".into()),
    }
}

/// Split a parameter list at its top-level commas, and each parameter at
/// its first `:` (the pattern before it is never needed: the function is
/// called positionally).
fn parse_params(stream: TokenStream) -> Result<Vec<Param>, String> {
    let mut params = Vec::new();
    let mut current: Vec<TokenTree> = Vec::new();
    let mut depth = 0i32;
    let mut prev_joint_dash = false;
    for t in stream {
        if let TokenTree::Punct(p) = &t {
            match p.as_char() {
                '<' => depth += 1,
                // `->` in a function-pointer type isn't a closing bracket.
                '>' if !prev_joint_dash => depth -= 1,
                ',' if depth == 0 => {
                    params.push(param(std::mem::take(&mut current))?);
                    prev_joint_dash = false;
                    continue;
                }
                _ => {}
            }
            prev_joint_dash = p.as_char() == '-' && p.spacing() == Spacing::Joint;
        } else {
            prev_joint_dash = false;
        }
        current.push(t);
    }
    if !current.is_empty() {
        params.push(param(current)?);
    }
    Ok(params)
}

fn param(tokens: Vec<TokenTree>) -> Result<Param, String> {
    let colon = tokens
        .iter()
        .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ':'))
        .ok_or("#[native]: each parameter needs a type (`self` isn't allowed)")?;
    let ty: TokenStream = tokens[colon + 1..].iter().cloned().collect();
    let ty = ty.to_string();
    let compact: String = ty.chars().filter(|c| !c.is_whitespace()).collect();
    let stack = compact.starts_with("&mut") && compact.contains("Stack");
    Ok(Param { ty, stack })
}
