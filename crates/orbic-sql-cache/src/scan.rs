//! Finding `sql!` invocations in source text (for `hyve-orbic codegen`) and
//! the invocation grammar the macro shares.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, LitStr, Token, Type};

/// `sql!(Model, "SQL", args...)` or `sql!("SQL", args...)`.
pub struct SqlInput {
    /// The model rows decode into.
    pub model: Option<Type>,
    /// The query.
    pub sql: LitStr,
    /// One argument per parameter.
    pub args: Vec<Expr>,
}

impl Parse for SqlInput {
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
        Ok(SqlInput { model, sql, args })
    }
}

/// A query found in source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The query text.
    pub sql: String,
    /// 1-based line of the invocation.
    pub line: usize,
}

/// Queries found, and `(line, error)` for invocations that do not parse.
pub type Scanned = (Vec<Found>, Vec<(usize, String)>);

/// Every `sql!` invocation in `source`, including inside other macros'
/// bodies; invocations that do not parse are returned as errors with their
/// line (the compiler reports them too).
pub fn scan(source: &str) -> Result<Scanned, String> {
    let tokens: TokenStream = source.parse().map_err(|e| format!("{e}"))?;
    let (mut found, mut bad) = (Vec::new(), Vec::new());
    walk(tokens, &mut found, &mut bad);
    Ok((found, bad))
}

fn walk(tokens: TokenStream, found: &mut Vec<Found>, bad: &mut Vec<(usize, String)>) {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    for (i, t) in trees.iter().enumerate() {
        if let TokenTree::Group(g) = t {
            let is_call = i >= 2
                && matches!(&trees[i - 1], TokenTree::Punct(p) if p.as_char() == '!')
                && matches!(&trees[i - 2], TokenTree::Ident(id) if id == "sql")
                && g.delimiter() != Delimiter::None;
            if is_call {
                let line = trees[i - 2].span().start().line;
                match syn::parse2::<SqlInput>(g.stream()) {
                    Ok(input) => found.push(Found {
                        sql: input.sql.value(),
                        line,
                    }),
                    // Inside a `macro_rules!` body the arguments are
                    // metavariables (`$x`); the query is still a literal.
                    Err(e) => match literal_query(g.stream()) {
                        Some(sql) => found.push(Found { sql, line }),
                        None => bad.push((line, e.to_string())),
                    },
                }
            } else {
                walk(g.stream(), found, bad);
            }
        }
    }
}

/// The string literal that is the first argument, or the second after a
/// model type.
fn literal_query(tokens: TokenStream) -> Option<String> {
    let mut before_comma = true;
    let mut position = 0;
    for t in tokens {
        match &t {
            TokenTree::Punct(p) if p.as_char() == ',' => {
                if !before_comma {
                    return None;
                }
                before_comma = false;
                position = 0;
                continue;
            }
            TokenTree::Literal(_) if position == 0 => {
                if let Ok(lit) = syn::parse2::<LitStr>(t.into()) {
                    return Some(lit.value());
                }
            }
            _ => {}
        }
        position += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_invocations_anywhere() {
        let src = r##"
            /// Not this: sql!("in a doc comment")
            fn f() {
                let q = orbic::sql!(Project, "SELECT * FROM projects WHERE id = $1", id);
                let r = sql!("UPDATE t SET a = $1", 1);
                macro_rules! m { ($x:expr) => { sql![Row, r#"SELECT "x" FROM t"#, $x] }; }
                let s = "sql!(\"in a string\")";
                let bad = sql!(42);
            }
        "##;
        let (found, bad) = scan(src).unwrap();
        let sql: Vec<_> = found.iter().map(|f| f.sql.as_str()).collect();
        assert_eq!(
            sql,
            [
                "SELECT * FROM projects WHERE id = $1",
                "UPDATE t SET a = $1",
                "SELECT \"x\" FROM t"
            ]
        );
        assert_eq!(found[0].line, 4);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].0, 8);
    }
}
