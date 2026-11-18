//! Standalone executable copied to two paths by the signed macOS acceptance test.
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

fn expect_echo(result: io::Result<Vec<u8>>, payload: &[u8], blocked: bool) {
    match result {
        Ok(reply) => {
            assert!(!blocked, "received a reply through a blocked exit");
            assert_eq!(reply, payload);
        }
        Err(error) => assert!(blocked, "echo failed: {error}"),
    }
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let blocked = args[0] == "block";
    let timeout = Duration::from_secs(2);
    let peers: Vec<SocketAddr> = args[1..].iter().map(|s| s.parse().unwrap()).collect();
    assert_eq!(peers.len(), 2);
    let payload = format!("astra-{}", std::process::id()).into_bytes();
    let tcp = || -> io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect_timeout(&peers[0], timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(&payload)?;
        let mut reply = vec![0; payload.len()];
        stream.read_exact(&mut reply)?;
        Ok(reply)
    };
    expect_echo(tcp(), &payload, blocked);
    for connected in [true, false] {
        let socket = UdpSocket::bind(if peers[0].is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
        socket.set_read_timeout(Some(timeout))?;
        for address in peers.iter().take(if connected { 1 } else { peers.len() }) {
            let datagram = || -> io::Result<Vec<u8>> {
                if connected {
                    socket.connect(address)?;
                    socket.send(&payload)?;
                } else {
                    socket.send_to(&payload, address)?;
                }
                let mut buffer = [0; 256];
                let (size, peer) = socket.recv_from(&mut buffer)?;
                assert_eq!(peer, *address, "incorrect UDP reply peer");
                Ok(buffer[..size].to_vec())
            };
            expect_echo(datagram(), &payload, blocked);
        }
    }
    Ok(())
}
