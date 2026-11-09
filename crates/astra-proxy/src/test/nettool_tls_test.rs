use super::*;
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) fn fixture() -> (tokio_rustls::TlsAcceptor, TrustSnapshot) {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.cert.der().clone()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
        )
        .unwrap();
    (
        tokio_rustls::TlsAcceptor::from(Arc::new(config)),
        TrustSnapshot::fixture(roots),
    )
}
fn collect(events: &Mutex<Vec<Event>>, event: Event) -> Result<()> {
    assert!(serde_json::to_vec(&event).unwrap().len() < 56 * 1024);
    events.lock().unwrap().push(event);
    Ok(())
}
#[tokio::test]
async fn trusted_enterprise_fixture_succeeds_and_offline_bundled_difference_is_evidence_only() {
    let (acceptor, trust) = fixture();
    let state = TlsState::default();
    state.set_fixture(trust);
    let (client, server) = tokio::io::duplex(65536);
    let worker = tokio::spawn(async move {
        let mut tls = acceptor.accept(server).await.unwrap();
        tls.write_all(b"ok").await.unwrap();
    });
    let events = Mutex::new(vec![]);
    let mut tls = state
        .connect(1, "localhost", client, true, &|event| {
            collect(&events, event)
        })
        .await
        .unwrap();
    let mut bytes = [0; 2];
    tls.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"ok");
    worker.await.unwrap();
    let events = events.into_inner().unwrap();
    assert!(events.iter().any(|event|matches!(event,Event::Tls {detail,..} if matches!(detail.as_ref(),TlsDetail::Handshake {verified:true,bundled_comparison:Some(TrustComparison {verified:false,offline:true,..}),received_certificates:1,..}))));
    assert!(events.iter().any(|event|matches!(event,Event::Tls {detail,..} if matches!(detail.as_ref(),TlsDetail::Certificate {sha256,subject:Some(_),..} if sha256.len()==64))));
    assert!(events.iter().any(|event|matches!(event,Event::Tls {detail,..} if matches!(detail.as_ref(),TlsDetail::CertificateDer {base64,..} if !base64.is_empty()))));
}
#[tokio::test]
async fn verification_failure_keeps_peer_chain_and_never_sends_application_data() {
    for wrong_name in [false, true] {
        let (acceptor, trust) = fixture();
        let state = TlsState::default();
        state.set_fixture(if wrong_name {
            trust
        } else {
            TrustSnapshot::fixture(rustls::RootCertStore::empty())
        });
        let (client, server) = tokio::io::duplex(65536);
        let worker = tokio::spawn(async move {
            assert!(acceptor.accept(server).await.is_err());
        });
        let events = Mutex::new(vec![]);
        assert!(state
            .connect(
                1,
                if wrong_name {
                    "wrong.invalid"
                } else {
                    "localhost"
                },
                client,
                false,
                &|event| collect(&events, event)
            )
            .await
            .is_err());
        worker.await.unwrap();
        let events = events.into_inner().unwrap();
        assert!(events.iter().any(|event|matches!(event,Event::Tls {detail,..} if matches!(detail.as_ref(),TlsDetail::Handshake {verified:false,received_certificates:1,verification_error:Some(_),..}))));
        assert!(events.iter().any(|event|matches!(event,Event::Tls {detail,..} if matches!(detail.as_ref(),TlsDetail::CertificateDer {..}))));
    }
}
#[tokio::test]
async fn handshake_without_certificate_has_explicit_empty_evidence_and_cancels_cleanly() {
    let state = TlsState::default();
    state.set_fixture(TrustSnapshot::fixture(rustls::RootCertStore::empty()));
    let (client, mut server) = tokio::io::duplex(65536);
    let events = Mutex::new(vec![]);
    let worker = tokio::spawn(async move {
        let mut all = vec![];
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            server.read_to_end(&mut all),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!all.is_empty());
    });
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(50),
        state.connect(1, "localhost", client, false, &|event| collect(
            &events, event
        ))
    )
    .await
    .is_err());
    state
        .flush(1, "timeout", &|event| collect(&events, event))
        .unwrap();
    worker.await.unwrap();
    assert!(events.lock().unwrap().iter().any(|event|matches!(event,Event::Tls {detail,..} if matches!(detail.as_ref(),TlsDetail::Handshake {verified:false,received_certificates:0,retained_certificates:0,..}))));
}
#[test]
fn capture_keeps_at_most_sixteen_certificates_and_256k_der_with_bounded_events() {
    for size in [1024, 128 * 1024] {
        let capture = Arc::new(Mutex::new(Capture::default()));
        let verifier = RecordingVerifier {
            inner: None,
            comparison: None,
            capture: capture.clone(),
        };
        let certificate = CertificateDer::from(vec![0; size]);
        let chain = vec![certificate.clone(); 32];
        assert!(verifier
            .verify_server_cert(
                &certificate,
                &chain,
                &ServerName::try_from("localhost").unwrap(),
                &[],
                UnixTime::now()
            )
            .is_err());
        {
            let value = capture.lock().unwrap();
            assert!(value.truncated);
            assert_eq!(value.received, 33);
            assert!(value.chain.len() <= 16);
            assert!(value.retained_bytes <= 256 * 1024);
        }
        let state = TlsState::default();
        state
            .pending
            .lock()
            .unwrap()
            .push((1, "localhost".into(), Instant::now(), capture));
        let events = Mutex::new(vec![]);
        state
            .flush(1, "fixture failure", &|event| collect(&events, event))
            .unwrap();
        assert!(state.pending.lock().unwrap().is_empty());
    }
}
#[test]
fn native_snapshot_records_source_and_loading_errors_without_bundled_substitution() {
    let snapshot = TrustSnapshot::native();
    assert!(snapshot.source.contains("rustls-native-certs"));
    if snapshot.roots.is_empty() {
        assert!(!snapshot.errors.is_empty());
        assert!(snapshot.verifier().is_none());
    }
    assert!(
        matches!(snapshot.detail(),TlsDetail::Trust {policy,..} if policy.contains("no bundled-root fallback"))
    );
}

