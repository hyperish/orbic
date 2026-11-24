//! `sql!(Model, "SQL", args...)` and `sql!("SQL", args...)`, checked against
//! the offline cache (`docs/01-dx-and-dsl.md` §4). The macro reads files only;
//! it never connects to a database.

use std::path::{Path, PathBuf};

use orbic_sql_cache::{CACHE_DIR, CachedQuery, SqlInput as Input, file_name, find_root};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::util::{error, error_at};

const RUN_CODEGEN: &str =
    "run `hyve codegen` to analyze the query and commit the files it writes to `.orbic/sql/`";

pub(crate) fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_default();
    expand_in(input, &manifest)
}

fn marker(ty: Option<&str>) -> TokenStream {
    let db = quote!(::orbic::__private::db);
    let name = match ty {
        None => return quote!(_),
        Some("bool") => "Bool",
        Some("int") => "Int",
        Some("float") => "Float",
        Some("text") => "Text",
        Some("bytes") => "Bytes",
        Some(_) => "Timestamp",
    };
    let name = format_ident!("{}", name);
    quote!(#db::types::#name)
}

fn expand_in(input: TokenStream, manifest: &Path) -> syn::Result<TokenStream> {
    let usage = "write `sql!(Project, \"SELECT * FROM projects WHERE id = $1\", id)`, or `sql!(\"UPDATE ...\", args...)` for statements without rows";
    let Input { model, sql, args } = syn::parse2(input).map_err(|e| {
        error(
            e.span(),
            "`sql!` takes an optional model type, the query as a string literal, then one argument per parameter",
            usage,
        )
    })?;
    let text = sql.value();
    let db = quote!(::orbic::__private::db);
    let Some(root) = find_root(manifest) else {
        return Err(error_at(
            &sql,
            format!("no `{CACHE_DIR}` cache at or above {}", manifest.display()),
            RUN_CODEGEN,
        ));
    };
    let path = root.join(CACHE_DIR).join(file_name(&text));
    let Ok(file) = std::fs::read_to_string(&path) else {
        return Err(error_at(
            &sql,
            format!(
                "this query is not in the offline cache ({CACHE_DIR}/{})",
                file_name(&text)
            ),
            RUN_CODEGEN,
        ));
    };
    let cached = CachedQuery::parse(&file).map_err(|why| {
        error_at(
            &sql,
            format!("{} is not a valid cache file: {why}", path.display()),
            RUN_CODEGEN,
        )
    })?;
    if cached.sql != text {
        return Err(error_at(
            &sql,
            format!(
                "{} holds a different query (hash collision)",
                path.display()
            ),
            RUN_CODEGEN,
        ));
    }
    if args.len() != cached.params.len() {
        return Err(error_at(
            &sql,
            format!(
                "the query takes {} parameter(s), but {} argument(s) were given",
                cached.params.len(),
                args.len()
            ),
            "pass one argument per `$n` placeholder, in order",
        ));
    }

    // A known parameter type binds as that type; an unknown one (SQLite does
    // not type parameters) takes the type the argument's Rust type implies.
    let binds = args.iter().zip(&cached.params).map(|(a, p)| {
        let m = marker(p.ty.as_deref());
        quote!(#db::bind::<#m, _>(&(#a)))
    });
    let names: Vec<&str> = cached.columns.iter().map(|c| c.name.as_str()).collect();
    let dialects = &cached.dialects;
    let checks = match &model {
        None if !cached.columns.is_empty() => {
            return Err(error_at(
                &sql,
                format!(
                    "the query returns columns ({}); name the model rows decode into",
                    names.join(", ")
                ),
                "write `sql!(Model, \"...\", ...)`",
            ));
        }
        None => quote!(),
        Some(m) => {
            let model_name = quote!(#m).to_string().replace(' ', "");
            let per_column = cached.columns.iter().map(|c| {
                let n = &c.name;
                let ty = match c.ty.as_deref() {
                    None => quote!(::core::option::Option::None),
                    Some(t) => {
                        let m = marker(Some(t));
                        quote!(::core::option::Option::Some(<#m as #db::Marker>::TYPE))
                    }
                };
                let nullable = match c.nullable {
                    None => quote!(::core::option::Option::None),
                    Some(b) => quote!(::core::option::Option::Some(#b)),
                };
                let shown = c.ty.as_deref().unwrap_or("unknown");
                let missing = format!("`sql!`: the query returns column `{n}`, which `{model_name}` does not have");
                let wrong = format!(
                    "`sql!`: column `{n}` is `{shown}` in the query, but the type of `{model_name}.{n}` is not stored as `{shown}`"
                );
                let null = format!("`sql!`: column `{n}` may be NULL, but `{model_name}.{n}` is not an `Option`");
                quote!(const _: () = #db::assert_column::<#m>(#n, #ty, #nullable, #missing, #wrong, #null);)
            });
            let all = format!(
                "`sql!`: the query does not return every column of `{model_name}`; select them all (e.g. `RETURNING *`)"
            );
            quote! {
                #(#per_column)*
                const _: () = #db::assert_all_columns::<#m>(&[#(#names),*], #all);
            }
        }
    };
    let model = model.map_or_else(|| quote!(()), |m| quote!(#m));
    let path = path.to_string_lossy().into_owned();
    Ok(quote! {{
        // Recompiles the query when its cache file changes.
        const _: &[u8] = ::core::include_bytes!(#path);
        #checks
        #db::Sql::<#model>::new(#text, &[#(#dialects),*], &[#(#names),*], ::std::vec![#(#binds),*])
    }})
}

#[cfg(test)]
mod tests {
    use orbic_sql_cache::{Column, Param};

    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbic-macros-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn query(sql: &str) -> CachedQuery {
        CachedQuery {
            sql: sql.into(),
            params: vec![
                Param {
                    ty: Some("int".into()),
                },
                Param { ty: None },
            ],
            columns: vec![Column {
                name: "n".into(),
                ty: Some("int".into()),
                nullable: None,
            }],
            dialects: vec!["sqlite".into()],
        }
    }

    fn message(r: syn::Result<TokenStream>) -> String {
        r.expect_err("expected an error").to_string()
    }

    #[test]
    fn reads_the_nearest_cache_above_the_crate() {
        let root = temp("read");
        query("SELECT $1 + $2 AS n").write(&root).unwrap();
        let out = expand_in(
            quote!(Row, "SELECT $1 + $2 AS n", 1, x),
            &root.join("crates/app"),
        )
        .unwrap()
        .to_string();
        assert!(
            out.contains("include_bytes") && out.contains("assert_column"),
            "{out}"
        );
        assert!(
            out.contains("bind :: < :: orbic :: __private :: db :: types :: Int , _ >"),
            "{out}"
        );
        assert!(
            out.contains("bind :: < _ , _ >"),
            "unknown types are inferred: {out}"
        );

        // A file under the query's name that holds another query.
        let other = query("SELECT $1 AS m");
        let path = root.join(CACHE_DIR).join(file_name("SELECT 2"));
        std::fs::write(&path, other.to_json()).unwrap();
        assert!(message(expand_in(quote!("SELECT 2"), &root)).contains("different query"));
        std::fs::write(&path, "{").unwrap();
        assert!(message(expand_in(quote!("SELECT 2"), &root)).contains("not a valid cache file"));
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            message(expand_in(quote!("SELECT 1"), &temp("none"))).contains("no `.orbic/sql` cache")
        );
    }
}
