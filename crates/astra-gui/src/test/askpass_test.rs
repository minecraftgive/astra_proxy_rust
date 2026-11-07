use super::*;
use gpui::TestAppContext;

#[test]
fn askpass_is_an_explicit_early_mode() {
    let args = |values: &[&str]| {
        values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>()
    };
    assert!(requested_with_marker(
        &args(&["--askpass", "sudo prompt"]),
        None
    ));
    assert!(requested_with_marker(
        &args(&["sudo prompt"]),
        Some(std::ffi::OsStr::new("1"))
    ));
    assert!(!requested_with_marker(
        &args(&["--endpoint", "/tmp/--askpass.sock"]),
        None
    ));
    assert!(!requested_with_marker(&[], Some(std::ffi::OsStr::new("0"))));
}

#[gpui::test]
fn secret_edits_are_bounded_masked_and_cancel_returns_one(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let outcome = Arc::new(AtomicI32::new(1));
    let mut secret = None;
    let window = cx.add_window(|window, cx| {
        let view = cx.new(|cx| Secret::new(outcome.clone(), window, cx));
        secret = Some(view.clone());
        Root::new(view, window, cx)
    });
    let secret = secret.unwrap();
    window
        .update(cx, |_, window, cx| {
            secret.update(cx, |secret, cx| {
                let capacities = [secret.value.capacity(), secret.scratch.capacity()];
                secret.replace_text_in_range(None, "密碼é", window, cx);
                assert_eq!(&*secret.value, "密碼é");
                assert!(secret.text_for_range(0..3, &mut None, window, cx).is_none());
                secret.replace_text_in_range(Some(1..2), "A", window, cx);
                assert_eq!(&*secret.value, "密Aé");
                assert!(secret.scratch.is_empty());
                assert!(!secret.replace(0..0, &"x".repeat(CAPACITY)));
                assert!(!secret.replace(0..0, "\n"));
                assert!(capacities.contains(&secret.value.capacity()));
                assert!(capacities.contains(&secret.scratch.capacity()));
                secret.finish(false, cx);
                assert!(secret.value.is_empty());
                assert!(secret.scratch.is_empty());
            })
        })
        .unwrap();
    assert_eq!(outcome.load(Ordering::Acquire), 1);
}

#[gpui::test]
fn secret_submission_writes_once_and_clears_even_on_io_failure(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let mut secret = None;
    let window = cx.add_window(|window, cx| {
        let view = cx.new(|cx| Secret::new(Arc::new(AtomicI32::new(1)), window, cx));
        secret = Some(view.clone());
        Root::new(view, window, cx)
    });
    let secret = secret.unwrap();
    window
        .update(cx, |_, window, cx| {
            secret.update(cx, |secret, cx| {
                secret.replace_text_in_range(None, "test-only", window, cx);
                let mut output = Zeroizing::new(Vec::new());
                assert!(secret.write_secret(&mut *output));
                assert_eq!(&*output, b"test-only\n");
                assert!(secret.value.is_empty());
                secret.replace_text_in_range(None, "test-only", window, cx);
                struct Failed;
                impl Write for Failed {
                    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                        Err(std::io::Error::other("closed"))
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                assert!(!secret.write_secret(&mut Failed));
                assert!(secret.value.is_empty());
            })
        })
        .unwrap();
}

#[cfg(unix)]
#[test]
fn parent_watch_probe() {
    let Some(directory) = std::env::var_os("ASTRA_TEST_ASKPASS_PARENT") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let original = parent_id();
    std::fs::write(directory.join("ready"), b"ready").unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < end {
        if parent_id() != original {
            std::fs::write(directory.join("cancelled"), b"cancelled").unwrap();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("fake askpass parent did not disappear");
}

#[cfg(unix)]
#[test]
fn terminating_a_fake_sudo_parent_is_observed_without_real_elevation() {
    let directory =
        std::env::temp_dir().join(format!("astra-askpass-parent-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let mut parent = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "\"$1\" --exact askpass::tests::parent_watch_probe --nocapture & wait",
            "astra-test",
        ])
        .arg(std::env::current_exe().unwrap())
        .env("ASTRA_TEST_ASKPASS_PARENT", &directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let wait_for = |name: &str| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while !directory.join(name).exists() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        directory.join(name).exists()
    };
    let ready = wait_for("ready");
    parent.kill().unwrap();
    parent.wait().unwrap();
    let cancelled = wait_for("cancelled");
    std::fs::remove_dir_all(directory).unwrap();
    assert!(ready && cancelled, "the same parent-liveness detector used by the askpass window must cancel after its launcher dies");
}
