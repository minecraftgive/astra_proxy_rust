#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
fn main() -> anyhow::Result<()> {
    astra_gui::run(astra_client::protocol::Kind::Server)
}
