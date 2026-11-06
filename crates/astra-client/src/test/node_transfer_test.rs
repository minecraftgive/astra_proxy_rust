use super::*;
use crate::{
    editor::Editor,
    protocol::{Instance, Row},
};

const NODES: &str = "[[outbounds]]\ntag='first'\nkind='socks5'\nserver='127.0.0.1:1080'\n[[outbounds]]\ntag='second'\nkind='socks5'\nserver='127.0.0.1:1081'\n";
fn client() -> Client {
    let mut client = Client::new(Kind::Proxy);
    client.editor =
        Some(Editor::new("proxy".into(), "standalone=true\n".into()).with_kind(Kind::Proxy));
    client
}
#[test]
fn semantic_import_rejects_stale_targets_and_keeps_atomic_undo() {
    let mut client = client();
    client.rows.push(Row {
        instance: Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: "first.toml".into(),
        },
        pid: None,
        state: "Stopped".into(),
        uptime: 0,
        changed: false,
    });
    let before = client.editor.as_ref().unwrap().text();
    client
        .prepare_node_import(NODES, crate::import::MergeMode::Append)
        .unwrap();
    client.node_transfer(TransferAction::ToggleAll);
    let prompt = client.prompt.clone().unwrap();
    assert!(client.apply_node_import(&prompt).is_err());
    assert_eq!(client.editor.as_ref().unwrap().text(), before);
    client.node_transfer(TransferAction::ToggleAll);
    let prompt = client.prompt.clone().unwrap();
    client.rows[0].instance.config = "changed.toml".into();
    assert!(client
        .apply_node_import(&prompt)
        .unwrap_err()
        .to_string()
        .contains("target changed"));
    client.rows[0].instance.config = "first.toml".into();
    client.node_transfer(TransferAction::Next);
    client.node_transfer(TransferAction::Toggle);
    client.node_transfer(TransferAction::Confirm);
    assert!(client.prompt.is_none());
    assert!(client.recovery.is_none());
    let edited: toml::Value = toml::from_str(&client.editor.as_ref().unwrap().text()).unwrap();
    assert_eq!(edited["outbounds"].as_array().unwrap().len(), 1);
    assert_eq!(edited["outbounds"][0]["tag"].as_str(), Some("first"));
    client.editor.as_mut().unwrap().restore(false).unwrap();
    assert_eq!(client.editor.as_ref().unwrap().text(), before);
}
#[test]
fn semantic_clipboard_failure_retains_destination_for_recovery() {
    let mut client = client();
    client.prompt(Prompt::ExportNodeDestination {
        text: NODES.into(),
        share: false,
        clipboard: false,
    });
    client.node_transfer(TransferAction::SwitchDestination);
    client.node_transfer_with(TransferAction::Confirm, |_| anyhow::bail!("clipboard busy"));
    let recovery = client.recovery.as_ref().unwrap();
    assert!(recovery.message.contains("clipboard busy"));
    assert!(matches!(
        &recovery.resume,
        Resume::Prompt(
            Prompt::ExportNodeDestination {
                clipboard: true,
                ..
            },
            _
        )
    ));
    assert!(client.prompt.is_none());
}

#[test]
fn click_selection_and_inline_submission_preserve_invalid_prompt() {
    let mut client = client();
    client
        .prepare_node_import(NODES, crate::import::MergeMode::Append)
        .unwrap();
    client.node_transfer(TransferAction::ToggleIndex(0));
    client.node_transfer(TransferAction::ToggleIndex(1));
    assert!(client.submit_transfer().is_err());
    assert!(client.recovery.is_none());
    assert!(
        matches!(&client.prompt, Some(Prompt::ImportNodeSelection { selected, .. }) if selected.is_empty())
    );
    client.node_transfer(TransferAction::ToggleIndex(1));
    client.submit_transfer().unwrap();
    assert!(client.prompt.is_none());
    let text = client.editor.as_ref().unwrap().text();
    assert!(text.contains("second"));
    assert!(!text.contains("first"));
    client.prompt(Prompt::ExportNodeDestination {
        text: NODES.into(),
        share: false,
        clipboard: false,
    });
    client.node_transfer(TransferAction::SetDestination(true));
    assert!(client
        .submit_transfer_with(|_| anyhow::bail!("clipboard busy"))
        .is_err());
    assert!(matches!(
        &client.prompt,
        Some(Prompt::ExportNodeDestination {
            clipboard: true,
            ..
        })
    ));
}
