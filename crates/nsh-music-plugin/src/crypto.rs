//! Pure-Rust cryptographic primitives exposed to JS/TS plugins via `host.crypto`.
//!
//! Zero OpenSSL or C library dependencies so every plugin works identically on
//! macOS, Windows, Linux, iOS, Android, and HarmonyOS (OHOS).

use aes::cipher::{Array, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Aes192, Aes256};
use md5::{Digest, Md5};
use thiserror::Error;

/// Errors returned by pure-Rust crypto operations.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("invalid AES key length {0} bytes (expected 16, 24, or 32 bytes)")]
    InvalidKeyLength(usize),
    #[error("invalid AES IV length {0} bytes (expected 16 bytes)")]
    InvalidIvLength(usize),
    #[error("ciphertext length {0} is not a positive multiple of 16 bytes")]
    InvalidCiphertextLength(usize),
    #[error("invalid PKCS#7 padding")]
    InvalidPadding,
    #[error("invalid base64 input: {0}")]
    InvalidBase64(String),
    #[error("invalid hex input: {0}")]
    InvalidHex(String),
}

/// Computes MD5 digest bytes.
pub fn md5_bytes(data: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&out);
    buf
}

/// Computes lowercase hex MD5 digest.
pub fn md5_hex(data: &[u8]) -> String {
    hex::encode(md5_bytes(data))
}

// Pure-Rust FIPS-180-4 SHA-256 (zero external OpenSSL / C dependencies).
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const SHA256_H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Computes SHA-256 digest bytes (FIPS-180-4).
pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut state = SHA256_H0;
    let bit_len = (data.len() as u64).wrapping_mul(8);

    let mut padded = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut out = [0u8; 32];
    for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(state.iter()) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Computes lowercase hex SHA-256 digest.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256_bytes(data))
}

/// Computes HMAC-SHA256 digest bytes (RFC 2104).
pub fn hmac_sha256_bytes(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut block_key = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hashed = sha256_bytes(key);
        block_key[..32].copy_from_slice(&hashed);
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(BLOCK_SIZE + data.len());
    for b in &block_key {
        inner.push(b ^ 0x36);
    }
    inner.extend_from_slice(data);
    let inner_hash = sha256_bytes(&inner);

    let mut outer = Vec::with_capacity(BLOCK_SIZE + 32);
    for b in &block_key {
        outer.push(b ^ 0x5c);
    }
    outer.extend_from_slice(&inner_hash);
    sha256_bytes(&outer)
}

/// Computes lowercase hex HMAC-SHA256 digest.
pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    hex::encode(hmac_sha256_bytes(key, data))
}

fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad_len = 16 - (data.len() % 16);
    let mut out = Vec::with_capacity(data.len() + pad_len);
    out.extend_from_slice(data);
    out.resize(data.len() + pad_len, pad_len as u8);
    out
}

fn pkcs7_unpad(data: &mut Vec<u8>) -> Result<(), CryptoError> {
    let Some(&last) = data.last() else {
        return Err(CryptoError::InvalidPadding);
    };
    let pad_len = usize::from(last);
    if pad_len == 0 || pad_len > 16 || pad_len > data.len() {
        return Err(CryptoError::InvalidPadding);
    }
    if !data[data.len() - pad_len..].iter().all(|&b| b == last) {
        return Err(CryptoError::InvalidPadding);
    }
    data.truncate(data.len() - pad_len);
    Ok(())
}

fn encrypt_blocks_in_place(buf: &mut [u8], key: &[u8]) -> Result<(), CryptoError> {
    match key.len() {
        16 => {
            let mut k = [0u8; 16];
            k.copy_from_slice(key);
            let cipher = Aes128::new(&Array::from(k));
            for chunk in buf.as_chunks_mut::<16>().0 {
                let mut block = [0u8; 16];
                block.copy_from_slice(chunk);
                let mut arr = Array::from(block);
                cipher.encrypt_block(&mut arr);
                chunk.copy_from_slice(&arr);
            }
            Ok(())
        }
        24 => {
            let mut k = [0u8; 24];
            k.copy_from_slice(key);
            let cipher = Aes192::new(&Array::from(k));
            for chunk in buf.as_chunks_mut::<16>().0 {
                let mut block = [0u8; 16];
                block.copy_from_slice(chunk);
                let mut arr = Array::from(block);
                cipher.encrypt_block(&mut arr);
                chunk.copy_from_slice(&arr);
            }
            Ok(())
        }
        32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(key);
            let cipher = Aes256::new(&Array::from(k));
            for chunk in buf.as_chunks_mut::<16>().0 {
                let mut block = [0u8; 16];
                block.copy_from_slice(chunk);
                let mut arr = Array::from(block);
                cipher.encrypt_block(&mut arr);
                chunk.copy_from_slice(&arr);
            }
            Ok(())
        }
        other => Err(CryptoError::InvalidKeyLength(other)),
    }
}

