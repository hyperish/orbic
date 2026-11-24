//! `sql!(Model, "SQL", args...)` and `sql!("SQL", args...)`, checked against
//! the offline cache (`docs/01-dx-and-dsl.md` §4). The macro reads files only;
//! it never connects to a database.

use std::path::{Path, PathBuf};

use orbic_sql_cache::{CACHE_DIR, CachedQuery, file_name, find_root};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, LitStr, Token, Type};

use crate::util::{error, error_at};

struct Input {
    model: Option<Type>,
    sql: LitStr,
    args: Vec<Expr>,
}

impl Parse for Input {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let model = if input.peek(LitStr) {
            None
        } else {
            let t: Type = input.parse()?;
            input.parse::<Token![,]>()?;
            Some(t)
        };
        let sql: LitStr = input.parse()?;
        let mut args = Vec::new();
        if input.parse::<Option<Token![,]>>()?.is_some() {
            args = Punctuated::<Expr, Token![,]>::parse_terminated(input)?
                .into_iter()
                .collect();
        }
        Ok(Input { model, sql, args })
    }
}

/// Set by `hyve-orbic codegen` while it drives a build to list the queries:
/// each `sql!` writes its text to `<dir>/<hash>.sql` instead of checking the
/// cache (`docs/01-dx-and-dsl.md` §4, 실행 주체).
const COLLECT_ENV: &str = "HYPERISH_ORBIC_SQL_COLLECT";

const RUN_CODEGEN: &str =
    "run `hyve codegen` to analyze the query and commit the files it writes to `.orbic/sql/`";

pub(crate) fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_default();
    let collect = std::env::var_os(COLLECT_ENV).map(PathBuf::from);
    expand_in(input, &manifest, collect.as_deref())
}

fn expand_in(
    input: TokenStream,
    manifest: &Path,
    collect: Option<&Path>,
) -> syn::Result<TokenStream> {
    let usage = "write `sql!(Project, \"SELECT * FROM projects WHERE id = $1\", id)`, or `sql!(\"UPDATE ...\", args...)` for statements without rows";
    let Input { model, sql, args } = syn::parse2(input.clone())
        .map_err(|e| {
            error(
                e.span(),
                "`sql!` takes an optional model type, the query as a string literal, then one argument per parameter",
                usage,
            )
        })?;
    let text = sql.value();
    let db = quote!(::orbic::__private::db);
    if let Some(dir) = collect {
        let file = dir.join(file_name(&text).replace(".json", ".sql"));
        std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(&file, &text))
            .map_err(|e| {
                error_at(
                    &sql,
                    format!("cannot record the query in {}: {e}", file.display()),
                    "check the directory `hyve-orbic codegen` passed",
                )
            })?;
        let model = model.map_or_else(|| quote!(()), |m| quote!(#m));
        // Unverified on every dialect, so it can never run.
        return Ok(quote! {{
            let _ = (#(&(#args),)*);
            #db::Sql::<#model>::new(#text, &[], &[], ::std::vec::Vec::new())
        }});
    }
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

    let ty_ident = |t: &str| {
        let name = match t {
            "bool" => "Bool",
            "int" => "Int",
            "float" => "Float",
            "text" => "Text",
            "bytes" => "Bytes",
            _ => "Timestamp",
        };
        format_ident!("{}", name)
    };
    let binds = args.iter().zip(&cached.params).map(|(a, p)| {
        let t = ty_ident(&p.ty);
        quote!(#db::bind::<#db::types::#t, _>(&(#a)))
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
                let (n, t, nullable) = (&c.name, ty_ident(&c.ty), c.nullable);
                let missing = format!("`sql!`: the query returns column `{n}`, which `{model_name}` does not have");
                let wrong = format!("`sql!`: column `{n}` is `{}` in the query, but the type of `{model_name}.{n}` is not stored as `{}`", c.ty, c.ty);
                let null = format!("`sql!`: column `{n}` may be NULL, but `{model_name}.{n}` is not an `Option`");
                quote!(const _: () = #db::assert_column::<#m>(#n, #db::SqlType::#t, #nullable, #missing, #wrong, #null);)
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
            params: vec![Param {
                ty: "int".into(),
                nullable: false,
            }],
            columns: vec![Column {
                name: "n".into(),
                ty: "int".into(),
                nullable: false,
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
        query("SELECT $1 AS n").write(&root).unwrap();
        let out = expand_in(
            quote!(Row, "SELECT $1 AS n", 1),
            &root.join("crates/app"),
            None,
        )
        .unwrap()
        .to_string();
        assert!(
            out.contains("include_bytes") && out.contains("assert_column"),
            "{out}"
        );
        assert!(out.contains("types :: Int"), "{out}");

        // A file under the query's name that holds another query.
        let other = query("SELECT $1 AS m");
        let path = root.join(CACHE_DIR).join(file_name("SELECT 2"));
        std::fs::write(&path, other.to_json()).unwrap();
        assert!(message(expand_in(quote!("SELECT 2"), &root, None)).contains("different query"));
        std::fs::write(&path, "{").unwrap();
        assert!(
            message(expand_in(quote!("SELECT 2"), &root, None)).contains("not a valid cache file")
        );
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            message(expand_in(quote!("SELECT 1"), &temp("none"), None))
                .contains("no `.orbic/sql` cache")
        );
    }

    #[test]
    fn collect_mode_lists_queries_without_checking() {
        let dir = temp("collect");
        let out = expand_in(
            quote!(Row, "SELECT $1 AS n", x),
            Path::new("/nowhere"),
            Some(&dir),
        )
        .unwrap()
        .to_string();
        let file = dir.join(file_name("SELECT $1 AS n").replace(".json", ".sql"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "SELECT $1 AS n");
        assert!(
            out.contains("& []") && !out.contains("include_bytes"),
            "never verified: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
