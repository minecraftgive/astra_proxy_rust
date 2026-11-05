use super::*;

const CONFIG: &str = r#"mixed_listen = "127.0.0.1:7890"

[[outbounds]]
kind = "vless_tls"
tag = "secret-node"
password = "must-not-be-in-shell"

[[outbounds]]
kind = "socks5"
tag = "other"
server = "127.0.0.1:1"

[[rules]]
kind = "rule_set"
value = "ads"
action = "block"

[[rules]]
kind = "domain_suffix"
value = "example.com"
action = "direct"

[rule_sets.ads]
behavior = "domain"
format = "yaml"
payload = ["+.ads.example"]
"#;

const NODES: &str = r#"mixed_listen = "127.0.0.1:7890"
[[outbounds]]
kind = "socks5"
tag = "香港 first"
server = "proxy.example:1080"
username = "user"
password = "pass word"

[[outbounds]]
kind = "http"
tag = "unselected"
server = "unrelated.example:8080"

[[outbounds]]
kind = "socks5"
tag = "second"
server = "[::1]:1081"
"#;

#[test]
fn full_export_preserves_native_text() {
    let source = "# comment\nstandalone = true\n";
    assert_eq!(full_config(source).unwrap(), source);
    assert!(full_config(&"x".repeat(MAX_EXPORT_BYTES + 1)).is_err());
}

#[test]
fn selected_node_contains_only_requested_node() {
    let output = selected_node(CONFIG, "secret-node").unwrap();
    let value: toml::Value = toml::from_str(&output).unwrap();
    assert_eq!(value["outbounds"].as_array().unwrap().len(), 1);
    assert_eq!(value["outbounds"][0]["tag"].as_str(), Some("secret-node"));
    assert!(!output.contains("other"));
}

#[test]
fn multiple_nodes_keep_source_order_and_share_links_roundtrip() {
    let tags = vec!["second".into(), "香港 first".into(), "second".into()];
    let output = selected_nodes(NODES, &tags).unwrap();
    let value: toml::Value = toml::from_str(&output).unwrap();
    let nodes = value["outbounds"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0]["tag"].as_str(), Some("香港 first"));
    assert_eq!(nodes[1]["tag"].as_str(), Some("second"));
    assert_eq!(value.as_table().unwrap().len(), 1);
    assert!(!output.contains("unrelated"));

    let links = selected_nodes_share(NODES, &tags).unwrap();
    assert_eq!(links.lines().count(), 2);
    let converted =
        astra_convert::share::convert_links(&links, &astra_convert::Options::default()).unwrap();
    assert_eq!(converted.nodes, 2);
    let restored: toml::Value = toml::from_str(&converted.proxy).unwrap();
    assert_eq!(restored["outbounds"], value["outbounds"]);
}

#[test]
fn node_selection_rejects_missing_ambiguous_and_unsupported_items_as_a_whole() {
    let ambiguous =
        format!("{NODES}\n[[outbounds]]\nkind='socks5'\ntag='second'\nserver='other:1080'\n");
    for export in [
        selected_nodes as fn(&str, &[String]) -> Result<String>,
        selected_nodes_share,
    ] {
        assert!(export(NODES, &[]).is_err());
        assert!(export(NODES, &["second".into(), "missing".into()]).is_err());
        assert!(export(&ambiguous, &["second".into()])
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
    }
    assert!(selected_node(&ambiguous, "second").is_err());
    let error =
        selected_nodes_share(NODES, &["香港 first".into(), "unselected".into()]).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("unselected"));
    assert!(message.contains("cannot be exported"));
    assert!(!message.contains("socks5://"));
    assert!(!message.contains("pass word"));
}

#[test]
fn share_export_bounds_expanded_output() {
    let input = format!(
        "[[outbounds]]\nkind='socks5'\ntag='large'\nserver='proxy.example:1080'\nusername='user'\npassword='{}'\n",
        "中".repeat(MAX_EXPORT_BYTES / 5)
    );
    assert!(input.len() < MAX_EXPORT_BYTES);
    assert!(selected_nodes_share(&input, &["large".into()])
        .unwrap_err()
        .to_string()
        .contains("480 KiB"));
}

