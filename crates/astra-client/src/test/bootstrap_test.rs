use super::*;
fn parse(args: &[&str]) -> Result<Options> {
    Options::parse(None, args.iter().map(|s| (*s).to_owned()))
}
#[test]
fn desktop_and_terminal_share_launch_options() {
    let options = parse(&[
        "--core",
        "server",
        "--view",
        "profiles",
        "--instance",
        "one",
        "--endpoint",
        "/tmp/test.sock",
        "--agent",
        "remote",
    ])
    .unwrap();
    assert_eq!(options.launch.kind, Some(protocol::Kind::Server));
    assert_eq!(options.launch.view, Some(windows::View::Profiles));
    assert_eq!(options.launch.instance.as_deref(), Some("one"));
    assert_eq!(options.agent_id.as_deref(), Some("remote"));
}
#[test]
fn conflicting_maintenance_commands_are_rejected() {
    assert!(parse(&["update-status", "rollback"]).is_err());
    assert!(parse(&["--shutdown", "rollback"]).is_err());
    assert!(parse(&["--silent", "--view", "logs"]).is_err());
    assert!(parse(&["--restart-ready", "/tmp/ready"]).is_err());
}
#[test]
fn server_default_is_a_primary_profiles_window() {
    let options = Options::parse(Some(protocol::Kind::Server), Vec::<String>::new()).unwrap();
    assert_eq!(options.launch.view, Some(windows::View::Profiles));
    assert!(options.launch.window_id.is_none());
}
