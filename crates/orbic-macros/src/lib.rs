//! `orbic-macros` — Zero-dependency backend procedural macros (`#[table]`, `sql!`) for `orbic`.

use proc_macro::{Delimiter, Literal, Span, TokenStream, TokenTree};
use std::str::FromStr;

fn compile_err(span: Span, message: &str) -> TokenStream {
    let lit = Literal::string(message);
    let ts: TokenStream = format!("::core::compile_error!({lit});")
        .parse()
        .unwrap_or_default();
    ts.into_iter()
        .map(|mut tt| {
            tt.set_span(span);
            tt
        })
        .collect()
}

fn extract_leading_attrs(tokens: &[TokenTree]) -> (Vec<TokenStream>, usize) {
    let mut attrs = Vec::new();
    let mut idx = 0;
    while idx + 1 < tokens.len() {
        if let TokenTree::Punct(p) = &tokens[idx]
            && p.as_char() == '#'
            && let TokenTree::Group(g) = &tokens[idx + 1]
            && g.delimiter() == Delimiter::Bracket
        {
            attrs.push(g.stream());
            idx += 2;
            continue;
        }
        break;
    }
    (attrs, idx)
}

fn attr_head_ident(attr: &TokenStream) -> Option<String> {
    let first = attr.clone().into_iter().next()?;
    match first {
        TokenTree::Ident(id) => Some(id.to_string()),
        _ => None,
    }
}

struct TableField {
    attrs: Vec<TokenStream>,
    vis: TokenStream,
    name: String,
    ty: TokenStream,
    is_pk_attr: bool,
}

