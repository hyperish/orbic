//! `ttl!`, `key!`, `tag!`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Expr, ExprLit, Lit, LitStr, Token};

use crate::util::error_at;

pub(crate) fn ttl(input: TokenStream) -> syn::Result<TokenStream> {
    let lit: LitStr = syn::parse2(input.clone()).map_err(|_| {
        error_at(
            &input,
            "`ttl!` takes one string literal",
            "write `ttl!(\"30s\")`",
        )
    })?;
    let d = hyve_parser::parse_duration(&lit.value()).map_err(|d| {
        let help = d
            .help()
            .first()
            .cloned()
            .unwrap_or_else(|| "write e.g. `30s`, `5m` or `1h30m`".into());
        error_at(
            &lit,
            format!("invalid TTL {:?}: {}", lit.value(), d.message()),
            help,
        )
    })?;
    if d.is_zero() {
        return Err(error_at(
            &lit,
            "a TTL must be longer than zero",
            "write e.g. `30s`; to skip caching, do not cache",
        ));
    }
    let (secs, nanos) = (d.as_secs(), d.subsec_nanos());
    Ok(quote!(::core::time::Duration::new(#secs, #nanos)))
}

pub(crate) fn parts(input: TokenStream, ty: &str, name: &str) -> syn::Result<TokenStream> {
    let parts = Punctuated::<Expr, Token![,]>::parse_terminated.parse2(input.clone())?;
    let mut it = parts.iter();
    let Some(first) = it.next() else {
        return Err(error_at(
            &input,
            format!("`{name}!` needs at least one part"),
            format!("write `{name}!(\"projects\", user.id)`"),
        ));
    };
    for p in &parts {
        if let Expr::Lit(ExprLit { lit, .. }) = p {
            match lit {
                Lit::Str(s) if s.value().is_empty() => {
                    return Err(error_at(
                        s,
                        format!("`{name}!` parts cannot be empty"),
                        "name the part, e.g. `\"projects\"`",
                    ));
                }
                Lit::Str(s) if s.value().chars().any(char::is_control) => {
                    return Err(error_at(
                        s,
                        format!("`{name}!` parts cannot contain control characters"),
                        "use printable text",
                    ));
                }
                Lit::Str(_) | Lit::Int(_) | Lit::Bool(_) => {}
                other => {
                    return Err(error_at(
                        other,
                        format!("this literal cannot be part of a `{name}!`"),
                        "use a string, an integer or a boolean",
                    ));
                }
            }
        }
    }
    let ty = syn::Ident::new(ty, proc_macro2::Span::call_site());
    let rest = it.map(|p| quote!(.push(&(#p))));
    Ok(quote!(::orbic::__private::cache::#ty::new(&(#first)) #(#rest)*))
}
