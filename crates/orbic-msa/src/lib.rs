//! Service-to-service transports and database-backed events.
//! The application provides the service endpoints and credentials.
mod events;
mod service;
pub use events::{EventBus, ServiceEvent};
pub use service::Service;