fn parse_fields(body: TokenStream) -> Result<Vec<TableField>, String> {
    let mut chunks: Vec<Vec<TokenTree>> = Vec::new();
    let mut cur = Vec::new();
    let mut angle = 0usize;
    for tt in body {
        match &tt {
            TokenTree::Punct(p) if p.as_char() == '<' => {
                angle += 1;
                cur.push(tt);
            }
            TokenTree::Punct(p) if p.as_char() == '>' => {
                angle = angle.saturating_sub(1);
                cur.push(tt);
            }
            TokenTree::Punct(p) if p.as_char() == ',' && angle == 0 => {
                if !cur.is_empty() {
                    chunks.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(tt),
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }

    let mut fields = Vec::new();
    for chunk in chunks {
        let (attrs, after) = extract_leading_attrs(&chunk);
        let mut kept = Vec::new();
        let mut is_pk_attr = false;
        for a in attrs {
            if attr_head_ident(&a).as_deref() == Some("pk") {
                is_pk_attr = true;
            } else {
                kept.push(a);
            }
        }
        let rest = &chunk[after..];
        let colon = rest
            .iter()
            .position(|tt| matches!(tt, TokenTree::Punct(p) if p.as_char() == ':'))
            .filter(|&c| c > 0)
            .ok_or_else(|| "expected `field: Type` inside #[table] struct".to_owned())?;
        let name = rest[colon - 1].to_string();
        let vis: TokenStream = rest[..colon - 1].iter().cloned().collect();
        let ty: TokenStream = rest[colon + 1..].iter().cloned().collect();
        fields.push(TableField {
            attrs: kept,
            vis,
            name,
            ty,
            is_pk_attr,
        });
    }
    Ok(fields)
}

/// Binds a `#[model]` struct directly to a SQL table (`#[table("projects")]`),
/// emitting typed column constants (`Project::OWNER_ID`) and `orbic::db::TableModel`.
#[proc_macro_attribute]
pub fn table(attr: TokenStream, item: TokenStream) -> TokenStream {
    let raw_attr = attr.to_string();
    let trimmed_attr = raw_attr.trim();
    if !(trimmed_attr.starts_with('"') && trimmed_attr.ends_with('"') && trimmed_attr.len() >= 3) {
        return compile_err(
            Span::call_site(),
            "#[table(\"table_name\")] requires a string literal table name",
        );
    }
    let table_name = &trimmed_attr[1..trimmed_attr.len() - 1];

    let tokens: Vec<TokenTree> = item.into_iter().collect();
    let (outer_attrs, after_attrs) = extract_leading_attrs(&tokens);
    let mut kept_outer = String::new();
    for a in outer_attrs {
        kept_outer.push_str("#[");
        kept_outer.push_str(&a.to_string());
        kept_outer.push_str("]\n");
    }

    let rest = &tokens[after_attrs..];
    let Some(struct_pos) = rest
        .iter()
        .position(|tt| matches!(tt, TokenTree::Ident(id) if id.to_string() == "struct"))
    else {
        return compile_err(Span::call_site(), "#[table] must decorate a struct");
    };
    let vis: String = rest[..struct_pos]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let Some(TokenTree::Ident(struct_ident)) = rest.get(struct_pos + 1) else {
        return compile_err(Span::call_site(), "expected struct name after `struct`");
    };
    let struct_name = struct_ident.to_string();
    let Some(TokenTree::Group(body_group)) = rest.get(struct_pos + 2) else {
        return compile_err(Span::call_site(), "#[table] requires a braced struct");
    };
    let fields = match parse_fields(body_group.stream()) {
        Ok(f) => f,
        Err(msg) => return compile_err(Span::call_site(), &msg),
    };

    let mut struct_body = String::new();
    let mut col_consts = String::new();
    let mut to_row_entries = String::new();
    let mut from_row_entries = String::new();
    let mut has_version = false;
    let mut pk_field = fields
        .iter()
        .find(|f| f.is_pk_attr)
        .map(|f| f.name.clone())
        .or_else(|| {
            fields
                .iter()
                .find(|f| f.name == "id")
                .map(|f| f.name.clone())
        })
        .unwrap_or_else(|| {
            fields
                .first()
                .map(|f| f.name.clone())
                .unwrap_or_else(|| "id".to_owned())
        });

    for f in &fields {
        if f.is_pk_attr {
            pk_field = f.name.clone();
        }
        if f.name == "version" {
            has_version = true;
        }
        for a in &f.attrs {
            struct_body.push_str("    #[");
            struct_body.push_str(&a.to_string());
            struct_body.push_str("]\n");
        }
        struct_body.push_str(&format!("    {} {}: {},\n", f.vis, f.name, f.ty));

        let fname = &f.name;
        let fupper = fname.to_ascii_uppercase();
        let flit = Literal::string(fname).to_string();
        let fty = &f.ty;
        col_consts.push_str(&format!(
            "    #[allow(missing_docs)]\n\
             \x20   pub const {fupper}: ::orbic::db::Column<Self, {fty}> = ::orbic::db::Column::new({flit});\n"
        ));
        to_row_entries.push_str(&format!(
            "        rows.push(({flit}, ::orbic::db::IntoSqlParam::into_sql_param(self.{fname}.clone())));\n"
        ));
        from_row_entries.push_str(&format!(
            "            {fname}: ::orbic::db::FromSqlParam::from_sql_row(row, {flit})?,\n"
        ));
    }

    let table_lit = Literal::string(table_name).to_string();
    let pk_lit = Literal::string(&pk_field).to_string();
    let version_impl = if has_version {
        "    fn version_of(&self) -> Option<i64> { Some(self.version) }\n\
         \x20   fn set_version(&mut self, v: i64) { self.version = v; }\n"
    } else {
        ""
    };

    let expanded = format!(
        "{kept_outer}{vis} struct {struct_name} {{\n{struct_body}}}\n\n\
         impl {struct_name} {{\n{col_consts}}}\n\n\
         impl ::orbic::db::TableModel for {struct_name} {{\n\
             fn table_name() -> &'static str {{ {table_lit} }}\n\
             fn pk_field() -> &'static str {{\n\
                 <Self as ::hyve::validate::HasSchema>::schema()\n\
                     .fields\n\
                     .iter()\n\
                     .find(|f| f.is_pk)\n\
                     .map(|f| f.name)\n\
                     .unwrap_or({pk_lit})\n\
             }}\n\
             fn pk_value(&self) -> String {{\n\
                 self.to_sql_row()\n\
                     .into_iter()\n\
                     .find(|(k, _)| *k == Self::pk_field())\n\
                     .map(|(_, v)| v.as_key_string())\n\
                     .unwrap_or_default()\n\
             }}\n\
         {version_impl}\
             fn to_sql_row(&self) -> ::std::vec::Vec<(&'static str, ::orbic::db::SqlParam)> {{\n\
                 let mut rows = ::std::vec::Vec::new();\n\
         {to_row_entries}\
                 rows\n\
             }}\n\
             fn from_sql_row(row: &[(&'static str, ::orbic::db::SqlParam)]) -> ::orbic::OrbicResult<Self> {{\n\
                 Ok(Self {{\n\
         {from_row_entries}\
                 }})\n\
             }}\n\
         }}\n"
    );

    TokenStream::from_str(&expanded).unwrap_or_else(|e| {
        compile_err(
            Span::call_site(),
            &format!("internal #[table] expansion error: {e}"),
        )
    })
}

fn split_comma_args(stream: TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut angle = 0usize;
    for tt in stream {
        match &tt {
            TokenTree::Punct(p) if p.as_char() == '<' => {
                angle += 1;
                cur.push(tt);
            }
            TokenTree::Punct(p) if p.as_char() == '>' => {
                angle = angle.saturating_sub(1);
                cur.push(tt);
            }
            TokenTree::Punct(p) if p.as_char() == ',' && angle == 0 => {
                let s: TokenStream = cur.drain(..).collect();
                let txt = s.to_string();
                if !txt.trim().is_empty() {
                    out.push(txt);
                }
            }
            _ => cur.push(tt),
        }
    }
    let s: TokenStream = cur.into_iter().collect();
    let txt = s.to_string();
    if !txt.trim().is_empty() {
        out.push(txt);
    }
    out
}

fn unquote_rust_str(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if !(t.starts_with('"') && t.ends_with('"') && t.len() >= 2) {
        return Err("expected string literal SQL query".to_owned());
    }
    let mut out = String::new();
    let mut chars = t[1..t.len() - 1].chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => break,
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn max_placeholder_index(sql: &str) -> usize {
    let mut max_idx = 0usize;
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start
                && let Ok(n) = sql[start..end].parse::<usize>()
            {
                max_idx = max_idx.max(n);
            }
            i = end;
        } else {
            i += 1;
        }
    }
    max_idx
}

/// Maps `$N` placeholders in `INSERT INTO table (c1, c2, ...) VALUES (v1, v2, ...)` to column names.
fn extract_insert_column_map(sql: &str) -> Vec<(usize, String)> {
    let upper = sql.to_ascii_uppercase();
    if !upper.trim_start().starts_with("INSERT INTO") {
        return Vec::new();
    }
    let Some(lp1) = sql.find('(') else {
        return Vec::new();
    };
    let Some(rp1) = sql[lp1 + 1..].find(')').map(|x| lp1 + 1 + x) else {
        return Vec::new();
    };
    let cols: Vec<&str> = sql[lp1 + 1..rp1].split(',').map(str::trim).collect();
    let rest = &sql[rp1 + 1..];
    let Some(lp2) = rest.find('(') else {
        return Vec::new();
    };
    let Some(rp2) = rest[lp2 + 1..].find(')').map(|x| lp2 + 1 + x) else {
        return Vec::new();
    };
    let vals: Vec<&str> = rest[lp2 + 1..rp2].split(',').map(str::trim).collect();

    let mut pairs = Vec::new();
    for (col, val) in cols.into_iter().zip(vals) {
        if let Some(num_str) = val.strip_prefix('$')
            && let Ok(idx) = num_str.trim().parse::<usize>()
            && idx >= 1
        {
            pairs.push((idx - 1, col.to_owned()));
        }
    }
    pairs
}

/// Compile-time validated SQL query builder (`sql!(Model, "INSERT INTO ...", args...)`).
#[proc_macro]
pub fn sql(input: TokenStream) -> TokenStream {
    let parts = split_comma_args(input);
    if parts.len() < 2 {
        return compile_err(
            Span::call_site(),
            "sql!(Model, \"QUERY\", args...) requires at least Model type and SQL string literal",
        );
    }
    let model_ty = parts[0].trim();
    let sql_text = match unquote_rust_str(&parts[1]) {
        Ok(s) => s,
        Err(msg) => return compile_err(Span::call_site(), &msg),
    };
    let args = &parts[2..];

    let trimmed_upper = sql_text.trim_start().to_ascii_uppercase();
    if !(trimmed_upper.starts_with("SELECT ")
        || trimmed_upper.starts_with("INSERT ")
        || trimmed_upper.starts_with("UPDATE ")
        || trimmed_upper.starts_with("DELETE "))
    {
        return compile_err(
            Span::call_site(),
            "sql! only accepts SELECT, INSERT, UPDATE, or DELETE statements",
        );
    }

    let expected_placeholders = max_placeholder_index(&sql_text);
    if expected_placeholders != args.len() {
        return compile_err(
            Span::call_site(),
            &format!(
                "sql! placeholder count mismatch: query uses $1..=${expected_placeholders}, but {} argument(s) provided",
                args.len()
            ),
        );
    }

    let col_map = extract_insert_column_map(&sql_text);
    let mut param_exprs = Vec::with_capacity(args.len());
    for (idx, arg) in args.iter().enumerate() {
        if let Some((_, col_name)) = col_map.iter().find(|(p_idx, _)| *p_idx == idx) {
            let col_upper = col_name.to_ascii_uppercase();
            param_exprs.push(format!(
                "::orbic::db::bind_column(&{model_ty}::{col_upper}, {arg})"
            ));
        } else {
            param_exprs.push(format!("::orbic::db::IntoSqlParam::into_sql_param({arg})"));
        }
    }

    let sql_lit = Literal::string(&sql_text).to_string();
    let params_vec = param_exprs.join(", ");
    let expanded =
        format!("::orbic::db::SqlQuery::<{model_ty}>::new({sql_lit}, vec![{params_vec}])");

    TokenStream::from_str(&expanded)
        .unwrap_or_else(|e| compile_err(Span::call_site(), &format!("internal sql! error: {e}")))
}
