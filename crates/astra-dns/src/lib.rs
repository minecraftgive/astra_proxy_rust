//! Bounded DNS cache and address mappings, independent of platform and proxy I/O.
mod cache;
pub mod config;
mod maintenance;
mod mapping;
mod query_log;
mod resolver;
pub mod wire;
pub use config::{Config, Upstream};
pub use mapping::{LocalMappingStore, MappingStore};
pub use resolver::{Resolver, Transport};
