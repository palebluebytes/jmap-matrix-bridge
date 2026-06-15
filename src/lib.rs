#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::unnecessary_fallible_conversions,
    clippy::multiple_crate_versions,
    clippy::cargo_common_metadata
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::str_to_string,
        clippy::too_many_lines,
        clippy::unreadable_literal,
        clippy::uninlined_format_args
    )
)]

pub mod client_manager;
pub mod commands;
pub mod config;
pub mod crypto;
pub mod ghost;
pub mod ingest;
pub mod matrix;
pub mod puppet;
pub mod retry;
pub mod routes;
pub mod sender;
pub mod services;
pub mod state;
pub mod store;
pub mod sync;

// Re-export core types for convenience
pub use routes::AppState;
