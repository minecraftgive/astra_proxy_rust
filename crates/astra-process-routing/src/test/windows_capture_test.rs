use super::*;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::process::Command;
use std::time::Duration;

#[test]
fn child_connect() {
    let Some(tcp) = std::env::var_os("ASTRA_WFP_TEST_TCP") else {
        return;
    };
    let endpoint: SocketAddr = tcp.to_str().unwrap().parse().unwrap();
    let udp_ports = std::env::var("ASTRA_WFP_TEST_UDP").unwrap();
    let peers: Vec<SocketAddr> = udp_ports.split(',').map(|s| s.parse().unwrap()).collect();
    let blocked = std::env::var("ASTRA_WFP_TEST_EXPECT").unwrap() == "block";
    let check = |result: io::Result<()>| {
        if blocked {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        } else {
            result.unwrap();
        }
    };
    check(TcpStream::connect_timeout(&endpoint, Duration::from_secs(3)).map(drop));
    let bind = if endpoint.is_ipv4() {
        "127.0.0.1:0"
    } else {
        "[::1]:0"
    };
    let connected = UdpSocket::bind(bind).unwrap();
    check(
        connected
            .connect(peers[0])
            .and_then(|_| connected.send(b"connected").map(drop)),
    );
    let socket = UdpSocket::bind(bind).unwrap();
    for peer in peers {
        check(socket.send_to(b"sendto", peer).map(drop));
    }
}

#[test]
#[ignore = "requires elevated native Windows and the Base Filtering Engine"]
fn native_block_lease() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let mut binaries = Vec::new();
    for name in ["selected", "control"] {
        let directory = root.path().join(name);
        std::fs::create_dir(&directory)?;
        let path = directory.join("client.exe");
        std::fs::copy(std::env::current_exe()?, &path)?;
        binaries.push(path);
    }
    for ip in [
        IpAddr::from([127, 0, 0, 1]),
        IpAddr::from(std::net::Ipv6Addr::LOCALHOST),
    ] {
        let tcp = TcpListener::bind((ip, 0))?;
        let first = UdpSocket::bind((ip, 0))?;
        let second = UdpSocket::bind((ip, 0))?;
        let probe = |path: &Path, expect: &str| -> io::Result<()> {
            let status = Command::new(path)
                .args(["--exact", "windows::tests::child_connect", "--nocapture"])
                .env("ASTRA_WFP_TEST_TCP", tcp.local_addr()?.to_string())
                .env(
                    "ASTRA_WFP_TEST_UDP",
                    format!("{},{}", first.local_addr()?, second.local_addr()?),
                )
                .env("ASTRA_WFP_TEST_EXPECT", expect)
                .status()?;
            assert!(status.success());
            Ok(())
        };
        let lease = BlockLease::acquire(&binaries[0])?;
        probe(&binaries[0], "block")?;
        probe(&binaries[1], "allow")?;
        drop(lease);
        probe(&binaries[0], "allow")?;
    }
    Ok(())
}

#[test]
fn tcp_child() {
    let Ok(endpoint) = std::env::var("ASTRA_WFP_TCP_DESTINATION") else {
        return;
    };
    let mut stream =
        TcpStream::connect_timeout(&endpoint.parse().unwrap(), Duration::from_secs(3)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(b"probe").unwrap();
    let mut response = [0; 5];
    stream.read_exact(&mut response).unwrap();
    assert_eq!(response, *b"probe");
}

#[test]
#[ignore = "requires elevated native Windows and the signed AstraRoute TCP callout"]
fn native_tcp_redirect() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let mut programs = Vec::new();
    for dir in ["bound", "control"] {
        let path = root.path().join(dir).join("client.exe");
        std::fs::create_dir(path.parent().unwrap())?;
        std::fs::copy(std::env::current_exe()?, &path)?;
        programs.push(path);
    }
    let mut redirector = TcpRedirector::open()?;
    for ip in [
        IpAddr::from([127, 0, 0, 1]),
        IpAddr::from(std::net::Ipv6Addr::LOCALHOST),
    ] {
        let destination = TcpListener::bind((ip, 0))?;
        let broker = TcpListener::bind((ip, 0))?;
        broker.set_nonblocking(true)?;
        destination.set_nonblocking(true)?;
        redirector.set(
            &programs[0],
            std::process::id().try_into().unwrap(),
            broker.local_addr()?.port().try_into().unwrap(),
        )?;
        let probe = |path: &Path, capture: bool| -> io::Result<()> {
            let intended = destination.local_addr()?;
            let mut child = Command::new(path)
                .args(["--exact", "windows::tests::tcp_child", "--nocapture"])
                .env("ASTRA_WFP_TCP_DESTINATION", intended.to_string())
                .spawn()?;
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            let mut served = false;
            loop {
                for (listener, redirected) in [(&broker, true), (&destination, false)] {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            assert_eq!(
                                redirected,
                                capture,
                                "unexpected listener for {}",
                                path.display()
                            );
                            stream.set_nonblocking(false)?;
                            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                            if redirected {
                                let context = RedirectContext::read(&stream)?;
                                assert_eq!(context.destination, intended);
                                assert_eq!(context.source_pid, child.id());
                            }
                            let mut bytes = [0; 5];
                            stream.read_exact(&mut bytes)?;
                            stream.write_all(&bytes)?;
                            served = true;
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(error) => return Err(error),
                    }
                }
                if let Some(status) = child.try_wait()? {
                    assert!(status.success() && served);
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    child.kill()?;
                    child.wait()?;
                    return Err(io::ErrorKind::TimedOut.into());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        probe(&programs[0], true)?;
        probe(&programs[1], false)?;
        redirector.remove(&programs[0])?;
        probe(&programs[0], false)?;
    }
    Ok(())
}
