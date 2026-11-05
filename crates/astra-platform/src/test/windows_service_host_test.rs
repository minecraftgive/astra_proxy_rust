use super::*;

fn identity() -> crate::lifecycle::ProcessIdentity {
    crate::lifecycle::ProcessIdentity {
        pid: 42,
        started: "trusted-kernel-start".into(),
    }
}
fn args() -> Vec<String> {
    [
        "--windows-elevated",
        r"C:\Users\owner\profile.toml",
        r"C:\Users\owner\state\proxy.sock",
        r"C:\Users\owner\state\proxy.log",
        "info",
        r"C:\untrusted\wintun.dll",
        r"C:\untrusted",
        "S-1-5-18",
        r"C:\Users\owner\runtime",
        "",
        "0",
        "forged-owner",
    ]
    .map(str::to_owned)
    .into()
}
#[test]
fn core_launch_overrides_every_privileged_identity_and_search_argument() {
    let install = Path::new(r"C:\Program Files\Astra Privilege\owner");
    let owner = "S-1-5-21-1-2-3-1001";
    let actual = validated_args(args(), owner, install, &identity()).unwrap();
    assert_eq!(actual[1], args()[1]);
    assert_eq!(actual[5], install.join("wintun.dll").to_str().unwrap());
    assert_eq!(actual[6], install.to_str().unwrap());
    assert_eq!(actual[7], owner);
    assert_eq!(actual[10], "1");
    assert_eq!(
        serde_json::from_str::<crate::lifecycle::ProcessIdentity>(&actual[11]).unwrap(),
        identity()
    );
}
#[test]
fn privileged_argument_contract_rejects_extra_flags_relative_paths_and_nuls() {
    let install = Path::new(r"C:\Program Files\Astra");
    let owner = "S-1-5-21-1-2-3-1001";
    let mut malformed = args();
    malformed.push("--arbitrary-flag".into());
    assert!(validated_args(malformed, owner, install, &identity()).is_err());
    for (index, value) in [
        (0, "--normal"),
        (1, "relative.toml"),
        (2, ""),
        (3, "bad\0path"),
        (4, "anything"),
        (8, "runtime"),
        (9, "0"),
    ] {
        let mut malformed = args();
        malformed[index] = value.into();
        assert!(
            validated_args(malformed, owner, install, &identity()).is_err(),
            "index {index}"
        );
    }
}
#[test]
fn owner_identity_never_accepts_system_other_users_or_another_session() {
    let owner = "S-1-5-21-1-2-3-1001";
    assert!(verify_identity(owner, owner, 4, 4).is_ok());
    for (actual, session, token_session) in [
        ("S-1-5-18", 4, 4),
        ("S-1-5-21-1-2-3-1002", 4, 4),
        (owner, 0, 0),
        (owner, 4, 5),
    ] {
        assert!(verify_identity(owner, actual, session, token_session).is_err());
    }
}
#[test]
fn command_line_quotes_backslashes_and_embedded_quotes_without_a_shell() {
    assert_eq!(quote(""), "\"\"");
    assert_eq!(
        quote(r"C:\Program Files\Astra\"),
        "\"C:\\Program Files\\Astra\\\\\""
    );
    assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    assert_eq!(quote("a&b%PATH%"), "\"a&b%PATH%\"");
}
#[test]
fn child_environment_contains_only_fixed_system_search_locations() {
    let block = environment(Path::new(r"C:\Program Files\Astra")).unwrap();
    assert_eq!(&block[block.len() - 2..], &[0, 0]);
    let text = String::from_utf16(&block).unwrap();
    let entries: Vec<_> = text.split('\0').filter(|entry| !entry.is_empty()).collect();
    assert_eq!(entries.len(), 3);
    assert!(entries[0].starts_with(r"PATH=C:\Program Files\Astra;"));
    assert!(entries[0].ends_with(r"\System32"));
    assert!(entries[1].starts_with("SystemRoot="));
    assert!(entries[2].starts_with("WINDIR="));
}

#[tokio::test]
async fn handoff_waits_for_client_ownership_before_resuming_and_sending_ready() {
    use std::sync::Arc;
    let (mut client, mut server) = tokio::io::duplex(16);
    let resumed = Arc::new(AtomicBool::new(false));
    let observed = resumed.clone();
    let task = tokio::spawn(async move {
        acknowledge_and_resume(&mut server, || {
            observed.store(true, Ordering::Release);
            Ok(())
        })
        .await
    });
    tokio::task::yield_now().await;
    assert!(!resumed.load(Ordering::Acquire));
    client.write_u8(1).await.unwrap();
    assert_eq!(client.read_u8().await.unwrap(), 1);
    task.await.unwrap().unwrap();
    assert!(resumed.load(Ordering::Acquire));
}

#[tokio::test]
async fn handoff_missing_or_invalid_ack_never_resumes_the_candidate() {
    use std::sync::Arc;
    for acknowledgment in [None, Some(0)] {
        let (mut client, mut server) = tokio::io::duplex(16);
        let resumed = Arc::new(AtomicBool::new(false));
        let observed = resumed.clone();
        let task = tokio::spawn(async move {
            acknowledge_and_resume(&mut server, || {
                observed.store(true, Ordering::Release);
                Ok(())
            })
            .await
        });
        if let Some(byte) = acknowledgment {
            client.write_u8(byte).await.unwrap();
        }
        drop(client);
        assert!(task.await.unwrap().is_err());
        assert!(!resumed.load(Ordering::Acquire));
    }
}

#[tokio::test]
async fn handoff_resume_failure_never_reports_ready() {
    let (mut client, mut server) = tokio::io::duplex(16);
    let task = tokio::spawn(async move {
        acknowledge_and_resume(&mut server, || Err(io::Error::other("resume failed"))).await
    });
    client.write_u8(1).await.unwrap();
    assert!(client.read_u8().await.is_err());
    assert!(task
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("resume failed"));
}
