//! Native loopback load generator. Both proxy versions use this same executable.
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

async fn echo(single: bool) -> Result<(u16, JoinHandle<()>)> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((mut socket, _)) = accepted else { break };
                    tasks.spawn(async move {
                        socket.set_nodelay(true)?;
                        if single {
                            let mut bytes = [0; 1024];
                            socket.read_exact(&mut bytes).await?;
                            socket.write_all(&bytes).await?;
                        } else {
                            let mut bytes = vec![0; 65536];
                            loop {
                                let n = socket.read(&mut bytes).await?;
                                if n == 0 { break }
                                socket.write_all(&bytes[..n]).await?;
                            }
                        }
                        socket.shutdown().await
                    });
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
    });
    Ok((port, task))
}

async fn connect(proxy: u16, target: u16) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, proxy)).await?;
    stream.set_nodelay(true)?;
    stream.write_all(&[5, 1, 0]).await?;
    let mut greeting = [0; 2];
    stream.read_exact(&mut greeting).await?;
    ensure!(greeting == [5, 0], "SOCKS greeting rejected");
    let mut request = [5, 1, 0, 1, 127, 0, 0, 1, 0, 0];
    request[8..].copy_from_slice(&target.to_be_bytes());
    stream.write_all(&request).await?;
    let mut reply = [0; 10];
    stream.read_exact(&mut reply).await?;
    ensure!(reply[1] == 0 && reply[3] == 1, "SOCKS CONNECT rejected");
    Ok(stream)
}

fn quantiles(mut values: Vec<f64>) -> Value {
    values.sort_by(f64::total_cmp);
    let at = |p: usize| values[(values.len() * p).div_ceil(100) - 1];
    json!({"p50":at(50), "p95":at(95), "p99":at(99)})
}

async fn connections(proxy: u16, target: u16, count: usize, concurrency: usize) -> Result<Value> {
    let mut tasks = JoinSet::new();
    let started = Instant::now();
    for worker in 0..concurrency {
        tasks.spawn(async move {
            let mut samples = Vec::with_capacity(count / concurrency + 1);
            for _ in (worker..count).step_by(concurrency) {
                let began = Instant::now();
                let mut socket = connect(proxy, target).await?;
                let connected = began.elapsed().as_secs_f64() * 1000.;
                let payload = [0x5a; 1024];
                socket.write_all(&payload).await?;
                let mut bytes = [0; 1024];
                socket.read_exact(&mut bytes).await?;
                ensure!(bytes == payload, "echo payload mismatch");
                let echoed = began.elapsed().as_secs_f64() * 1000.;
                ensure!(
                    socket.read(&mut bytes[..1]).await? == 0,
                    "unexpected echo data"
                );
                samples.push((
                    connected,
                    echoed,
                    began.elapsed().as_secs_f64() * 1000. - echoed,
                ));
            }
            Ok::<_, anyhow::Error>(samples)
        });
    }
    let mut all = Vec::with_capacity(count);
    while let Some(samples) = tasks.join_next().await {
        all.extend(samples??);
    }
    let elapsed = started.elapsed().as_secs_f64();
    Ok(
        json!({"throughput_connections_per_second":count as f64/elapsed,
        "setup_ms":quantiles(all.iter().map(|r|r.0).collect()),
        "echo_ms":quantiles(all.iter().map(|r|r.1).collect()),
        "close_wait_ms":quantiles(all.iter().map(|r|r.2).collect())}),
    )
}

pub async fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let proxy = args[2].parse()?;
    let concurrency: usize = args[3].parse()?;
    let count: usize = args[4].parse()?;
    let seconds: u64 = args[5].parse()?;
    ensure!(concurrency > 0 && count >= concurrency);
    let (target, short_server) = echo(true).await?;
    connections(proxy, target, 512, concurrency).await?;
    let mut report = connections(proxy, target, count, concurrency).await?;
    short_server.abort();
    let _ = short_server.await;
    if seconds > 0 {
        let (target, stream_server) = echo(false).await?;
        let mut channels = Vec::new();
        for _ in 0..4 {
            channels.push(connect(proxy, target).await?);
        }
        let began = Instant::now();
        let deadline = began + Duration::from_secs(seconds);
        let mut tasks = JoinSet::new();
        for mut socket in channels {
            tasks.spawn(async move {
                let data = vec![0x5a; 65536];
                let mut bytes = vec![0; data.len()];
                let mut transferred = 0_u64;
                while Instant::now() < deadline {
                    socket.write_all(&data).await?;
                    socket.read_exact(&mut bytes).await?;
                    ensure!(bytes == data, "stream payload mismatch");
                    transferred += data.len() as u64;
                }
                Ok::<_, anyhow::Error>(transferred)
            });
        }
        let mut total = 0;
        while let Some(transferred) = tasks.join_next().await {
            total += transferred??;
        }
        report["throughput_mib_per_second"] =
            json!(total as f64 / 1048576. / began.elapsed().as_secs_f64());
        stream_server.abort();
        let _ = stream_server.await;
    }
    println!("{report}");
    Ok(())
}
