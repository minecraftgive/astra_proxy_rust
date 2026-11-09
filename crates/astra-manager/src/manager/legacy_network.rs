//! Legacy tools retain their common scheduler permit until every child is reaped.
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub(super) async fn collect(child: &mut Child, payload: Vec<u8>) -> Result<std::process::Output> {
    let run = async {
        let mut input = child.stdin.take().context("Missing diagnostic input")?;
        let output = child.stdout.take().context("Missing diagnostic output")?;
        let errors = child.stderr.take();
        let write = async move {
            input.write_all(&payload).await?;
            input.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        };
        let stdout = async {
            let mut bytes = Vec::new();
            output.take(900 * 1024 + 1).read_to_end(&mut bytes).await?;
            anyhow::ensure!(bytes.len() <= 900 * 1024, "Diagnostic report exceeds limit");
            Ok::<_, anyhow::Error>(bytes)
        };
        let stderr = async {
            let mut bytes = Vec::new();
            if let Some(errors) = errors {
                errors.take(4097).read_to_end(&mut bytes).await?;
                anyhow::ensure!(bytes.len() <= 4096, "Diagnostic error output exceeds limit");
            }
            Ok::<_, anyhow::Error>(bytes)
        };
        let ((), stdout, stderr) = tokio::try_join!(write, stdout, stderr)?;
        Ok(std::process::Output {
            status: child.wait().await?,
            stdout,
            stderr,
        })
    };
    let result = timeout(Duration::from_secs(70), run)
        .await
        .context("Legacy diagnostic timed out")
        .and_then(|r| r);
    if result.is_err() {
        super::network::terminate(child).await?;
    }
    result
}

pub(super) async fn pipe_events<E: serde::de::DeserializeOwned + Serialize>(
    (executable, runtime_dir): (PathBuf, PathBuf),
    flag: &str,
    config_path: PathBuf,
    payload: Vec<u8>,
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
    outdated: &'static str,
    done: impl FnMut(&mut E) -> bool,
) -> Result<()> {
    let mut child = core_process(executable, &runtime_dir)
        .arg(flag)
        .arg(config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    pipe_child_events(&mut child, payload, output, outdated, done).await
}

pub(super) async fn pipe_child_events<E: serde::de::DeserializeOwned + Serialize>(
    child: &mut Child,
    payload: Vec<u8>,
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
    outdated: &'static str,
    mut done: impl FnMut(&mut E) -> bool,
) -> Result<()> {
    let run = async {
        let mut input = child.stdin.take().context("Missing diagnostic input")?;
        let stderr = child.stderr.take().context("Missing diagnostic errors")?;
        let mut reader = BufReader::new(child.stdout.take().context("Missing diagnostic output")?);
        let write = async move {
            input.write_all(&payload).await?;
            input.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        };
        let errors = async {
            let mut bytes = Vec::new();
            stderr.take(4097).read_to_end(&mut bytes).await?;
            anyhow::ensure!(bytes.len() <= 4096, "Diagnostic error output exceeds limit");
            Ok::<_, anyhow::Error>(bytes)
        };
        let read = async {
            for _ in 0..20000 {
                let mut line = String::new();
                let n = (&mut reader).take(1024 * 1024).read_line(&mut line).await?;
                anyhow::ensure!(
                    n > 0 && line.ends_with('\n'),
                    "Diagnostic stream ended or event exceeds limit"
                );
                let mut event: E = serde_json::from_str(&line).context(outdated)?;
                if done(&mut event) {
                    return Ok::<_, anyhow::Error>(event);
                }
                timeout(
                    Duration::from_secs(2),
                    astra_ipc::send_management(
                        output,
                        &Reply::text(serde_json::to_string(&event)?),
                    ),
                )
                .await??;
            }
            anyhow::bail!("Diagnostic event count exceeds limit")
        };
        let ((), errors, last) = tokio::try_join!(write, errors, read)?;
        let status = child.wait().await?;
        anyhow::ensure!(
            status.success(),
            "Diagnostic core failed: {}",
            String::from_utf8_lossy(&errors)
        );
        timeout(
            Duration::from_secs(2),
            astra_ipc::send_management(output, &Reply::text(serde_json::to_string(&last)?)),
        )
        .await??;
        Ok(())
    };
    let result = timeout(Duration::from_secs(180), run)
        .await
        .context("Legacy diagnostic timed out")
        .and_then(|r| r);
    if result.is_err() {
        super::network::terminate(child).await?;
    }
    result
}
