use super::*;
use std::{collections::BTreeMap, time::Duration};

#[test]
fn environment_child() {
    if std::env::var_os("NETTOOL_ENV_CHILD").is_none() {
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    let runtime = PathBuf::from(std::env::var_os("ASTRA_RUNTIME_DIR").unwrap());
    let temporary = std::env::temp_dir();
    std::fs::write(cwd.join("relative-child-state"), "child").unwrap();
    std::fs::write(runtime.join("child-state"), "child").unwrap();
    std::fs::write(temporary.join("child-temporary"), "child").unwrap();
    let variables = std::env::vars()
        .filter(|(key, _)| {
            key.starts_with("ASTRA_")
                || [
                    "HOME",
                    "USERPROFILE",
                    "XDG_CONFIG_HOME",
                    "PATH",
                    "SSL_CERT_FILE",
                    "SSL_CERT_DIR",
                    "HOSTALIASES",
                    "RESOLV_HOST_CONF",
                    "LOCALDOMAIN",
                    "RES_OPTIONS",
                    "LD_NETTOOL_TEST",
                ]
                .contains(&key.as_str())
        })
        .collect::<BTreeMap<_, _>>();
    let ssl_contents =
        std::env::var_os("SSL_CERT_FILE").and_then(|path| std::fs::read_to_string(path).ok());
    let report = serde_json::json!({"cwd": cwd, "runtime": runtime, "tmp": temporary,
        "variables": variables, "ssl_contents": ssl_contents});
    std::fs::write(
        std::env::var_os("NETTOOL_ENV_REPORT").unwrap(),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    if std::env::var_os("NETTOOL_ENV_WAIT").is_some() {
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    assert!(
        std::env::var_os("NETTOOL_ENV_FAIL").is_none(),
        "fixture child failure"
    );
}

fn variables(original: &Path, report: &Path) -> Vec<(OsString, OsString)> {
    // Keep the platform launch essentials, but supply fixture values without
    // mutating process-global environment while Rust tests run concurrently.
    let mut values = std::env::vars_os().collect::<Vec<_>>();
    values.retain(|(key, _)| {
        let key = key.to_string_lossy().to_ascii_uppercase();
        !key.starts_with("LD_") && !key.starts_with("DYLD_")
    });
    let additions = [
        ("NETTOOL_ENV_CHILD", "1".into()),
        ("NETTOOL_ENV_REPORT", report.as_os_str().to_owned()),
        (
            "ASTRA_CONTROL_SOCKET",
            original.join("production.sock").into_os_string(),
        ),
        (
            "ASTRA_RUNTIME_DIR",
            original.join("production-runtime").into_os_string(),
        ),
        ("ASTRA_HOSTED_EXPERIMENTAL", "1".into()),
        ("ASTRA_LOG_LEVEL", "trace".into()),
        (
            "ASTRA_ELEVATED_OWNER",
            "invalid-owner-must-not-reach-child".into(),
        ),
        ("HOME", original.join("home").into_os_string()),
        ("USERPROFILE", original.join("profile").into_os_string()),
        ("XDG_CONFIG_HOME", original.join("xdg").into_os_string()),
        ("SSL_CERT_FILE", "fixture.pem".into()),
        (
            "SSL_CERT_DIR",
            std::env::join_paths([Path::new("certs"), Path::new(""), original]).unwrap(),
        ),
        ("HOSTALIASES", "aliases".into()),
        ("RESOLV_HOST_CONF", "resolver.conf".into()),
        ("LOCALDOMAIN", "test.invalid".into()),
        ("RES_OPTIONS", "attempts:1".into()),
        (
            "PATH",
            std::env::join_paths([Path::new("relative-bin"), Path::new(""), original]).unwrap(),
        ),
    ];
    for (key, value) in additions {
        values.retain(|(existing, _)| !existing.to_string_lossy().eq_ignore_ascii_case(key));
        values.push((key.into(), value));
    }
    values
}

fn environment(root: &Path, report: &Path, isolated: bool) -> Environment {
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(root.join("fixture.pem"), "fixture trust anchor").unwrap();
    Environment::with_environment(&runtime, isolated, root.to_owned(), variables(root, report))
        .unwrap()
}

fn child(environment: &mut Environment) -> Child {
    let mut command = environment
        .command(&std::env::current_exe().unwrap())
        .unwrap();
    command
        .args([
            "--exact",
            "manager::nettool_environment::tests::environment_child",
            "--nocapture",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    environment.spawn(&mut command).unwrap()
}

async fn report(path: &Path) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(value) = serde_json::from_slice(&bytes) {
                    return value;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn real_children_have_distinct_private_state_and_preserved_host_context() {
    let parent = tempfile::tempdir().unwrap();
    let root = &parent.path().canonicalize().unwrap();
    let first_report = root.join("first.json");
    let second_report = root.join("second.json");
    let mut first = environment(root, &first_report, true);
    let mut second = environment(root, &second_report, true);
    let first_root = first.root.as_ref().unwrap().path().to_owned();
    let second_root = second.root.as_ref().unwrap().path().to_owned();
    assert_ne!(first_root, second_root);
    let (mut first_child, mut second_child) = (child(&mut first), child(&mut second));
    let (one, two) = tokio::join!(first_child.wait(), second_child.wait());
    assert!(one.unwrap().success());
    assert!(two.unwrap().success());
    for (data, private) in [
        (report(&first_report).await, &first_root),
        (report(&second_report).await, &second_root),
    ] {
        assert_eq!(data["cwd"], private.join("work").to_string_lossy().as_ref());
        assert_eq!(
            data["runtime"],
            private.join("runtime").to_string_lossy().as_ref()
        );
        assert_eq!(data["tmp"], private.join("tmp").to_string_lossy().as_ref());
        let vars = data["variables"].as_object().unwrap();
        assert_eq!(
            vars.keys()
                .filter(|key| key.starts_with("ASTRA_"))
                .collect::<Vec<_>>(),
            vec!["ASTRA_RUNTIME_DIR"]
        );
        for (key, suffix) in [
            ("HOME", "home"),
            ("USERPROFILE", "profile"),
            ("XDG_CONFIG_HOME", "xdg"),
            ("HOSTALIASES", "aliases"),
            ("RESOLV_HOST_CONF", "resolver.conf"),
        ] {
            assert_eq!(vars[key], root.join(suffix).to_string_lossy().as_ref());
        }
        assert_eq!(vars["LOCALDOMAIN"], "test.invalid");
        assert_eq!(vars["RES_OPTIONS"], "attempts:1");
        assert_eq!(data["ssl_contents"], "fixture trust anchor");
        for (key, relative) in [("SSL_CERT_DIR", "certs"), ("PATH", "relative-bin")] {
            assert_eq!(
                std::env::split_paths(vars[key].as_str().unwrap()).collect::<Vec<_>>(),
                vec![
                    root.join(relative),
                    if key == "SSL_CERT_DIR" {
                        PathBuf::new()
                    } else {
                        root.to_owned()
                    },
                    root.to_owned()
                ]
            );
        }
        assert!(private.join("runtime/child-state").is_file());
        assert!(private.join("tmp/child-temporary").is_file());
    }
    assert!(!root.join("relative-child-state").exists());
    assert!(!root.join("runtime/child-state").exists());
    first.finish(Ok(())).unwrap();
    second.finish(Ok(())).unwrap();
    assert!(!first_root.exists());
    assert!(!second_root.exists());
}

#[tokio::test]
async fn cancellation_reaps_before_private_state_is_removed() {
    let parent = tempfile::tempdir().unwrap();
    let report_path = parent.path().join("report.json");
    let mut environment = environment(parent.path(), &report_path, true);
    environment.set("NETTOOL_ENV_WAIT", "1".into());
    let root = environment.root.as_ref().unwrap().path().to_owned();
    let mut child = child(&mut environment);
    report(&report_path).await;
    assert!(root.join("runtime/child-state").is_file());
    super::super::network::terminate(&mut child).await.unwrap();
    assert!(child.try_wait().unwrap().is_some());
    environment.finish(Ok(())).unwrap();
    assert!(!root.exists());
}

#[tokio::test]
async fn failed_child_exit_is_reaped_and_private_files_are_removed() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("report.json");
    let mut environment = environment(parent.path(), &path, true);
    environment.set("NETTOOL_ENV_FAIL", "1".into());
    let root = environment.root.as_ref().unwrap().path().to_owned();
    let mut child = child(&mut environment);
    assert!(!child.wait().await.unwrap().success());
    assert!(child.try_wait().unwrap().is_some());
    report(&path).await;
    let error = environment
        .finish::<()>(Err(anyhow::anyhow!("fixture probe failed")))
        .unwrap_err();
    assert!(error.to_string().contains("fixture probe failed"));
    assert!(!root.exists());
}

#[test]
fn unconfirmed_cleanup_or_dropped_owner_retains_state() {
    let parent = tempfile::tempdir().unwrap();
    for unexpected_drop in [false, true] {
        let mut environment = environment(parent.path(), &parent.path().join("report"), true);
        let root = environment.root.as_ref().unwrap().path().to_owned();
        environment.spawned = true;
        if unexpected_drop {
            drop(environment);
        } else {
            let error = environment
                .finish::<()>(Err(crate::network::CleanupUnconfirmed(
                    "fixture reaper failure".into(),
                )
                .into()))
                .unwrap_err();
            assert!(error
                .chain()
                .any(|cause| cause.is::<crate::network::CleanupUnconfirmed>()));
        }
        assert!(root.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn legacy_outbound_preserves_original_cache_and_relative_assets() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().canonicalize().unwrap();
    let path = root.join("report.json");
    let mut environment = environment(&root, &path, false);
    // Keep fixture temporary writes inside this test's directory for the
    // deliberate legacy exception; production values remain inherited.
    for key in ["TMPDIR", "TEMP", "TMP"] {
        environment.set(key, parent.path().as_os_str().to_owned());
    }
    assert!(environment.root.is_none());
    assert!(child(&mut environment).wait().await.unwrap().success());
    let data = report(&path).await;
    assert_eq!(data["cwd"], root.to_string_lossy().as_ref());
    assert_eq!(
        data["runtime"],
        root.join("runtime").to_string_lossy().as_ref()
    );
    assert_eq!(data["variables"]["SSL_CERT_FILE"], "fixture.pem");
    assert_eq!(data["ssl_contents"], "fixture trust anchor");
    environment.finish(Ok(())).unwrap();
    assert!(parent.path().join("runtime/child-state").is_file());
}

#[tokio::test]
async fn dynamic_loader_override_keeps_cwd_but_isolates_runtime_and_temporary_files() {
    let parent = tempfile::tempdir().unwrap();
    let root = &parent.path().canonicalize().unwrap();
    let path = root.join("report.json");
    std::fs::create_dir(root.join("runtime")).unwrap();
    std::fs::write(root.join("fixture.pem"), "fixture trust anchor").unwrap();
    let mut values = variables(root, &path);
    values.push(("LD_NETTOOL_TEST".into(), "relative/$ORIGIN".into()));
    let mut environment =
        Environment::with_environment(&root.join("runtime"), true, root.to_owned(), values)
            .unwrap();
    let private = environment.root.as_ref().unwrap().path().to_owned();
    assert!(child(&mut environment).wait().await.unwrap().success());
    let data = report(&path).await;
    assert_eq!(data["cwd"], root.to_string_lossy().as_ref());
    assert_eq!(
        data["runtime"],
        private.join("runtime").to_string_lossy().as_ref()
    );
    assert_eq!(data["tmp"], private.join("tmp").to_string_lossy().as_ref());
    assert_eq!(data["variables"]["LD_NETTOOL_TEST"], "relative/$ORIGIN");
    assert_eq!(data["variables"]["SSL_CERT_FILE"], "fixture.pem");
    environment.finish(Ok(())).unwrap();
    assert!(!private.exists());
}

#[test]
fn empty_trust_and_resolver_overrides_do_not_gain_working_directory_semantics() {
    let parent = tempfile::tempdir().unwrap();
    let mut environment = environment(parent.path(), &parent.path().join("report"), true);
    for key in [
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HOSTALIASES",
        "RESOLV_HOST_CONF",
        "PATH",
    ] {
        environment.set(key, OsString::new());
    }
    environment.rebase_paths().unwrap();
    for (key, value) in &environment.variables {
        if [
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "HOSTALIASES",
            "RESOLV_HOST_CONF",
        ]
        .iter()
        .any(|expected| key == expected)
        {
            assert!(value.is_empty(), "empty override was changed: {key:?}");
        } else if key == "PATH" {
            assert_eq!(Path::new(value), environment.original_cwd);
        }
    }
    environment.finish(Ok(())).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_manager_launch_clears_only_non_outbound_profiles_and_marks_context_accurately() {
    use astra_model::nettool::{Egress, Event, Probe, Request};
    use std::os::unix::fs::PermissionsExt;
    for egress in [
        Egress::System,
        Egress::Interface("fixture0".into()),
        Egress::Outbound("selected".into()),
    ] {
        let outbound = matches!(egress, Egress::Outbound(_));
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path();
        let runtime = root.join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::write(runtime.join("production-sentinel"), "unchanged").unwrap();
        let binary = root.join("fixture-core");
        let started = serde_json::to_string(&Event::Started {
            egress: egress.to_string(),
            probe: "fixture".into(),
            note: "fixture".into(),
            running_config: true,
        })
        .unwrap();
        let expected_config = if outbound { "profile sentinel" } else { "" };
        let script = format!(
            r##"#!/bin/sh
set -eu
[ "$1" = --nettool-stream ]
case "$2" in /*) ;; *) exit 11;; esac
payload=$(cat)
case "$payload" in *'"config":"{expected_config}"'*) ;; *) exit 12;; esac
printf child > "$ASTRA_RUNTIME_DIR/child-state"
printf '%s\n' '{started}' '{{"event":"done"}}'
"##
        );
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plan = super::super::DiagnosticPlan {
            runtime_dir: runtime.clone(),
            log_path: root.join("unused.log"),
            executable: binary,
            config_path: "relative-profile.toml".into(),
            text: "profile sentinel".into(),
            running: true,
            // Any accidental configuration refresh for system/interface paths
            // fails against this intentionally nonexistent endpoint.
            control: (!outbound).then(|| (root.join("unreachable.sock"), Some(u32::MAX))),
        };
        let request = Request {
            config: "client supplied profile must be replaced".into(),
            egress,
            probe: Probe::Tcp {
                host: "127.0.0.1".into(),
                port: 9,
            },
            count: 1,
            timeout_ms: 100,
            interval_ms: 0,
        };
        let mut output = Vec::new();
        plan.nettool(request, &mut output).await.unwrap();
        let mut output = output.as_slice();
        let first = astra_ipc::receive_management::<astra_management::Reply>(&mut output)
            .await
            .unwrap();
        let Event::Started { running_config, .. } = serde_json::from_str(&first.text).unwrap()
        else {
            panic!("missing Started event");
        };
        assert_eq!(running_config, outbound);
        assert_eq!(
            std::fs::read_to_string(runtime.join("production-sentinel")).unwrap(),
            "unchanged"
        );
        assert_eq!(runtime.join("child-state").exists(), outbound);
        assert!(std::fs::read_dir(&runtime).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("nettool-")));
    }
}

#[cfg(windows)]
#[test]
fn drive_relative_paths_are_resolved_before_launch() {
    let cwd = std::env::current_dir().unwrap();
    let std::path::Component::Prefix(prefix) = cwd.components().next().unwrap() else {
        panic!("Windows cwd must have an absolute prefix");
    };
    if let std::path::Prefix::Disk(drive) | std::path::Prefix::VerbatimDisk(drive) = prefix.kind() {
        let relative = PathBuf::from(format!("{}:fixture.pem", char::from(drive)));
        let resolved = absolute_in(&relative, &cwd).unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, std::path::absolute(relative).unwrap());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unrepresentable_rebased_path_lists_keep_original_cwd_and_variables_atomically() {
    let parent = tempfile::tempdir().unwrap();
    let parent_path = parent.path().canonicalize().unwrap();
    for failing_key in ["SSL_CERT_DIR", "PATH"] {
        let root = parent_path.join(format!("colon:cwd-{failing_key}"));
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        std::fs::write(root.join("fixture.pem"), "fixture trust anchor").unwrap();
        let report_path = root.join("report.json");
        let mut values = variables(&parent_path, &report_path);
        for key in ["SSL_CERT_DIR", "PATH"] {
            let value = if key == failing_key {
                OsString::from("relative::other")
            } else {
                parent_path.as_os_str().to_owned()
            };
            values.retain(|(existing, _)| existing != key);
            values.push((key.into(), value));
        }
        let before = values
            .iter()
            .filter(|(key, _)| {
                [
                    "SSL_CERT_FILE",
                    "SSL_CERT_DIR",
                    "PATH",
                    "HOSTALIASES",
                    "RESOLV_HOST_CONF",
                ]
                .iter()
                .any(|expected| key == expected)
            })
            .cloned()
            .collect::<BTreeMap<_, _>>();
        let mut environment =
            Environment::with_environment(&root.join("runtime"), true, root.clone(), values)
                .unwrap();
        let private = environment.root.as_ref().unwrap().path().to_owned();
        assert_eq!(environment.cwd, root);
        assert!(child(&mut environment).wait().await.unwrap().success());
        let data = report(&report_path).await;
        assert_eq!(data["cwd"], root.to_string_lossy().as_ref());
        assert_eq!(
            data["runtime"],
            private.join("runtime").to_string_lossy().as_ref()
        );
        assert_eq!(data["tmp"], private.join("tmp").to_string_lossy().as_ref());
        assert_eq!(data["ssl_contents"], "fixture trust anchor");
        for (key, value) in before {
            assert_eq!(
                data["variables"][key.to_str().unwrap()],
                value.to_string_lossy().as_ref()
            );
        }
        environment.finish(Ok(())).unwrap();
        assert!(!private.exists());
    }
}

#[cfg(unix)]
#[test]
fn unix_custom_lowercase_variables_are_not_rewritten_or_treated_as_loader_overrides() {
    let parent = tempfile::tempdir().unwrap();
    std::fs::create_dir(parent.path().join("runtime")).unwrap();
    let custom = [
        "ssl_cert_file",
        "ssl_cert_dir",
        "hostaliases",
        "resolv_host_conf",
        "path",
        "tmp",
        "temp",
        "tmpdir",
        "astra_custom",
        "ld_custom",
        "dyld_custom",
    ];
    let values = custom
        .iter()
        .map(|key| (OsString::from(key), OsString::from("relative-custom-value")))
        .collect();
    let environment = Environment::with_environment(
        &parent.path().join("runtime"),
        true,
        parent.path().to_owned(),
        values,
    )
    .unwrap();
    assert_ne!(environment.cwd, parent.path());
    for key in custom {
        let (_, value) = environment
            .variables
            .iter()
            .find(|(name, _)| name == key)
            .unwrap();
        assert_eq!(value, "relative-custom-value");
    }
    environment.finish(Ok(())).unwrap();
}
