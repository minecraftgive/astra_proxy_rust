//! Dependency-free native load generator. Build with rustc -O.
use std::{
    env,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    },
    thread,
    time::{Duration, Instant},
};

fn address(stream: &mut TcpStream) -> io::Result<(String, u16)> {
    let mut kind = [0];
    stream.read_exact(&mut kind)?;
    let n = match kind[0] {
        1 => 4,
        4 => 16,
        3 => {
            stream.read_exact(&mut kind)?;
            kind[0] as usize
        }
        _ => return Err(io::Error::other("bad address")),
    };
    let mut host = vec![0; n];
    stream.read_exact(&mut host)?;
    let mut port = [0; 2];
    stream.read_exact(&mut port)?;
    Ok((
        String::from_utf8_lossy(&host).into_owned(),
        u16::from_be_bytes(port),
    ))
}
fn serve_one(mut stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut header = [0; 2];
    stream.read_exact(&mut header)?;
    let mut methods = vec![0; header[1] as usize];
    stream.read_exact(&mut methods)?;
    stream.write_all(&[5, 0])?;
    let mut request = [0; 3];
    stream.read_exact(&mut request)?;
    let (host, port) = address(&mut stream)?;
    if request[1] == 3 {
        // The TCP control channel remains idle while UDP is active.
        stream.set_read_timeout(None)?;
        let udp = UdpSocket::bind("127.0.0.1:0")?;
        udp.set_read_timeout(Some(Duration::from_millis(100)))?;
        let port = udp.local_addr()?.port().to_be_bytes();
        stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, port[0], port[1]])?;
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let job = thread::spawn(move || {
            let mut buf = vec![0; 65536];
            while !stop.load(Ordering::Relaxed) {
                if let Ok((n, peer)) = udp.recv_from(&mut buf) {
                    let _ = udp.send_to(&buf[..n], peer);
                }
            }
        });
        let _ = stream.read(&mut [0]);
        done.store(true, Ordering::Relaxed);
        let _ = job.join();
    } else {
        stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])?;
        if host == "system-proxy.astra.invalid" {
            let mut remote = TcpStream::connect(("127.0.0.1", port))?;
            remote.set_nodelay(true)?;
            let mut source = stream.try_clone()?;
            let mut target = remote.try_clone()?;
            let upload = thread::spawn(move || {
                let _ = io::copy(&mut source, &mut target);
                let _ = target.shutdown(std::net::Shutdown::Write);
            });
            let _ = io::copy(&mut remote, &mut stream);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let _ = upload.join();
        } else {
            let mut buf = vec![0; 65536];
            loop {
                let n = stream.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                stream.write_all(&buf[..n])?;
            }
        }
    }
    Ok(())
}
fn socks(host: &str, port: u16, udp: bool) -> io::Result<(TcpStream, u16)> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(&[5, 1, 0])?;
    let mut pair = [0; 2];
    stream.read_exact(&mut pair)?;
    if pair != [5, 0] {
        return Err(io::Error::other("SOCKS authentication failed"));
    }
    if udp {
        stream.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0])?;
    } else {
        stream.write_all(&[5, 1, 0, 1, 198, 19, 253, 2, 0x46, 0xa1])?;
    }
    let mut reply = [0; 10];
    stream.read_exact(&mut reply)?;
    if reply[1] != 0 || reply[3] != 1 {
        return Err(io::Error::other("SOCKS connection failed"));
    }
    Ok((stream, u16::from_be_bytes([reply[8], reply[9]])))
}
fn tcp(kind: &str, host: &str, port: u16) -> io::Result<TcpStream> {
    if kind == "socks" {
        return Ok(socks(host, port, false)?.0);
    }
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    if kind == "connect" {
        stream.write_all(
            b"CONNECT 198.19.253.2:18081 HTTP/1.1\r\nHost: 198.19.253.2:18081\r\n\r\n",
        )?;
        let mut response = vec![];
        while !response.ends_with(b"\r\n\r\n") {
            let mut b = [0];
            stream.read_exact(&mut b)?;
            response.push(b[0]);
            if response.len() > 4096 {
                return Err(io::Error::other("oversized header"));
            }
        }
        if !response.starts_with(b"HTTP/1.1 200") {
            return Err(io::Error::other("CONNECT failed"));
        }
    }
    Ok(stream)
}
fn one(
    kind: String,
    host: String,
    port: u16,
    size: usize,
    seconds: f64,
    gate: Arc<Barrier>,
) -> io::Result<(u64, Vec<f64>)> {
    let body = vec![0x5a; size];
    let mut samples = Vec::new();
    let mut count = 0;
    if kind == "udp" || kind == "socks-udp" {
        let (control, relay) = if kind == "socks-udp" {
            let (s, p) = socks(&host, port, true)?;
            (Some(s), p)
        } else {
            (None, port)
        };
        let udp = UdpSocket::bind(if host.contains(':') {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        })?;
        udp.connect((host.as_str(), relay))?;
        udp.set_read_timeout(Some(Duration::from_secs(3)))?;
        let header = if control.is_some() {
            vec![0, 0, 0, 1, 198, 19, 253, 2, 0x46, 0xa2]
        } else {
            vec![]
        };
        let mut outgoing = header.clone();
        outgoing.extend_from_slice(&body);
        let mut response = vec![0; 65536];
        gate.wait();
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        while Instant::now() < deadline {
            let start = Instant::now();
            udp.send(&outgoing)?;
            let n = udp.recv(&mut response).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "UDP receive: local {:?}, remote {:?}, size {size}, completed {count}: {e}",
                        udp.local_addr(),
                        udp.peer_addr()
                    ),
                )
            })?;
            if response[..n] != outgoing {
                return Err(io::Error::other("UDP payload mismatch"));
            }
            if count % 16 == 0 {
                samples.push(start.elapsed().as_secs_f64() * 1000.)
            }
            count += 1;
        }
        drop(control);
    } else {
        let mut stream = tcp(&kind, &host, port)?;
        let mut writer = stream.try_clone()?;
        // Read concurrently with writes: 1 MiB messages must not deadlock socket buffers.
        let (begin, starts) = std::sync::mpsc::sync_channel(1);
        let (ack, acks) = std::sync::mpsc::sync_channel(1);
        let job = thread::spawn(move || -> io::Result<()> {
            while acks.recv().is_ok() {
                let start = Instant::now();
                if begin.send(start).is_err() {
                    break;
                }
                writer.write_all(&body)?;
            }
            Ok(())
        });
        let mut response = vec![0; size];
        gate.wait();
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        while Instant::now() < deadline {
            ack.send(())
                .map_err(|_| io::Error::other("writer stopped"))?;
            let start = starts
                .recv()
                .map_err(|_| io::Error::other("writer stopped"))?;
            stream.read_exact(&mut response)?;
            if response.iter().any(|b| *b != 0x5a) {
                return Err(io::Error::other("TCP payload mismatch"));
            }
            if count % 16 == 0 {
                samples.push(start.elapsed().as_secs_f64() * 1000.)
            }
            count += 1;
        }
        drop(ack);
        job.join()
            .map_err(|_| io::Error::other("writer panicked"))??;
    }
    Ok((count, samples))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<_> = env::args().collect();
    if a[1] == "serve" {
        let listener = TcpListener::bind(("127.0.0.1", a[2].parse::<u16>()?))?;
        println!("READY");
        io::stdout().flush()?;
        for s in listener.incoming() {
            let stream = s?;
            thread::spawn(move || {
                let _ = serve_one(stream);
            });
        }
        return Ok(());
    }
    let kind = &a[2];
    let host = &a[3];
    let port = a[4].parse::<u16>()?;
    let concurrency = a[5].parse::<usize>()?;
    let size = a[6].parse::<usize>()?;
    let seconds = a[7].parse::<f64>()?;
    let gate = Arc::new(Barrier::new(concurrency + 1));
    let mut jobs = vec![];
    for _ in 0..concurrency {
        let (k, h, g) = (kind.clone(), host.clone(), gate.clone());
        jobs.push(thread::spawn(move || one(k, h, port, size, seconds, g)));
    }
    gate.wait();
    let start = Instant::now();
    let mut count = 0;
    let mut samples = vec![];
    for job in jobs {
        let (n, mut values) = job
            .join()
            .map_err(|_| io::Error::other("worker panicked"))??;
        count += n;
        samples.append(&mut values);
    }
    let wall = start.elapsed().as_secs_f64();
    samples.sort_by(|a, b| a.total_cmp(b));
    let percentile = |p: f64| samples[((samples.len() - 1) as f64 * p).ceil() as usize];
    println!("{{\"messages\":{count},\"bytes_one_direction\":{},\"seconds\":{wall},\"messages_per_second\":{},\"mib_per_second_one_direction\":{},\"latency_p50_ms\":{},\"latency_p95_ms\":{},\"latency_p99_ms\":{},\"latency_samples\":{}}}",count*size as u64,count as f64/wall,(count*size as u64) as f64/wall/1048576.,percentile(0.5),percentile(0.95),percentile(0.99),samples.len());
    Ok(())
}
