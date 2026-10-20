//! UI capabilities; hidden configuration is preserved verbatim when saving.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Macos,
    Linux,
    Other,
}
impl Platform {
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
    pub fn option(self, parent: Option<&str>, key: &str) -> bool {
        match (parent, key) {
            (None, "tproxy" | "redirect") => self == Self::Linux,
            (None, "tun" | "system_proxy") => self != Self::Other,
            (Some("system_proxy"), "services") => self == Self::Macos,
            _ => true,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platform_capabilities() {
        for platform in [
            Platform::Windows,
            Platform::Macos,
            Platform::Linux,
            Platform::Other,
        ] {
            assert_eq!(platform.option(None, "tproxy"), platform == Platform::Linux);
            assert_eq!(
                platform.option(None, "redirect"),
                platform == Platform::Linux
            );
            assert_eq!(
                platform.option(Some("system_proxy"), "services"),
                platform == Platform::Macos
            );
            assert!(platform.option(None, "outbounds"));
            assert!(platform.option(Some("transport"), "headers"));
        }
    }
}
