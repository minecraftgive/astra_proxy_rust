use sha2::{Digest, Sha256};

/// SHA-256 block size, in bytes.
const BLOCK: usize = 64;

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Recursively-nested HMAC-SHA256 as used by the VMess AEAD KDF.
///
/// `keys[0]` is the innermost HMAC key and `keys[last]` the outermost; `msg`
/// is fed to the outermost HMAC. With a single key this is a plain
/// HMAC-SHA256; each additional key wraps another HMAC whose underlying hash
/// primitive is the *inner HMAC* (not raw SHA-256). This mirrors v2ray
/// `proxy/vmess/aead` `KDF` / `hMacCreator` exactly — a cascade (feeding each
/// digest as the next HMAC key) produces entirely different output and does
/// not interoperate.
fn nested_hmac(keys: &[&[u8]], msg: &[u8]) -> [u8; 32] {
    let Some((k, inner_keys)) = keys.split_last() else {
        return sha256(msg);
    };

    // Normalize the outermost key to the block size. Keys longer than the
    // block are first hashed by the parent hash; VMess never hits this (all
    // salts/derived inputs are < 64 bytes) but it keeps the primitive correct.
    let mut key_block = [0u8; BLOCK];
    if k.len() > BLOCK {
        key_block[..32].copy_from_slice(&nested_hmac(inner_keys, k));
    } else {
        key_block[..k.len()].copy_from_slice(k);
    }

    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = key_block[i] ^ 0x36;
        opad[i] = key_block[i] ^ 0x5c;
    }

    let mut inner_msg = Vec::with_capacity(BLOCK + msg.len());
    inner_msg.extend_from_slice(&ipad);
    inner_msg.extend_from_slice(msg);
    let inner_digest = nested_hmac(inner_keys, &inner_msg);

    let mut outer_msg = Vec::with_capacity(BLOCK + 32);
    outer_msg.extend_from_slice(&opad);
    outer_msg.extend_from_slice(&inner_digest);
    nested_hmac(inner_keys, &outer_msg)
}

/// VMess AEAD KDF — recursively-nested HMAC-SHA256 keyed by the fixed
/// `"VMess AEAD KDF"` salt (innermost) and each `path` segment, with `key`
/// fed as the message to the outermost HMAC.
///
/// upstream: `proxy/vmess/aead/kdf.go` (`KDF`)
pub fn kdf(key: &[u8], path: &[&[u8]]) -> [u8; 32] {
    let mut keys: Vec<&[u8]> = Vec::with_capacity(1 + path.len());
    keys.push(b"VMess AEAD KDF");
    keys.extend_from_slice(path);
    nested_hmac(&keys, key)
}

pub fn kdf16(key: &[u8], path: &[&[u8]]) -> [u8; 16] {
    let full = kdf(key, path);
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

pub fn kdf12(key: &[u8], path: &[&[u8]]) -> [u8; 12] {
    let full = kdf(key, path);
    let mut out = [0u8; 12];
    out.copy_from_slice(&full[..12]);
    out
}

#[cfg(test)]
#[path = "../test/vmess_kdf_test.rs"]
mod tests;
