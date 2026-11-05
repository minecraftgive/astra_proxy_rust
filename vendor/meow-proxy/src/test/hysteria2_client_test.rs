use super::*;

#[test]
fn parses_domain_server_target() {
    let target = ServerTarget::parse("example.com:443").unwrap();
    assert_eq!(target.host, "example.com");
    assert_eq!(target.port, 443);
}

#[test]
fn parses_bracketed_ipv6_server_target() {
    let target = ServerTarget::parse("[::1]:443").unwrap();
    assert_eq!(target.host, "::1");
    assert_eq!(target.port, 443);
}

#[test]
fn remember_prefers_errno_over_context_only_errors() {
    let emfile = Error::Io(std::io::Error::from_raw_os_error(1));
    let timeout_err = Error::Quic("connect timeout".into());
    // errno first, timeout second: errno survives.
    let acc = remember(Some(emfile), timeout_err);
    assert!(matches!(acc, Some(Error::Io(_))));
    // timeout first, errno second: errno replaces it.
    let acc = remember(
        Some(Error::Quic("connect timeout".into())),
        Error::Io(std::io::Error::from_raw_os_error(1)),
    );
    assert!(matches!(acc, Some(Error::Io(_))));
    // Two context-only errors: the last one wins (historical shape).
    let acc = remember(
        Some(Error::Quic("first".into())),
        Error::Quic("second".into()),
    );
    assert!(matches!(acc, Some(Error::Quic(ref s)) if s == "second"));
    // An io error without errno does not gain precedence — neither
    // as `next` (last-wins takes it, same as any context error)…
    let acc = remember(
        Some(Error::Quic("first".into())),
        Error::Io(std::io::Error::other("ctx")),
    );
    assert!(matches!(acc, Some(Error::Io(_))));
    // …nor as `prev`: `driver.rs` synthesizes errno-less io errors
    // ("quiche send: {e}"), and they must not veto a later candidate
    // — this is the discriminating direction.
    let acc = remember(
        Some(Error::Io(std::io::Error::other("ctx"))),
        Error::Quic("second".into()),
    );
    assert!(matches!(acc, Some(Error::Quic(ref s)) if s == "second"));
}
