//! `#[table("name")]` and `#[pk]`.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{Fields, ItemStruct, LitStr};

use crate::util::{error, error_at};

pub(crate) fn expand(args: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let fix = "write `#[table(\"projects\")]` above a `#[model]` struct, with `#[pk]` on its primary-key field";
    let name: LitStr = syn::parse2(args.clone()).map_err(|_| {
        error(
            Span::call_site(),
            "`#[table]` takes the table name as a string literal",
            fix,
        )
    })?;
    let table = name.value();
    if table.is_empty()
        || !table
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(error_at(
            &name,
            format!("invalid table name {table:?}"),
            "use lower-case `snake_case`, plural (`projects`), per hyperish/docs 06-naming-conventions.md",
        ));
    }
    let mut s: ItemStruct = syn::parse2(item).map_err(|e| {
        error(
            e.span(),
            "`#[table]` goes on a struct with named fields",
            fix,
        )
    })?;
    if !s.generics.params.is_empty() {
        return Err(error_at(
            &s.generics,
            "`#[table]` structs cannot be generic",
            "store one concrete model per table",
        ));
    }
    let Fields::Named(named) = &mut s.fields else {
        return Err(error_at(
            &s.fields,
            "`#[table]` structs need named fields",
            fix,
        ));
    };

    let mut pk = None;
    let mut columns = Vec::new();
    for f in named.named.iter_mut() {
        let ident = f.ident.clone().expect("named field");
        let before = f.attrs.len();
        f.attrs.retain(|a| !a.path().is_ident("pk"));
        if f.attrs.len() < before {
            if let Some((prev, _)) = &pk {
                return Err(error_at(
                    &ident,
                    format!("`#[pk]` is on both `{prev}` and `{ident}`"),
                    "Phase 1 tables have a single-column primary key; mark one field",
                ));
            }
            pk = Some((ident.clone(), f.ty.clone()));
        }
        columns.push((ident, f.ty.clone()));
    }
    let Some((pk, _)) = pk else {
        return Err(error(
            Span::call_site(),
            format!("table `{table}` has no `#[pk]` field"),
            "mark the primary key: `#[pk] pub id: Id<Project>`",
        ));
    };

    let ty = &s.ident;
    let db = quote!(::orbic::__private::db);
    let column_name = |i: &syn::Ident| {
        let n = i.to_string();
        n.strip_prefix("r#").unwrap_or(&n).to_owned()
    };
    let names: Vec<String> = columns.iter().map(|(i, _)| column_name(i)).collect();
    let consts = columns.iter().zip(&names).map(|((i, t), n)| {
        let c = format_ident!("{}", n.to_ascii_uppercase(), span = i.span());
        let doc = format!("The `{n}` column.");
        quote!(#[doc = #doc] pub const #c: #db::Column<#ty, #t> = #db::Column::new(#n);)
    });
    let types = columns
        .iter()
        .map(|(_, t)| quote!((<#t as #db::SqlValue>::TYPE, <#t as #db::SqlValue>::NULLABLE)));
    let values = columns
        .iter()
        .map(|(i, _)| quote!(#db::SqlValue::to_db(&self.#i)));
    let fields = columns
        .iter()
        .zip(&names)
        .map(|((i, t), n)| quote!(#i: #db::column_value::<#t>(&mut __values, #table, #n)?));
    let pk_name = column_name(&pk);

    Ok(quote! {
        #s

        // `server` is the declaring crate's feature (see `#[server]`).
        #[allow(unexpected_cfgs)]
        const _: () = {
            #[cfg(feature = "server")]
            const _: () = {
                impl #ty {
                    #(#consts)*
                }

                impl #db::Table for #ty {
                    const NAME: &'static str = #table;
                    const PK: &'static str = #pk_name;
                    const COLUMNS: &'static [&'static str] = &[#(#names),*];
                    const TYPES: &'static [(#db::SqlType, bool)] = &[#(#types),*];

                    fn values(&self) -> ::std::vec::Vec<#db::DbValue> {
                        ::std::vec![#(#values),*]
                    }

                    fn from_values(
                        __values: ::std::vec::Vec<#db::DbValue>,
                    ) -> ::core::result::Result<Self, ::orbic::__private::HyveError> {
                        let mut __values = __values.into_iter();
                        ::core::result::Result::Ok(#ty { #(#fields),* })
                    }
                }
            };
        };
    })
}
