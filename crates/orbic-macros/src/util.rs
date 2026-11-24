//! Diagnostics shared by the macros.

use hyve_diagnostics::Diagnostic;
use proc_macro2::Span;
use syn::spanned::Spanned;

/// An error that says what is wrong and how to fix it.
pub(crate) fn error(span: Span, what: impl Into<String>, fix: impl Into<String>) -> syn::Error {
    syn::Error::new(span, Diagnostic::error(what).with_help(fix).summary())
}

/// Like [`error`], spanning a syntax node.
pub(crate) fn error_at(
    node: &impl Spanned,
    what: impl Into<String>,
    fix: impl Into<String>,
) -> syn::Error {
    error(node.span(), what, fix)
}
