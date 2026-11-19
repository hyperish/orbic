//! `orbic` — Component-based Rust backend framework for the Hyperish family.
//!
//! Unopinionated `BackendComponent` assembly built on `hyve` (`ref`) with
//! dual-track DB queries (`#[table]` + `sql!`), L1/L2 cache with Singleflight
//! and tag invalidation, L2-backed Opaque sessions, Dual-Rail transport,
//! OpenAPI `/docs`, realtime rooms, and portless `TestClient`.

extern crate self as orbic;

pub mod auth;
pub mod cache;
pub mod core;
pub mod db;
pub mod openapi;
pub mod realtime;

pub use hyve::HyveResult as OrbicResult;
pub use orbic_macros::{sql, table};

/// Commonly used backend types, macros, and builders (`use orbic::prelude::*;`).
pub mod prelude {
    pub use crate::OrbicResult;
    pub use crate::auth::{AuthGuard, CurrentUser, SessionStore, SessionTokens, User, test_user};
    pub use crate::cache::{Cache, CacheKey, CacheOpts, CacheTag};
    pub use crate::core::{
        BackendComponent, CursorParams, HttpMethod, OrbicApp, Query, RateLimit, RouteSpec,
        TestClient, TestResponse, component,
    };
    pub use crate::db::{
        Column, Db, Page, PageMeta, SqlExecutor, SqlParam, SqlQuery, TableModel, Tx,
    };
    pub use crate::realtime::{ClientFrame, RealtimeHub, RealtimeTransport, ServerFrame};
    pub use crate::{key, sql, table, tag, ttl};
}
