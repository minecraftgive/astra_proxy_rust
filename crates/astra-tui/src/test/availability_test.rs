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
