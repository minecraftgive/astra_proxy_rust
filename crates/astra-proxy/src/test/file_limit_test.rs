//! Exercise the executable with the 256-FD soft limit used by desktop launchers.
#![cfg(unix)]

use anyhow::{ensure, Result};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[tokio::test]
async fn mixed_listener_serves_beyond_inherited_file_limit() -> Result<()> {
    let root = std::env::temp_dir().join(format!("astra-fd-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root)?;
    let config = root.join("proxy.toml");
    let control = root.join("core.sock");
    let reserved = TcpListener::bind("127.0.0.1:0").await?;
    let address = reserved.local_addr()?;
    drop(reserved);
    let echo = TcpListener::bind("127.0.0.1:0").await?;
    let target = echo.local_addr()?;
    std::fs::write(&config, format!(
        "standalone=true\nstandalone_outbound='direct'\nmixed_listen='{address}'\nshutdown_timeout_secs=1\n[proxy_groups.direct]\nmembers=['DIRECT']\n"
    ))?;
    let log = std::fs::File::create(root.join("core.log"))?;
    let mut command = astra_platform::platform::background_core(std::path::Path::new(env!(
        "CARGO_BIN_EXE_astra-proxy"
    )));
    command
        .arg(&config)
        .env("ASTRA_CONTROL_SOCKET", &control)
        .env("ASTRA_RUNTIME_DIR", root.join("runtime"))
        .env("ASTRA_LOG_LEVEL", "info")
        .env_remove("ASTRA_HOSTED_EXPERIMENTAL")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true);
    // SAFETY: the child hook only calls async-signal-safe resource-limit APIs.
    unsafe {
        command.pre_exec(|| {
            let mut limits = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limits.rlim_cur = 256;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limits) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(async move {
        let mut sessions = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = echo.accept() => {
                    let (mut socket, _) = accepted?;
                    sessions.spawn(async move {
                        let (mut read, mut write) = socket.split();
                        tokio::io::copy(&mut read, &mut write).await
                    });
                }
                Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    });
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if astra_ipc::control::request(&control, astra_model::control::Command::Status)
                .await
                .is_ok()
            {
                break;
            }
            ensure!(child.try_wait()?.is_none(), "proxy exited before ready");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let mut clients = Vec::new();
        // 160 live direct tunnels require at least 320 FDs in the proxy.
        for _ in 0..160 {
            let mut socket = TcpStream::connect(address).await?;
            socket
                .write_all(
                    format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes(),
                )
                .await?;
            let mut response = Vec::new();
            while !response.ends_with(b"\r\n\r\n") {
                response.push(socket.read_u8().await?);
                ensure!(response.len() <= 1024, "oversized CONNECT response");
            }
            ensure!(
                response.starts_with(b"HTTP/1.1 200"),
                "CONNECT failed: {response:?}"
            );
            clients.push(socket);
        }
        for client in &mut clients {
            client.write_all(b"ok").await?;
            let mut response = [0; 2];
            client.read_exact(&mut response).await?;
            ensure!(&response == b"ok", "tunnel payload changed");
        }
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    servers.abort_all();
    let logs = std::fs::read_to_string(root.join("core.log"))?;
    std::fs::remove_dir_all(root)?;
    result
        .map_err(|error| anyhow::anyhow!("{error}; proxy log:\n{logs}"))?
        .map_err(|error| anyhow::anyhow!("{error:#}; proxy log:\n{logs}"))?;
    ensure!(
        logs.contains("raised file descriptor limit"),
        "missing limit diagnostic: {logs}"
    );
    ensure!(
        !logs.contains("Too many open files"),
        "proxy exhausted descriptors: {logs}"
    );
    Ok(())
}
