//! Persistent control service, independent of every frontend and proxy implementation.
pub use astra_management as protocol;
mod log_stream;
mod manager;
mod subscription_view;
mod tui_log;
pub use manager::{atomic_write, private_dir, serve, serve_with_config};
pub mod agent;
mod artifacts;
pub mod controller;
mod database;
mod save;
mod service;
pub mod store;
mod web_process;
pub use service::{run, run_config};
pub const UI_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
pub fn action_description(action: &protocol::Action) -> String {
    action.summary()
}
