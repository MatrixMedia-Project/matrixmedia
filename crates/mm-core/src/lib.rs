pub mod auth;
pub mod cache;
pub mod config;
pub mod e2ee;
pub mod error;
pub mod federation;
pub mod http;
pub mod media;
pub mod metrics;
/// Process-wide collectors for sites that cannot reach `AppState`.
pub mod metrics_global;
pub mod permissions;
pub mod types;
pub mod switch_auth;
pub mod switch_client;
pub mod turn_auth;
pub mod synapse_admin;
pub mod validation;
