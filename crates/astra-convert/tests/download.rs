#![cfg(feature = "download")]
use astra_convert::subscription::{diagnostics, download, Subscription};
use std::{
    io::{Read, Write},
    net::TcpListener,
};

#[test]
fn mihomo_headers_survive_redirects_and_chunked_gzip_finishes_before_connection_close() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (finished, download_finished) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        for hop in 0..=5 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap().to_lowercase();
            assert!(request.starts_with(&format!("get /{hop}?token=private ")));
            assert!(request.contains("user-agent: mihomo/1.19.0\r\n"));
            assert!(request.contains("accept: */*\r\n"));
            assert!(request.contains("accept-encoding: gzip, deflate\r\n"));
            if hop < 5 {
                write!(stream, "HTTP/1.1 302 Found\r\nLocation: /{}?token=private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", hop + 1).unwrap();
            } else {
                let data = [
                    31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 203, 203, 79, 73, 45, 6, 0, 252, 5, 61, 29,
                    5, 0, 0, 0,
                ];
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n").unwrap();
                for chunk in data.chunks(12) {
                    write!(stream, "{:x}\r\n", chunk.len()).unwrap();
                    stream.write_all(chunk).unwrap();
                    stream.write_all(b"\r\n").unwrap();
                }
                stream.write_all(b"0\r\n\r\n").unwrap();
                // A complete chunked response must finish while the TCP connection
                // remains open. The timeout bounds a regression that waits for EOF.
                download_finished
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("download waited for a keep-alive connection to close");
            }
        }
    });
    let text = download(
        &format!("http://{address}/0?token=private"),
        &Subscription::default().user_agent,
    );
    finished.send(()).unwrap();
    server.join().unwrap();
    assert_eq!(text.unwrap(), "nodes");
}

#[test]
fn rejects_redirect_credentials_before_connecting_and_hides_error_body() {
    for response in [
        "HTTP/1.1 302 Found\r\nLocation: http://user:private@127.0.0.1:1/sub\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 11\r\nConnection: close\r\n\r\nbody-secret",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            stream.write_all(response.as_bytes()).unwrap();
        });
        let error = download(&format!("http://{address}/private?token=secret"), "custom-UA").unwrap_err();
        let diagnostic = diagnostics::error(&error);
        assert!(!diagnostic.contains("private") && !diagnostic.contains("body-secret") && !diagnostic.contains("token=secret"), "{diagnostic}");
        if response.starts_with("HTTP/1.1 403") {
            assert!(diagnostic.contains("HTTP 403"), "{diagnostic}");
        } else {
            assert!(diagnostic.contains("credentials"), "{diagnostic}");
        }
        server.join().unwrap();
    }
}
