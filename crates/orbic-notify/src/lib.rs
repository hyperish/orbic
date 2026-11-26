//! Optional notification adapters. The application supplies credentials at
//! send time and chooses which provider to call.
mod email;
mod push;
mod webhook;
pub use email::{Email, Smtp};
pub use push::{Apns, HttpNotifications, PushType};
pub use webhook::{Delivery, DeliveryOutcome, Webhooks};
