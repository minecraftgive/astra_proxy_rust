//! Management contracts and clients. No daemon or UI implementation belongs here.
mod wire;
pub use wire::*;
pub mod client;
pub mod config;
mod update_json;
pub mod v2;
pub use client::ManagerClient;

pub mod server;

pub mod host_control;
