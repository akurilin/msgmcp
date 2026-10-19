//! Test helper for scripts/smoke.py, which builds it: write Telegram's `.tempkeyEncrypted`,
//! the database key and salt that Telegram for macOS encrypts with the default passcode.
//!
//! telegram_tempkey OUT HEX_PLAINTEXT
//!
//! HEX_PLAINTEXT is the key, the salt, their hash and padding, a multiple of 16 bytes.
use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::NoPadding};
use sha2::{Digest, Sha512};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, plain] = args.as_slice() else {
        eprintln!("usage: telegram_tempkey OUT HEX_PLAINTEXT");
        std::process::exit(2)
    };
    let mut data: Vec<u8> = (0..plain.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&plain[i..i + 2], 16).expect("the plaintext must be hex"))
        .collect();
    assert!(
        data.len().is_multiple_of(16),
        "the plaintext must fill whole blocks"
    );
    let hash = Sha512::digest(b"no-matter-key");
    let length = data.len();
    let encrypted = cbc::Encryptor::<aes::Aes256>::new(hash[..32].into(), hash[48..].into())
        .encrypt_padded_mut::<NoPadding>(&mut data, length)
        .expect("whole blocks");
    std::fs::write(out, encrypted).expect("write the key file");
}
