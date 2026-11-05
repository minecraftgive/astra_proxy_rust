//! Record-layer AEAD for Snell, VMess and VLESS encryption.
//!
//! Every relayed byte of those protocols passes through here, so the cipher
//! is BoringSSL's (the library meow-transport already links), not a generic
//! Rust AEAD: the release profile's `opt-level = "z"` leaves generic cipher
//! code ~25× slower than BoringSSL's assembly (issue #659).
//!
//! All three algorithms share a 12-byte nonce and a 16-byte tag.

use boring::aead::{AeadCtx, Algorithm};

/// Nonce length of every algorithm here.
pub(crate) const NONCE_LEN: usize = 12;
/// Tag length of every algorithm here.
pub(crate) const TAG_LEN: usize = 16;

/// AEAD failure: a tag that does not authenticate, or a sealed buffer too
/// short to hold one.
#[derive(Debug)]
pub(crate) struct AeadError;

/// One key's AEAD context. The key schedule lives behind BoringSSL's context
/// pointer, so this is pointer-sized.
pub(crate) struct Aead(AeadCtx);

impl Aead {
    /// AES-128-GCM; `key` must be 16 bytes.
    #[cfg(any(feature = "snell", feature = "vmess"))]
    pub(crate) fn aes_128_gcm(key: &[u8]) -> Self {
        Self::new(&Algorithm::aes_128_gcm(), key)
    }

    /// AES-256-GCM; `key` must be 32 bytes.
    #[cfg(feature = "vless-encryption")]
    pub(crate) fn aes_256_gcm(key: &[u8]) -> Self {
        Self::new(&Algorithm::aes_256_gcm(), key)
    }

    /// ChaCha20-Poly1305; `key` must be 32 bytes.
    #[cfg(any(feature = "vmess", feature = "vless-encryption"))]
    pub(crate) fn chacha20_poly1305(key: &[u8]) -> Self {
        Self::new(&Algorithm::chacha20_poly1305(), key)
    }

    /// Panics on a wrong-length key: every caller derives the key at a fixed
    /// length, so a mismatch is a bug, not input to handle.
    fn new(algorithm: &Algorithm, key: &[u8]) -> Self {
        Self(
            AeadCtx::new_default_tag(algorithm, key)
                .expect("AEAD key length matches the algorithm"),
        )
    }

    /// Encrypt `data` in place under `nonce` and `ad`, writing the tag to
    /// `tag` (which must be [`TAG_LEN`] bytes).
    pub(crate) fn seal_detached(
        &self,
        nonce: &[u8; NONCE_LEN],
        ad: &[u8],
        data: &mut [u8],
        tag: &mut [u8],
    ) -> Result<(), AeadError> {
        debug_assert_eq!(tag.len(), TAG_LEN);
        self.0
            .seal_in_place(nonce, data, tag, ad)
            .map(|_| ())
            .map_err(|_| AeadError)
    }

    /// Verify `tag` and decrypt `data` in place under `nonce` and `ad`.
    pub(crate) fn open_detached(
        &self,
        nonce: &[u8; NONCE_LEN],
        ad: &[u8],
        data: &mut [u8],
        tag: &[u8],
    ) -> Result<(), AeadError> {
        self.0
            .open_in_place(nonce, data, tag, ad)
            .map_err(|_| AeadError)
    }

    /// Encrypt `buf` in place under `nonce` and `ad` and append the tag.
    pub(crate) fn seal_append(
        &self,
        nonce: &[u8; NONCE_LEN],
        ad: &[u8],
        buf: &mut Vec<u8>,
    ) -> Result<(), AeadError> {
        let len = buf.len();
        buf.resize(len + TAG_LEN, 0);
        let (data, tag) = buf.split_at_mut(len);
        self.seal_detached(nonce, ad, data, tag)
    }

    /// Verify and strip the trailing tag of `buf`, decrypting the rest in
    /// place under `nonce` and `ad`.
    pub(crate) fn open_trailing(
        &self,
        nonce: &[u8; NONCE_LEN],
        ad: &[u8],
        buf: &mut Vec<u8>,
    ) -> Result<(), AeadError> {
        let len = buf.len().checked_sub(TAG_LEN).ok_or(AeadError)?;
        let (data, tag) = buf.split_at_mut(len);
        self.open_detached(nonce, ad, data, tag)?;
        buf.truncate(len);
        Ok(())
    }
}

#[cfg(test)]
#[path = "test/aead_test.rs"]
mod tests;
