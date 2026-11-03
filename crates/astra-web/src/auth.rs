use anyhow::{ensure, Context, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

const SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const MAX_SESSIONS: usize = 64;
pub struct Auth {
    password_file: PathBuf,
    sessions: Mutex<HashMap<[u8; 32], Session>>,
    attempts: Mutex<HashMap<IpAddr, (Instant, u8)>>,
    logins: tokio::sync::Semaphore,
}
struct Session {
    expires: Instant,
    credential: [u8; 32],
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("secure random source: {e}"))?;
    Ok(bytes)
}
pub fn set_password(path: &Path, password: &str) -> Result<()> {
    ensure!(
        (12..=1024).contains(&password.len()),
        "administrator password must contain 12–1024 bytes"
    );
    let salt =
        SaltString::encode_b64(&random::<16>()?).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("password hash: {e}"))?
        .to_string();
    astra_platform::storage::prepare_parent(path)?;
    // The shared private atomic writer supplies the same owner/permission policy as runtime state.
    astra_platform::storage::write_json(path, &hash).context("write administrator credential")
}
impl Auth {
    pub fn new(password_file: PathBuf) -> Result<Self> {
        let hash: String = serde_json::from_slice(&astra_platform::storage::read_bounded(&password_file,16*1024).context("administrator is not configured; run astra-web --config <file> --set-password --password-stdin")?)?;
        PasswordHash::new(&hash)
            .map_err(|_| anyhow::anyhow!("invalid administrator credential"))?;
        Ok(Self {
            password_file,
            sessions: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
            logins: tokio::sync::Semaphore::new(2),
        })
    }
    pub fn permit_attempt(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut attempts = self.attempts.lock().unwrap();
        attempts.retain(|_, (at, _)| now.duration_since(*at) < LOGIN_WINDOW);
        if attempts.len() >= 1024 && !attempts.contains_key(&ip) {
            return false;
        }
        let entry = attempts.entry(ip).or_insert((now, 0));
        entry.1 = entry.1.saturating_add(1);
        entry.1 <= 5
    }
    pub async fn login(&self, password: String) -> Result<String> {
        ensure!(password.len() <= 1024, "invalid credentials");
        let _permit = self
            .logins
            .try_acquire()
            .map_err(|_| anyhow::anyhow!("login capacity exceeded"))?;
        let bytes = astra_platform::storage::read_bounded(&self.password_file, 16 * 1024)?;
        let credential = digest(&bytes);
        let hash: String = serde_json::from_slice(&bytes)?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let hash =
                PasswordHash::new(&hash).map_err(|_| anyhow::anyhow!("invalid credentials"))?;
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .map_err(|_| anyhow::anyhow!("invalid credentials"))
        })
        .await??;
        let token = hex::encode(random::<32>()?);
        let now = Instant::now();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, s| s.expires > now && s.credential == credential);
        ensure!(
            sessions.len() < MAX_SESSIONS,
            "session capacity exceeded; log out an existing session"
        );
        sessions.insert(
            digest(token.as_bytes()),
            Session {
                expires: now + SESSION_TTL,
                credential,
            },
        );
        Ok(token)
    }
    pub fn verify(&self, token: &str) -> bool {
        if token.len() != 64 {
            return false;
        }
        let Ok(bytes) = astra_platform::storage::read_bounded(&self.password_file, 16 * 1024)
        else {
            return false;
        };
        let credential = digest(&bytes);
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now && s.credential == credential);
        sessions.contains_key(&digest(token.as_bytes()))
    }
    pub fn logout(&self, token: &str) {
        self.sessions
            .lock()
            .unwrap()
            .remove(&digest(token.as_bytes()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn credentials_sessions_revocation_and_password_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("admin.phc");
        assert!(Auth::new(file.clone()).is_err());
        set_password(&file, "a long test password").unwrap();
        let auth = Auth::new(file.clone()).unwrap();
        assert!(auth.login("wrong".into()).await.is_err());
        let token = auth.login("a long test password".into()).await.unwrap();
        assert!(auth.verify(&token));
        assert!(!auth.verify(&"0".repeat(64)));
        auth.logout(&token);
        assert!(!auth.verify(&token));
        let expired = auth.login("a long test password".into()).await.unwrap();
        auth.sessions
            .lock()
            .unwrap()
            .get_mut(&digest(expired.as_bytes()))
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert!(!auth.verify(&expired));
        let token = auth.login("a long test password".into()).await.unwrap();
        set_password(&file, "a different test password").unwrap();
        assert!(!auth.verify(&token));
        let restarted = Auth::new(file).unwrap();
        assert!(!restarted.verify(&token));
        for _ in 0..5 {
            assert!(auth.permit_attempt("127.0.0.1".parse().unwrap()));
        }
        assert!(!auth.permit_attempt("127.0.0.1".parse().unwrap()));
    }
}
