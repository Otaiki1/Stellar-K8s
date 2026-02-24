//! REST API module for external integrations
//!
//! Provides an HTTP API for querying and managing StellarNodes.

mod custom_metrics;
mod dto;
mod handlers;
mod server;

pub use server::{build_tls_server_config, run_server};
