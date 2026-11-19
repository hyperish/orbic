//! `orbic` — Component-based Rust backend framework for the Hyperish family.
//!
//! This initial pre-release establishes the unopinionated `BackendComponent`
//! and `OrbicApp` composition foundation described in `orbic/docs`.

/// Standard HTTP methods supported by `orbic` route specifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    /// HTTP GET.
    Get,
    /// HTTP POST.
    Post,
    /// HTTP PATCH.
    Patch,
    /// HTTP DELETE.
    Delete,
}

/// Declarative route entry collected inside a [`BackendComponent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    method: HttpMethod,
    path: String,
    handler_name: String,
}

impl RouteSpec {
    /// Returns the HTTP method for this route.
    #[must_use]
    pub fn method(&self) -> HttpMethod {
        self.method
    }

    /// Returns the route path pattern.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the registered handler identifier.
    #[must_use]
    pub fn handler_name(&self) -> &str {
        &self.handler_name
    }
}

/// Composable backend component bundling routes and nested components without
/// forcing a rigid folder architecture.
#[derive(Debug, Clone, Default)]
pub struct BackendComponent {
    routes: Vec<RouteSpec>,
}

impl BackendComponent {
    /// Creates an empty [`BackendComponent`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a `GET` route.
    #[must_use]
    pub fn get(self, path: impl Into<String>, handler_name: impl Into<String>) -> Self {
        self.route(HttpMethod::Get, path, handler_name)
    }

    /// Registers a `POST` route.
    #[must_use]
    pub fn post(self, path: impl Into<String>, handler_name: impl Into<String>) -> Self {
        self.route(HttpMethod::Post, path, handler_name)
    }

    /// Registers a `PATCH` route.
    #[must_use]
    pub fn patch(self, path: impl Into<String>, handler_name: impl Into<String>) -> Self {
        self.route(HttpMethod::Patch, path, handler_name)
    }

    /// Registers a `DELETE` route.
    #[must_use]
    pub fn delete(self, path: impl Into<String>, handler_name: impl Into<String>) -> Self {
        self.route(HttpMethod::Delete, path, handler_name)
    }

    /// Registers a route with an explicit [`HttpMethod`].
    #[must_use]
    pub fn route(
        mut self,
        method: HttpMethod,
        path: impl Into<String>,
        handler_name: impl Into<String>,
    ) -> Self {
        self.routes.push(RouteSpec {
            method,
            path: path.into(),
            handler_name: handler_name.into(),
        });
        self
    }

    /// Mounts a child component under the given path prefix.
    #[must_use]
    pub fn mount(mut self, prefix: &str, child: BackendComponent) -> Self {
        let base = prefix.trim_end_matches('/');
        for mut route in child.routes {
            let suffix = route.path.trim_start_matches('/');
            route.path = if suffix.is_empty() {
                if base.is_empty() {
                    "/".to_owned()
                } else {
                    base.to_owned()
                }
            } else {
                format!("{base}/{suffix}")
            };
            self.routes.push(route);
        }
        self
    }

    /// Returns all routes currently collected in this component.
    #[must_use]
    pub fn routes(&self) -> &[RouteSpec] {
        &self.routes
    }
}

/// Creates a fresh [`BackendComponent`] builder.
#[must_use]
pub fn component() -> BackendComponent {
    BackendComponent::new()
}

/// Root `orbic` backend application assembled from one or more [`BackendComponent`]s.
#[derive(Debug, Clone, Default)]
pub struct OrbicApp {
    root: BackendComponent,
    openapi_path: Option<String>,
}

impl OrbicApp {
    /// Creates a new empty [`OrbicApp`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables OpenAPI documentation serving at the specified path.
    #[must_use]
    pub fn openapi(mut self, path: impl Into<String>) -> Self {
        self.openapi_path = Some(path.into());
        self
    }

    /// Mounts a [`BackendComponent`] under the specified prefix.
    #[must_use]
    pub fn mount(mut self, prefix: &str, comp: BackendComponent) -> Self {
        self.root = self.root.mount(prefix, comp);
        self
    }

    /// Returns the mounted routes.
    #[must_use]
    pub fn routes(&self) -> &[RouteSpec] {
        self.root.routes()
    }

    /// Returns the configured OpenAPI docs path, if enabled.
    #[must_use]
    pub fn openapi_path(&self) -> Option<&str> {
        self.openapi_path.as_deref()
    }
}

/// Commonly used types re-exported for ergonomic imports.
pub mod prelude {
    pub use crate::{BackendComponent, HttpMethod, OrbicApp, RouteSpec, component};
}

#[cfg(test)]
mod tests {
    use super::prelude::*;

    #[test]
    fn app_mounts_nested_components_and_openapi() {
        let projects = component()
            .get("/projects", "list_projects")
            .post("/projects", "create_project");

        let app = OrbicApp::new().openapi("/docs").mount("/v1", projects);

        assert_eq!(app.openapi_path(), Some("/docs"));
        assert_eq!(app.routes().len(), 2);
        assert_eq!(app.routes()[0].path(), "/v1/projects");
        assert_eq!(app.routes()[0].method(), HttpMethod::Get);
        assert_eq!(app.routes()[1].method(), HttpMethod::Post);
    }
}