fn decrypt_blocks_in_place(buf: &mut [u8], key: &[u8]) -> Result<(), CryptoError> {
    match key.len() {
        16 => {
            let mut k = [0u8; 16];
            k.copy_from_slice(key);
            let cipher = Aes128::new(&Array::from(k));
            for chunk in buf.as_chunks_mut::<16>().0 {
                let mut block = [0u8; 16];
                block.copy_from_slice(chunk);
                let mut arr = Array::from(block);
                cipher.decrypt_block(&mut arr);
                chunk.copy_from_slice(&arr);
            }
            Ok(())
        }
        24 => {
            let mut k = [0u8; 24];
            k.copy_from_slice(key);
            let cipher = Aes192::new(&Array::from(k));
            for chunk in buf.as_chunks_mut::<16>().0 {
                let mut block = [0u8; 16];
                block.copy_from_slice(chunk);
                let mut arr = Array::from(block);
                cipher.decrypt_block(&mut arr);
                chunk.copy_from_slice(&arr);
            }
            Ok(())
        }
        32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(key);
            let cipher = Aes256::new(&Array::from(k));
            for chunk in buf.as_chunks_mut::<16>().0 {
                let mut block = [0u8; 16];
                block.copy_from_slice(chunk);
                let mut arr = Array::from(block);
                cipher.decrypt_block(&mut arr);
                chunk.copy_from_slice(&arr);
            }
            Ok(())
        }
        other => Err(CryptoError::InvalidKeyLength(other)),
    }
}

/// Encrypts `plaintext` with AES-ECB + PKCS#7 padding (128, 192, or 256-bit key).
pub fn aes_ecb_encrypt(plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut buf = pkcs7_pad(plaintext);
    encrypt_blocks_in_place(&mut buf, key)?;
    Ok(buf)
}

/// Decrypts `ciphertext` with AES-ECB + PKCS#7 unpadding (128, 192, or 256-bit key).
pub fn aes_ecb_decrypt(ciphertext: &[u8], key: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err(CryptoError::InvalidCiphertextLength(ciphertext.len()));
    }
    let mut buf = ciphertext.to_vec();
    decrypt_blocks_in_place(&mut buf, key)?;
    pkcs7_unpad(&mut buf)?;
    Ok(buf)
}

/// Encrypts `plaintext` with AES-CBC + PKCS#7 padding (128, 192, or 256-bit key, 16-byte IV).
pub fn aes_cbc_encrypt(plaintext: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if iv.len() != 16 {
        return Err(CryptoError::InvalidIvLength(iv.len()));
    }
    let mut buf = pkcs7_pad(plaintext);
    let mut prev = [0u8; 16];
    prev.copy_from_slice(iv);
    for chunk in buf.as_chunks_mut::<16>().0 {
        for i in 0..16 {
            chunk[i] ^= prev[i];
        }
        encrypt_blocks_in_place(chunk, key)?;
        prev.copy_from_slice(chunk);
    }
    Ok(buf)
}

/// Decrypts `ciphertext` with AES-CBC + PKCS#7 unpadding (128, 192, or 256-bit key, 16-byte IV).
pub fn aes_cbc_decrypt(ciphertext: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if iv.len() != 16 {
        return Err(CryptoError::InvalidIvLength(iv.len()));
    }
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err(CryptoError::InvalidCiphertextLength(ciphertext.len()));
    }
    let mut buf = ciphertext.to_vec();
    let mut prev = [0u8; 16];
    prev.copy_from_slice(iv);
    for chunk in buf.as_chunks_mut::<16>().0 {
        let mut ct = [0u8; 16];
        ct.copy_from_slice(chunk);
        decrypt_blocks_in_place(chunk, key)?;
        for i in 0..16 {
            chunk[i] ^= prev[i];
        }
        prev = ct;
    }
    pkcs7_unpad(&mut buf)?;
    Ok(buf)
}

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode_base64_table(data: &[u8], table: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(table[((triple >> 18) & 0x3f) as usize] as char);
        out.push(table[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(table[((triple >> 6) & 0x3f) as usize] as char);
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(table[(triple & 0x3f) as usize] as char);
        } else if pad {
            out.push('=');
        }
    }
    out
}

/// Encodes bytes as standard Base64 (RFC 4648 with `=` padding).
pub fn base64_encode(data: &[u8]) -> String {
    encode_base64_table(data, B64_STD, true)
}

