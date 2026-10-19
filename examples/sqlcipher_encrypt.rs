//! Test helper for scripts/smoke.py, which builds it: copy a plain SQLite database into a
//! new SQLCipher database, keyed the way Signal Desktop keys its own, or with
//! `--plaintext-header` the way Telegram's Postbox does: its first 32 bytes unencrypted.
//!
//! sqlcipher_encrypt PLAIN ENCRYPTED HEX_KEY [--plaintext-header]
use rusqlite::Connection;

fn main() -> rusqlite::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (plain, encrypted, key, header) = match args.as_slice() {
        [plain, encrypted, key] => (plain, encrypted, key, false),
        [plain, encrypted, key, flag] if flag == "--plaintext-header" => {
            (plain, encrypted, key, true)
        }
        _ => {
            eprintln!("usage: sqlcipher_encrypt PLAIN ENCRYPTED HEX_KEY [--plaintext-header]");
            std::process::exit(2)
        }
    };
    assert!(
        key.bytes().all(|b| b.is_ascii_hexdigit()),
        "the key must be hex"
    );
    let db = Connection::open(plain)?;
    if header {
        db.execute_batch("PRAGMA cipher_default_plaintext_header_size = 32;")?;
    }
    db.execute(
        &format!("ATTACH DATABASE ?1 AS encrypted KEY \"x'{key}'\""),
        [encrypted],
    )?;
    db.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))?;
    db.execute("DETACH DATABASE encrypted", [])?;
    Ok(())
}
