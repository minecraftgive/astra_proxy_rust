//! Shared document and schema engine. Terminal, transport, and process-free.
mod document;
pub mod edit;
pub mod history;
pub mod import;
pub mod schema;
pub use document::Document;