/// Encodes bytes as URL-safe Base64 without padding.
pub fn base64_url_encode(data: &[u8]) -> String {
    encode_base64_table(data, B64_URL, false)
}

fn b64_val(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Decodes standard or URL-safe Base64 (ignoring ASCII whitespace, tolerant of omitted padding).
pub fn base64_decode(input: &str) -> Result<Vec<u8>, CryptoError> {
    let cleaned: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if cleaned.is_empty() {
        return Ok(Vec::new());
    }
    let unpadded = match cleaned.iter().position(|&b| b == b'=') {
        Some(idx) => {
            if !cleaned[idx..].iter().all(|&b| b == b'=') {
                return Err(CryptoError::InvalidBase64(
                    "unexpected non-padding character after `=`".to_owned(),
                ));
            }
            &cleaned[..idx]
        }
        None => &cleaned[..],
    };
    if unpadded.len() % 4 == 1 {
        return Err(CryptoError::InvalidBase64(
            "invalid base64 length".to_owned(),
        ));
    }

    let mut out = Vec::with_capacity((unpadded.len() * 3) / 4);
    let mut i = 0;
    while i < unpadded.len() {
        let rem = unpadded.len() - i;
        let v0 = b64_val(unpadded[i])
            .ok_or_else(|| CryptoError::InvalidBase64(format!("invalid byte at index {i}")))?;
        let v1 = b64_val(unpadded[i + 1]).ok_or_else(|| {
            CryptoError::InvalidBase64(format!("invalid byte at index {}", i + 1))
        })?;
        out.push((v0 << 2) | (v1 >> 4));

        if rem >= 3 {
            let v2 = b64_val(unpadded[i + 2]).ok_or_else(|| {
                CryptoError::InvalidBase64(format!("invalid byte at index {}", i + 2))
            })?;
            out.push((v1 << 4) | (v2 >> 2));
            if rem >= 4 {
                let v3 = b64_val(unpadded[i + 3]).ok_or_else(|| {
                    CryptoError::InvalidBase64(format!("invalid byte at index {}", i + 3))
                })?;
                out.push((v2 << 6) | v3);
            }
        }
        i += 4;
    }
    Ok(out)
}

/// Encodes bytes as lowercase hexadecimal.
pub fn hex_encode(data: &[u8]) -> String {
    hex::encode(data)
}

/// Decodes a hexadecimal string into bytes.
pub fn hex_decode(input: &str) -> Result<Vec<u8>, CryptoError> {
    hex::decode(input.trim()).map_err(|e| CryptoError::InvalidHex(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_and_sha256_standard_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"hello world"), "5eb63bbbe01eeed093cb22bb8f5acdc3");

        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_vectors() {
        // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?"
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Key longer than 64 bytes
        let long_key = vec![0xaa; 131];
        assert_eq!(
            hmac_sha256_hex(
                &long_key,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn aes_ecb_128_192_256_roundtrip() {
        let msg = b"nsh-music netease eapi payload 1234567890!";
        for key in [
            b"0123456789abcdef".as_slice(),
            b"0123456789abcdef01234567".as_slice(),
            b"0123456789abcdef0123456789abcdef".as_slice(),
        ] {
            let ct = aes_ecb_encrypt(msg, key).expect("encrypt");
            assert_eq!(ct.len() % 16, 0);
            let pt = aes_ecb_decrypt(&ct, key).expect("decrypt");
            assert_eq!(pt, msg);
        }
        assert!(matches!(
            aes_ecb_encrypt(msg, b"short"),
            Err(CryptoError::InvalidKeyLength(5))
        ));
    }

    #[test]
    fn aes_cbc_roundtrip() {
        let key = b"0123456789abcdef";
        let iv = b"fedcba9876543210";
        let ct = aes_cbc_encrypt(b"hello cbc world", key, iv).expect("cbc encrypt");
        let pt = aes_cbc_decrypt(&ct, key, iv).expect("cbc decrypt");
        assert_eq!(pt, b"hello cbc world");
    }

    #[test]
    fn base64_std_and_url_roundtrip() {
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_decode("Zm9vYmFy").expect("decode"), b"foobar");
        assert_eq!(base64_decode("Zg==").expect("decode"), b"f");
        assert_eq!(base64_decode("Zg").expect("unpadded decode"), b"f");

        let raw = vec![0xfb, 0xff, 0xfe, 0x00, 0x12];
        let u = base64_url_encode(&raw);
        assert!(!u.contains('+') && !u.contains('/') && !u.contains('='));
        assert_eq!(base64_decode(&u).expect("url decode"), raw);
    }
}
