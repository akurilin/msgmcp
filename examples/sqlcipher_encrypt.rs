//! Test helper for scripts/smoke.py, which builds it: copy a plain SQLite database into a
//! new SQLCipher database, keyed the way Signal Desktop keys its own.
//!
//! sqlcipher_encrypt PLAIN ENCRYPTED HEX_KEY
use rusqlite::Connection;

fn main() -> rusqlite::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [plain, encrypted, key] = args.as_slice() else {
        eprintln!("usage: sqlcipher_encrypt PLAIN ENCRYPTED HEX_KEY");
        std::process::exit(2)
    };
    assert!(
        key.bytes().all(|b| b.is_ascii_hexdigit()),
        "the key must be hex"
    );
    let db = Connection::open(plain)?;
    db.execute(
        &format!("ATTACH DATABASE ?1 AS encrypted KEY \"x'{key}'\""),
        [encrypted],
    )?;
    db.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))?;
    db.execute("DETACH DATABASE encrypted", [])?;
    Ok(())
}
