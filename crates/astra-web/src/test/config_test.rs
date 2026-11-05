use super::*;
#[test]
fn cleartext_cannot_escape_loopback() {
    let mut c = Config {
        manager_socket: "m.sock".into(),
        state_dir: "s".into(),
        credentials_file: None,
        listen: listen(),
        public_origin: "http://127.0.0.1:8787".into(),
        tls: None,
        dev_http: true,
    };
    assert!(c.validate().is_ok());
    c.listen = "0.0.0.0:8787".parse().unwrap();
    assert!(c.validate().is_err());
    c.tls = Some(Tls {
        certificate: "cert.pem".into(),
        private_key: "key.pem".into(),
    });
    c.public_origin = "https://astra.example".into();
    assert!(c.validate().is_ok());
    c.public_origin = "https://astra.example/other".into();
    assert!(c.validate().is_err());
}
