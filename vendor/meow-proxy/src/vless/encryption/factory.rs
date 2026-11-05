//! Parse the VLESS `encryption` string into a [`ClientInstance`].
//!
//! Port of mihomo `transport/vless/encryption/factory.go` `NewClient`. The
//! string has the form:
//!
//! ```text
//! mlkem768x25519plus.<native|xorpub|random>.<1rtt|0rtt>[.<padding>…].<KEY>[.<KEY>…]
//! ```
//!
//! where each `KEY` is base64 (raw-url) of a 32-byte X25519 public key or a
//! 1184-byte ML-KEM-768 encapsulation key, and the optional `padding` tokens
//! are the short (`<20` char) dot-separated fragments before the keys.

use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::{alphabet, Engine};

use super::aead::{MLKEM768_EK_LEN, X25519_LEN};
use super::client::ClientInstance;

/// Raw-URL base64 decoder matching Go's `base64.RawURLEncoding.DecodeString`,
/// which mihomo/Xray use for the key tokens. Crucially, Go's default decoder is
/// lenient about a final symbol's unused trailing bits; the `base64` crate's
/// `URL_SAFE_NO_PAD` is strict and would reject otherwise-valid 32-byte keys
/// (e.g. the exact key in issue #301). Allow trailing bits to interoperate.
fn key_decoder() -> GeneralPurpose {
    GeneralPurpose::new(
        &alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::RequireNone)
            .with_decode_allow_trailing_bits(true),
    )
}

/// Parse a client-side `encryption` value.
///
/// - `""` / `"none"` → `Ok(None)` (no encryption layer).
/// - `mlkem768x25519plus.…` → `Ok(Some(instance))`.
/// - anything else → `Err(_)`.
pub fn parse_client_encryption(encryption: &str) -> Result<Option<ClientInstance>, String> {
    match encryption {
        "" | "none" => return Ok(None),
        _ => {}
    }

    let s: Vec<&str> = encryption.split('.').collect();
    if s.len() < 4 || s[0] != "mlkem768x25519plus" {
        return Err(format!("invalid vless encryption value: {encryption}"));
    }

    let xor_mode = match s[1] {
        "native" => 0u32,
        "xorpub" => 1,
        "random" => 2,
        _ => return Err(format!("invalid vless encryption value: {encryption}")),
    };
    let seconds = match s[2] {
        "1rtt" => 0u32,
        "0rtt" => 1,
        _ => return Err(format!("invalid vless encryption value: {encryption}")),
    };

    let mut nfs_keys_bytes: Vec<Vec<u8>> = Vec::new();
    let mut paddings: Vec<&str> = Vec::new();
    for r in &s[3..] {
        // Short tokens are padding parameters; long tokens are base64 keys.
        if r.len() < 20 {
            paddings.push(r);
            continue;
        }
        let b = key_decoder()
            .decode(r)
            .map_err(|_| format!("invalid vless encryption value: {encryption}"))?;
        if b.len() != X25519_LEN && b.len() != MLKEM768_EK_LEN {
            return Err(format!("invalid vless encryption value: {encryption}"));
        }
        nfs_keys_bytes.push(b);
    }
    let padding = paddings.join(".");

    let instance = ClientInstance::init(nfs_keys_bytes, xor_mode, seconds, &padding)
        .map_err(|e| format!("failed to use encryption: {e}"))?;
    Ok(Some(instance))
}

#[cfg(test)]
#[path = "../../test/vless_encryption_factory_test.rs"]
mod tests;
