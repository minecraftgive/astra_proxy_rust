use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::Command;

#[test]
#[ignore = "requires root in a disposable Linux network namespace, cgroup v2 and a compiled BPF object"]
fn isolated_capture() -> Result<()> {
    ensure!(
        std::fs::read_link("/proc/self/ns/net")? != std::fs::read_link("/proc/1/ns/net")?,
        "use unshare --net to isolate nftables and routing"
    );
    let object =
        std::fs::read(std::env::var_os("ASTRA_CAPTURE_OBJECT").context("ASTRA_CAPTURE_OBJECT")?)?;
    struct Cgroup(std::path::PathBuf);
    impl Drop for Cgroup {
        fn drop(&mut self) {
            std::fs::remove_dir(&self.0).expect("remove private test cgroup");
        }
    }
    let path = Path::new("/sys/fs/cgroup").join(format!("astra-capture-{}", std::process::id()));
    std::fs::create_dir(&path)?;
    let cgroup = Cgroup(path);
    let mut capture = Capture::attach(&object, &cgroup.0)?;
    let root = tempfile::tempdir()?;
    let target = root.path().join("target/client");
    let control = root.path().join("control/client");
    let python = std::fs::canonicalize("/usr/bin/python3")?;
    for executable in [&target, &control] {
        std::fs::create_dir(executable.parent().unwrap())?;
        std::fs::copy(&python, executable)?;
    }
    let probe = |phase: &str| -> Result<()> {
        let result = Command::new(&python)
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/test/linux_capture_probe.py"
            ))
            .arg(phase)
            .arg(&cgroup.0)
            .arg(&target)
            .arg(&control)
            .status()?;
        ensure!(result.success(), "{phase} probe failed");
        Ok(())
    };
    capture.set(&target, Disposition::Block)?;
    probe("block")?;
    let worker = cgroup.0.join("worker");
    std::fs::create_dir(&worker)?;
    let worker = Cgroup(worker);
    let worker_id = capture.exclude_forwarders(&worker.0)?;
    let status = Command::new(&python)
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/test/linux_capture_probe.py"
        ))
        .arg("removed")
        .arg(&worker.0)
        .arg(&target)
        .arg(&control)
        .status()?;
    ensure!(status.success(), "forwarder exclusion failed");
    capture.remove_forwarder_exclusion(worker_id)?;
    drop(worker);
    capture.set(
        &target,
        Disposition::Transparent(NonZeroU32::new(77).unwrap()),
    )?;
    probe("transparent")?;
    if std::env::var_os("ASTRA_CAPTURE_CORE").is_some() {
        let result = Command::new(&python)
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/test/linux_capture_egress.py"
            ))
            .arg(&cgroup.0)
            .arg(&target)
            .arg(&control)
            .status()?;
        ensure!(result.success(), "real core egress test failed");
    }
    // Hold sockets across a successful policy update. The same unconnected
    // UDP socket must retain its mark even when it next sends to a new peer.
    let mut child = Command::new(&target)
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/test/linux_capture_probe.py"
        ))
        .arg("hold")
        .arg(&cgroup.0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut ready = String::new();
    BufReader::new(child.stdout.take().context("probe output")?).read_line(&mut ready)?;
    ensure!(ready.trim() == "ready", "socket retention probe failed");
    capture.set(&target, Disposition::Block)?;
    child
        .stdin
        .take()
        .context("probe input")?
        .write_all(b"updated\n")?;
    ensure!(
        child.wait()?.success(),
        "socket retention probe failed after update"
    );
    capture.remove(&target)?;
    probe("removed")
}
