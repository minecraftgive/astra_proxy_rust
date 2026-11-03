//! Private per-profile runtime state persistence.
use astra_model::runtime::Overrides;
/// Load only private runtime state; source profiles never supply mutable switches.
pub fn load_overrides(profile: &std::path::Path) -> std::io::Result<Overrides> {
    load_overrides_in(&crate::storage::runtime_root()?, profile)
}
pub fn load_overrides_in(
    root: &std::path::Path,
    profile: &std::path::Path,
) -> std::io::Result<Overrides> {
    use std::io::Read;
    let path = crate::storage::runtime_path_in(root, profile, "overrides")?;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Overrides::default())
        }
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(std::io::Error::other("runtime overrides exceed 1 MiB"));
    }
    let mut state: Overrides = serde_json::from_slice(&bytes)?;
    state.system_proxy.get_or_insert(false);
    state.tun.get_or_insert(false);
    state.allow_lan.get_or_insert(false);
    Ok(state)
}
