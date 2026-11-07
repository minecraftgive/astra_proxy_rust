//! Native views emit shared semantic operations to their owning window.
pub mod connections;
pub mod debug;
pub mod diagnostics;
pub mod editor;
pub mod elevation;
pub mod help;
pub mod installation;
pub mod logs;
pub mod marquee;
pub mod monitor;
pub mod nettools;
pub mod overview;
pub mod palette;
pub mod profiles;
pub mod proxies;
pub mod recovery;
pub mod rules;
pub mod server;
pub mod settings;
pub mod tasks;
pub mod updates;
#[derive(Clone)]
pub enum ViewEvent {
    Input(astra_client::state::Input),
    Dismiss,
    Cancel(u64),
    Navigate(usize),
    Panel(crate::pages::Panel),
}
