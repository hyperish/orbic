//! Procedural macros for `orbic`. Assembly is plain method chaining; macros
//! only cover what needs compile-time checks (`docs/01-dx-and-dsl.md` §1):
//!
//! - `#[table("name")]` with `#[pk]` on one field: table metadata and typed
//!   column constants, kept only in builds with the declaring crate's
//!   `server` feature.
//! - `sql!(Model, "...", args...)`: a query checked against the offline cache.
//! - `ttl!("30s")`: a `Duration` checked at compile time.
//! - `key!(...)`, `tag!(...)`: cache keys and tags whose parts cannot collide.

mod literals;
mod sql;
mod table;
mod util;

use proc_macro::TokenStream;

/// Table metadata for a `#[model]` struct: `#[table("projects")]`, with
/// `#[pk]` on the primary-key field. Generates `Table` and one column
/// constant per field (`Project::OWNER_ID`), only with the declaring crate's
/// `server` feature.
#[proc_macro_attribute]
pub fn table(args: TokenStream, item: TokenStream) -> TokenStream {
    table::expand(args.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// A query checked against the offline cache at compile time:
/// `sql!(Project, "SELECT * FROM projects WHERE id = $1", id)`, or
/// `sql!("UPDATE ...", args...)` for statements without rows. A query missing
/// from `.orbic/sql/` is a compile error telling you to run `hyve codegen`.
#[proc_macro]
pub fn sql(input: TokenStream) -> TokenStream {
    sql::expand(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// A time-to-live checked at compile time: `ttl!("30s")`, `ttl!("1h30m")`.
#[proc_macro]
pub fn ttl(input: TokenStream) -> TokenStream {
    literals::ttl(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// A cache key from parts: `key!("projects", user.id, cursor.after)`.
#[proc_macro]
pub fn key(input: TokenStream) -> TokenStream {
    literals::parts(input.into(), "CacheKey", "key")
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// A cache tag from parts: `tag!("projects", user.id)`.
#[proc_macro]
pub fn tag(input: TokenStream) -> TokenStream {
    literals::parts(input.into(), "Tag", "tag")
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