#[tokio::test]
async fn both_normal_handshake_signature_checks_are_delegated_and_cannot_be_bypassed() {
    #[derive(Debug)]
    struct Fixed(Arc<rustls::sign::CertifiedKey>);
    impl rustls::server::ResolvesServerCert for Fixed {
        fn resolve(
            &self,
            _: rustls::server::ClientHello<'_>,
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            Some(self.0.clone())
        }
    }
    for tls12 in [false, true] {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let wrong_key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let key = rustls::crypto::aws_lc_rs::sign::any_supported_type(
            &PrivatePkcs8KeyDer::from(wrong_key.signing_key.serialize_der()).into(),
        )
        .unwrap();
        let resolver = Fixed(Arc::new(rustls::sign::CertifiedKey::new(
            vec![cert.cert.der().clone()],
            key,
        )));
        let versions = if tls12 {
            vec![&rustls::version::TLS12]
        } else {
            vec![&rustls::version::TLS13]
        };
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_protocol_versions(&versions)
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let (client, server) = tokio::io::duplex(65536);
        let worker = tokio::spawn(async move {
            assert!(acceptor.accept(server).await.is_err());
        });
        let state = TlsState::default();
        state.set_fixture(TrustSnapshot::fixture(roots));
        let events = Mutex::new(vec![]);
        assert!(state
            .connect(1, "localhost", client, false, &|event| collect(
                &events, event
            ))
            .await
            .is_err());
        worker.await.unwrap();
        assert!(events.lock().unwrap().iter().any(|event|matches!(event,Event::Tls {detail,..} if matches!(detail.as_ref(),TlsDetail::Handshake {verified:false,verification_attempted:true,verification_error:Some(error),..} if error.contains("Handshake signature")))));
    }
}