#[test]
fn selected_rule_carries_rule_set_definition() {
    let output = selected_rule(CONFIG, 0).unwrap();
    let value: toml::Value = toml::from_str(&output).unwrap();
    assert_eq!(value["rules"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["rule_sets"]["ads"]["behavior"].as_str(),
        Some("domain")
    );
    assert_eq!(rule_set_dependencies(CONFIG, 0).unwrap(), vec!["ads"]);
    assert!(!selected_rule(CONFIG, 1).unwrap().contains("rule_sets"));
}

#[test]
fn rule_selection_by_value_rejects_ambiguous_values() {
    assert!(selected_rule_by_value(CONFIG, "missing").is_err());
    assert!(selected_rule_by_value(CONFIG, "example.com")
        .unwrap()
        .contains("domain_suffix"));
}

#[test]
fn windows_shell_detects_powershell_through_launchers() {
    for shell in ["powershell.exe", "PwSh.EXE"] {
        let processes = [
            (4, 3, "astra-tui.exe".into()),
            (3, 2, "cargo.exe".into()),
            (2, 1, "rustup.exe".into()),
            (1, 0, shell.into()),
        ];
        assert_eq!(
            windows_shell(4, &processes, r"C:\Windows\System32\cmd.exe"),
            WindowsShell::PowerShell
        );
    }
}

#[test]
fn windows_shell_prefers_nearest_shell_when_nested() {
    for (nearest, outer, expected) in [
        ("cmd.exe", "powershell.exe", WindowsShell::Cmd),
        ("pwsh.exe", "cmd.exe", WindowsShell::PowerShell),
    ] {
        let processes = [
            (3, 2, "astra-tui.exe".into()),
            (2, 1, nearest.into()),
            (1, 0, outer.into()),
        ];
        assert_eq!(windows_shell(3, &processes, "cmd.exe"), expected);
    }
}

#[test]
fn windows_shell_falls_back_when_ancestors_are_unavailable() {
    let processes = [(3, 2, "astra-tui.exe".into())];
    assert_eq!(
        windows_shell(3, &processes, r"C:\Windows\System32\cmd.exe"),
        WindowsShell::Cmd
    );
    assert_eq!(
        windows_shell(3, &processes, r"C:\Program Files\PowerShell\7\pwsh.exe"),
        WindowsShell::PowerShell
    );
    assert_eq!(windows_shell(3, &[], ""), WindowsShell::Cmd);
}

#[test]
fn windows_shell_bounds_cyclic_parent_records() {
    let processes = [(3, 2, "astra-tui.exe".into()), (2, 3, "cargo.exe".into())];
    assert_eq!(windows_shell(3, &processes, "cmd.exe"), WindowsShell::Cmd);
}

#[test]
fn shell_commands_are_inert_and_do_not_include_outbound_secrets() {
    let commands = shell_proxy_commands(CONFIG).unwrap();
    assert!(commands
        .posix
        .contains("export HTTP_PROXY='http://127.0.0.1:7890'"));
    assert!(commands
        .fish
        .contains("set -gx ALL_PROXY 'socks5h://127.0.0.1:7890'"));
    assert!(commands
        .powershell
        .contains("$env:HTTPS_PROXY = 'http://127.0.0.1:7890'"));
    assert!(commands
        .cmd
        .contains("set \"HTTP_PROXY=http://127.0.0.1:7890\""));
    assert!(!commands.posix.contains("must-not-be-in-shell"));
}

#[test]
fn separate_listeners_generate_expected_proxy_kinds() {
    let commands =
        shell_proxy_commands("http_listen='127.0.0.1:8080'\nsocks5_listen='[::1]:1080'").unwrap();
    assert!(commands.posix.contains("http://127.0.0.1:8080"));
    assert!(commands.posix.contains("socks5h://[::1]:1080"));
}

#[test]
fn write_new_commits_complete_content_and_never_overwrites() {
    let path = std::env::temp_dir().join(format!(
        "astra-tui-export-{}-{}",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    write_new(&path, "standalone = true\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "standalone = true\n"
    );
    assert!(write_new(&path, "overwritten = true\n").is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "standalone = true\n"
    );
    let temporary_prefix = format!(
        ".{}-{}.astra-export-",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    );
    assert!(!std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with(&temporary_prefix)));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn write_new_creates_unicode_parents_and_validates_before_creation() {
    let root = std::env::temp_dir().join(format!(
        "astra-tui-export-folders-{}-{}",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let path = root.join("导出 文件").join("节点 links.toml");
    write_new(&path, "standalone=true\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "standalone=true\n");
    assert!(write_new(&path, "replacement").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "standalone=true\n");
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );

    let too_large = root.join("not created").join("deep").join("export.toml");
    assert!(write_new(&too_large, &"x".repeat(MAX_EXPORT_BYTES + 1)).is_err());
    assert!(!root.join("not created").exists());
    let error = write_new(&path.join("child.toml"), "data").unwrap_err();
    assert!(error.to_string().contains("Create export directory"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "standalone=true\n");
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(path.parent().unwrap()).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn write_new_accepts_a_bare_relative_filename() {
    let filename = format!(
        "astra-export-relative-{}-{}.toml",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    );
    let path = Path::new(&filename);
    assert_eq!(path.parent(), Some(Path::new("")));
    write_new(path, "standalone=true\n").unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "standalone=true\n");
    std::fs::remove_file(path).unwrap();
}
