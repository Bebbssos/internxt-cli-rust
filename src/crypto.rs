//! Crypto primitives ported 1:1 from og/cli crypto.service.ts, og/lib aes,
//! and og/inxt-js crypto utils. Byte-for-byte compatible with the node CLI.

use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit, StreamCipher};
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use md5::Md5;
use ripemd::Ripemd160;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::config;

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;

/// pbkdf2(password, salt, 10000, 32, sha1) -> (salt_hex, hash_hex)
pub fn pass_to_hash(password: &str, salt_hex: Option<&str>) -> Result<(String, String)> {
    let salt = match salt_hex {
        Some(s) => hex::decode(s)?,
        None => {
            let mut b = [0u8; 16];
            rand::RngCore::fill_bytes(&mut rand::rng(), &mut b);
            b.to_vec()
        }
    };
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha1>(password.as_bytes(), &salt, 10000, &mut out);
    Ok((hex::encode(&salt), hex::encode(out)))
}

/// OpenSSL EVP_BytesToKey (MD5, 1 iteration) used by CryptoJS for AES-256-CBC.
fn key_and_iv(secret: &str, salt: &[u8]) -> ([u8; 32], [u8; 16]) {
    let mut password = secret.as_bytes().to_vec();
    password.extend_from_slice(salt);

    let mut md5_hashes: Vec<[u8; 16]> = Vec::with_capacity(3);
    let mut digest = password.clone();
    for _ in 0..3 {
        let h: [u8; 16] = Md5::digest(&digest).into();
        md5_hashes.push(h);
        digest = h.to_vec();
        digest.extend_from_slice(&password);
    }

    let mut key = [0u8; 32];
    key[..16].copy_from_slice(&md5_hashes[0]);
    key[16..].copy_from_slice(&md5_hashes[1]);
    (key, md5_hashes[2])
}

/// CryptoJS-compatible AES-256-CBC encrypt. Output hex of: "Salted__" + salt(8) + ciphertext.
pub fn encrypt_text_with_key(text: &str, secret: &str) -> String {
    let mut salt = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut salt);
    let (key, iv) = key_and_iv(secret, &salt);

    let ct = Aes256CbcEnc::new_from_slices(&key, &iv)
        .expect("valid AES-256-CBC key/iv length")
        .encrypt_padded_vec::<cbc::cipher::block_padding::Pkcs7>(text.as_bytes());

    let mut out = b"Salted__".to_vec();
    out.extend_from_slice(&salt);
    out.extend_from_slice(&ct);
    hex::encode(out)
}

/// CryptoJS-compatible AES-256-CBC decrypt of hex string.
pub fn decrypt_text_with_key(encrypted_hex: &str, secret: &str) -> Result<String> {
    let data = hex::decode(encrypted_hex)?;
    if data.len() < 16 {
        return Err(anyhow!("ciphertext too short"));
    }
    let salt = &data[8..16];
    let (key, iv) = key_and_iv(secret, salt);
    let pt = Aes256CbcDec::new_from_slices(&key, &iv)
        .expect("valid AES-256-CBC key/iv length")
        .decrypt_padded_vec::<cbc::cipher::block_padding::Pkcs7>(&data[16..])
        .map_err(|e| anyhow!("cbc decrypt failed: {e}"))?;
    Ok(String::from_utf8(pt)?)
}

pub fn encrypt_text(text: &str) -> String {
    encrypt_text_with_key(text, &config::app_crypto_secret())
}

pub fn decrypt_text(encrypted_hex: &str) -> Result<String> {
    decrypt_text_with_key(encrypted_hex, &config::app_crypto_secret())
}

/// og/lib aes.decrypt: base64 [salt64][iv16][tag16][ciphertext], pbkdf2-sha512 2145 rounds, AES-256-GCM.
pub fn decrypt_private_key(private_key_b64: &str, password: &str) -> Result<String> {
    const MIN_LEN: usize = 129;
    if private_key_b64.len() <= MIN_LEN {
        return Ok(String::new());
    }
    let data = B64.decode(private_key_b64)?;
    if data.len() < 96 {
        return Err(anyhow!("private key too short"));
    }
    let salt = &data[0..64];
    let iv = &data[64..80];
    let tag = &data[80..96];
    let ct = &data[96..];

    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha512>(password.as_bytes(), salt, 2145, &mut key);

    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| anyhow!("gcm key: {e}"))?;
    let mut buf = ct.to_vec();
    buf.extend_from_slice(tag);
    let pt = cipher
        .decrypt(Nonce::from_slice(iv), buf.as_ref())
        .map_err(|_| anyhow!("Private key is corrupted"))?;
    Ok(String::from_utf8(pt)?)
}

