mod config;
mod protocols;
mod proxy;
mod transports;
use anyhow::{Context, Result};
use astra_protocols::wire;
use protocols::encryption;
use protocols::multiplex;
use protocols::salamander;
use protocols::vision;
use proxy::net;
use proxy::protocol;
use proxy::quic;
use proxy::runtime;
use std::path::PathBuf;
use transports::security;
use transports::transport;
use transports::xhttp;

pub async fn run() -> Result<()> {
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut args = std::env::args().skip(1);
    let mut config = PathBuf::from("server.toml");
    let mut control = default_control();
    let mut action = None;
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = args.next().context("--config requires path")?.into(),
            "--control" => control = args.next().context("--control requires path")?.into(),
            "--check" => check = true,
            "--generate-reality-keys" => {
                use base64::Engine;
                let private = rand::random::<[u8; 32]>();
                let secret = x25519_dalek::StaticSecret::from(private);
                let public = x25519_dalek::PublicKey::from(&secret);
                println!(
                    "{}",
                    serde_json::json!({"server_private_key":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(private),"client_public_key":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public.as_bytes())})
                );
                return Ok(());
            }
            "--generate-decryption-keys" => {
                use base64::Engine;
                use meow_proxy::vless_server_decryption::ServerKey;
                let kind = args.next().unwrap_or_else(|| "x25519".into());
                let keys = match kind.as_str() {
                    "x25519" => vec![ServerKey::new_x25519()],
                    "mlkem768" => vec![ServerKey::new_mlkem()],
                    "hybrid" => vec![ServerKey::new_x25519(), ServerKey::new_mlkem()],
                    _ => anyhow::bail!("key type must be x25519/mlkem768/hybrid"),
                };
                let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
                println!(
                    "{}",
                    serde_json::json!({"server_private_keys":keys.iter().map(|k|b64.encode(k.private_bytes())).collect::<Vec<_>>(),"client_public_keys":keys.iter().map(|k|b64.encode(k.public_bytes())).collect::<Vec<_>>()})
                );
                return Ok(());
            }
            "--status" => action = Some(astra_server_api::Action::Status),
            "--stop" => action = Some(astra_server_api::Action::Stop),
            "--reload" => action = Some(astra_server_api::Action::Reload { path: None }),
            "--generate-cert" => {
                let dir = PathBuf::from(
                    args.next()
                        .context("--generate-cert requires new directory")?,
                );
                let name = args.next().unwrap_or_else(|| "localhost".into());
                std::fs::create_dir(&dir)?;
                astra_ipc::platform::set_private(&dir, 0o700)?;
                let rcgen::CertifiedKey { cert, signing_key } =
                    rcgen::generate_simple_self_signed(vec![name])?;
                std::fs::write(dir.join("cert.pem"), cert.pem())?;
                std::fs::write(dir.join("key.pem"), signing_key.serialize_pem())?;
                astra_ipc::platform::set_private(&dir.join("key.pem"), 0o600)?;
                println!("Certificate: {}", dir.join("cert.pem").display());
                return Ok(());
            }
            "--init" => {
                let path = PathBuf::from(
                    args.next()
                        .context("--init requires new configuration path")?,
                );
                let id = uuid::Uuid::new_v4();
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                astra_ipc::platform::set_private(&path, 0o600)?;
                use std::io::Write;
                write!(file,"# Bind to loopback until TLS and firewall settings are ready.\n[[listeners]]\ntag = \"vless\"\nprotocol = \"vless\"\nlisten = \"127.0.0.1:24443\"\nallow_plaintext = true\n[[listeners.users]]\nname = \"default\"\nuuid = \"{id}\"\n")?;
                println!("Created {}", path.display());
                return Ok(());
            }
            "--help" | "-h" => {
                println!("astra-server-core [--config server.toml] [--control PATH]\n  --init PATH                  Create private configuration with random UUID\n  --generate-cert NEW_DIR DNS   Generate a local TLS certificate\n  --generate-reality-keys      Server private / client public key JSON\n  --generate-decryption-keys TYPE  x25519 / mlkem768 / hybrid\n  --check                      Validate server configuration without binding\n  --status | --stop | --reload  Manage existing server");
                return Ok(());
            }
            _ => anyhow::bail!("unknown option {arg}"),
        }
    }
    if let Some(action) = action {
        let r = astra_server_api::call(&control, action).await?;
        println!(
            "pid={} uptime={}s active={} accepted={} rejected={} upload={} download={}\n{}",
            r.pid,
            r.uptime_secs,
            r.active,
            r.accepted,
            r.rejected,
            r.upload,
            r.download,
            r.listeners.join("\n")
        );
        return Ok(());
    }
    let config = std::fs::canonicalize(config)?;
    let prepared = config::load(&config)?;
    if check {
        println!("Valid: {} listeners", prepared.config.listeners.len());
        return Ok(());
    }
    runtime::run(config, control, prepared).await
}
pub fn default_control() -> PathBuf {
    astra_ipc::platform::home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".astra-server")
        .join("control.sock")
}
