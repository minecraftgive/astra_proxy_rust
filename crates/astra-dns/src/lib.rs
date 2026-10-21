//! Bounded DNS cache and address mappings, independent of platform and proxy I/O.
pub mod config;
mod mapping;
mod resolver;
pub mod wire;
pub use config::{Config, Upstream};
pub use resolver::{Resolver, Transport};