pub fn sha256(input: &[u8]) -> Vec<u8> {
    Sha256::digest(input).to_vec()
}

pub fn ripemd160(input: &[u8]) -> Vec<u8> {
    Ripemd160::digest(input).to_vec()
}

/// Network basic-auth password = sha256(userId).hex
pub fn network_password(user_id: &str) -> String {
    hex::encode(sha256(user_id.as_bytes()))
}

fn mnemonic_to_seed(mnemonic: &str) -> Result<[u8; 64]> {
    let m = bip39::Mnemonic::parse_normalized(mnemonic.trim())
        .map_err(|e| anyhow!("invalid mnemonic: {e}"))?;
    Ok(m.to_seed(""))
}

pub fn validate_mnemonic(mnemonic: &str) -> bool {
    bip39::Mnemonic::parse_normalized(mnemonic.trim()).is_ok()
}

/// GenerateFileKey(mnemonic, bucketId, index) -> 32-byte AES key.
pub fn generate_file_key(mnemonic: &str, bucket_id: &str, index: &[u8]) -> Result<[u8; 32]> {
    let seed = mnemonic_to_seed(mnemonic)?;
    let bucket_id_bytes = hex::decode(bucket_id)?;

    // bucketKey = sha512(seed || bucketIdBytes)
    let mut h = Sha512::new();
    h.update(seed);
    h.update(&bucket_id_bytes);
    let bucket_key = h.finalize();

    // fileKey = sha512(bucketKey[0..32] || index)[0..32]
    let mut h2 = Sha512::new();
    h2.update(&bucket_key[0..32]);
    h2.update(index);
    let file_key = h2.finalize();

    let mut key = [0u8; 32];
    key.copy_from_slice(&file_key[0..32]);
    Ok(key)
}

/// In-place AES-256-CTR (encrypt == decrypt). iv must be 16 bytes.
pub fn aes256ctr_apply(key: &[u8; 32], iv: &[u8], data: &mut [u8]) {
    let mut cipher =
        Aes256Ctr::new_from_slices(key, iv).expect("valid AES-256-CTR key/iv length");
    cipher.apply_keystream(data);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "6KYQBP847D4ATSFA";
    const MNEMONIC: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";
    const BUCKET: &str = "0123456789abcdef0123456789abcdef";
    const INDEX_HEX: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    // Values produced by scripts/ref.js (node, identical algorithm).
    #[test]
    fn matches_node_decrypt() {
        let enc = "53616c7465645f5f00112233445566774556e47b00ca7ba4959bea3b6abdf0be";
        assert_eq!(decrypt_text_with_key(enc, SECRET).unwrap(), "hello world");
    }

    #[test]
    fn matches_node_pass_hash() {
        let (_, hash) = pass_to_hash("mypassword", Some("deadbeef")).unwrap();
        assert_eq!(
            hash,
            "c949a136c21c1b44b76e0c1d7e7f7178b7beb595d4fc18add5e4c6d01f980306"
        );
    }

    #[test]
    fn matches_node_file_key() {
        let index = hex::decode(INDEX_HEX).unwrap();
        let key = generate_file_key(MNEMONIC, BUCKET, &index).unwrap();
        assert_eq!(
            hex::encode(key),
            "ef27f0d3bbfe4b6d013c890b102af2df8c7cf3dcebe5683c54c71647f426e8cb"
        );
    }

    #[test]
    fn matches_node_ctr() {
        let index = hex::decode(INDEX_HEX).unwrap();
        let key = generate_file_key(MNEMONIC, BUCKET, &index).unwrap();
        let mut data = b"the quick brown fox".to_vec();
        aes256ctr_apply(&key, &index[0..16], &mut data);
        assert_eq!(hex::encode(&data), "9373448f118b254274c2ad6d5eccd424afc228");
    }

    #[test]
    fn matches_node_shard_hash() {
        let h = ripemd160(&sha256(b"encrypted-shard-content"));
        assert_eq!(hex::encode(h), "e1c941ff3e3e9f79d932941f88e8d3c833bc8e5d");
    }

    #[test]
    fn cbc_roundtrip() {
        let enc = encrypt_text_with_key("round trip me", SECRET);
        assert_eq!(decrypt_text_with_key(&enc, SECRET).unwrap(), "round trip me");
    }
}

/// Streaming AES-256-CTR state for chunked encrypt/decrypt.
pub struct Ctr(Aes256Ctr);

impl Ctr {
    pub fn new(key: &[u8; 32], iv: &[u8]) -> Self {
        Ctr(Aes256Ctr::new_from_slices(key, iv).expect("valid AES-256-CTR key/iv length"))
    }
    pub fn apply(&mut self, data: &mut [u8]) {
        self.0.apply_keystream(data);
    }
}
