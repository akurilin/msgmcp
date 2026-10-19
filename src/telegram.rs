//! Telegram for macOS backend (read-only).
//!
//! Telegram's native Mac app (the App Store and direct-download builds; Telegram Desktop,
//! the Qt app, keeps no history on disk) stores each account in a Postbox database:
//! SQLCipher, at `<build>/account-<id>/postbox/db/db_sqlite` in its group container, where
//! `<build>` is `appstore` or `stable`. `<build>/.tempkeyEncrypted` holds the database key,
//! AES-encrypted with a key derived from the local passcode, or from "no-matter-key" when
//! none is set. msgmcp reads Telegram only without a local passcode.
//!
//! Postbox tables are named `t<N>` and map keys (big-endian integers, or SQLite integers)
//! to little-endian binary values:
//! - t0: metadata. t2: peers (users, groups, channels, secret chats), keyed by peer ID,
//!   each a PostboxCoding object.
//! - t6: media shared by several messages, which messages reference by media ID.
//! - t7: messages, keyed by (peer ID, namespace, timestamp, message ID).
//! - t8: the chat list: each peer's newest message and folder (1 is the archive).
//! - t14: read state: unread counts and the newest read message, per peer and namespace.
//!
//! The database is a cache. Telegram fetches history from its servers as the user scrolls,
//! so older messages are often missing.
use crate::backend::Backend;
use crate::common::{Contacts, Filter, Listed, Page, contact_name, display_address, number, page};
use crate::types::{ChatType, Conversation, Kind, Message, Person, Reaction, Source, TextStatus};
use anyhow::{Context, Result, anyhow, bail};
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, functions::FunctionFlags, types::Value as Sql};
use sha2::{Digest, Sha512};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::SystemTime,
};

pub const PREFIX: &str = "telegram";

/// Telegram's group container.
struct Telegram(PathBuf);

pub fn backend(dir: PathBuf) -> Box<dyn Backend> {
    Box::new(Telegram(dir))
}

impl Backend for Telegram {
    fn check(&self) -> Result<()> {
        open(&self.0).map(drop)
    }
    fn conversations(
        &self,
        contacts: &Contacts,
        active_after: Option<DateTime<Utc>>,
    ) -> Result<Vec<Listed>> {
        conversations(&self.0, contacts, active_after)
    }
    fn messages(
        &self,
        contacts: &Contacts,
        filter: &Filter,
        cursor: Option<&str>,
        cap: usize,
        budget: usize,
    ) -> Result<Page> {
        messages(&self.0, contacts, filter, cursor, cap, budget)
    }
    fn context(
        &self,
        contacts: &Contacts,
        conversation: &str,
        message: &str,
        before: usize,
        after: usize,
    ) -> Result<Vec<Message>> {
        context(&self.0, contacts, conversation, message, (before, after))
    }
}

/// Build folders: the App Store build, then the direct download.
const BUILDS: [&str; 2] = ["appstore", "stable"];
const DATABASE: &str = "postbox/db/db_sqlite";
const DEFAULT_PASSCODE: &[u8] = b"no-matter-key";
/// Seed of Telegram's MurmurHash3, for the key check and for PostboxCoding type names.
const SEED: u32 = 0xF7CA_7FD2;

// Message namespaces read: cloud messages, and local and incoming secret-chat messages.
// The others hold scheduled messages, quick replies and other drafts.
const NAMESPACES: &str = "substr(key,9,4) IN (x'00000000',x'00000001',x'00000002')";
// A message key reordered as (timestamp, peer and namespace, ID): time order across chats.
// A row value, since SQLite's || would turn the blobs into text.
const ORDER: &str = "(substr(key,13,4),substr(key,1,12),substr(key,17,4))";

/// `ORDER` sorted one way.
fn order_by(direction: &str) -> String {
    format!(
        "substr(key,13,4) {direction}, substr(key,1,12) {direction}, substr(key,17,4) {direction}"
    )
}

/// A cursor in `ORDER` form as the three SQL values it compares with.
fn order_values(order: &[u8; 20]) -> [Sql; 3] {
    [
        Sql::Blob(order[..4].to_vec()),
        Sql::Blob(order[4..16].to_vec()),
        Sql::Blob(order[16..].to_vec()),
    ]
}
const ARCHIVE_GROUP: i32 = 1;

// Message flags.
const INCOMING: u32 = 4;
// User flags: Telegram marks the account as a scam or as impersonating someone.
const SCAM: i32 = 1 << 2;
const FAKE: i32 = 1 << 3;
/// The account Telegram sends login codes and service notifications from.
const SERVICE_NOTIFICATIONS: i64 = 777_000;

/// MurmurHash3 x86_32.
fn murmur3(data: &[u8], seed: u32) -> u32 {
    let (c1, c2) = (0xcc9e_2d51u32, 0x1b87_3593u32);
    let mut h = seed;
    let (chunks, tail) = data.as_chunks::<4>();
    for chunk in chunks {
        let k = u32::from_le_bytes(*chunk);
        h ^= k.wrapping_mul(c1).rotate_left(15).wrapping_mul(c2);
        h = h.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    if !tail.is_empty() {
        let k = tail
            .iter()
            .enumerate()
            .fold(0u32, |k, (i, b)| k | (u32::from(*b) << (8 * i)));
        h ^= k.wrapping_mul(c1).rotate_left(15).wrapping_mul(c2);
    }
    h ^= data.len() as u32;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^ (h >> 16)
}

/// PostboxCoding type hashes, from the Swift class names.
struct Types {
    user: i32,
    group: i32,
    channel: i32,
    secret_chat: i32,
    image: i32,
    file: i32,
    webpage: i32,
    action: i32,
    contact: i32,
    map: i32,
    reply: i32,
    reactions: i32,
}

static TYPES: LazyLock<Types> = LazyLock::new(|| {
    let hash = |name: &str| murmur3(name.as_bytes(), SEED) as i32;
    Types {
        user: hash("TelegramUser"),
        group: hash("TelegramGroup"),
        channel: hash("TelegramChannel"),
        secret_chat: hash("TelegramSecretChat"),
        image: hash("TelegramMediaImage"),
        file: hash("TelegramMediaFile"),
        webpage: hash("TelegramMediaWebpage"),
        action: hash("TelegramMediaAction"),
        contact: hash("TelegramMediaContact"),
        map: hash("TelegramMediaMap"),
        reply: hash("ReplyMessageAttribute"),
        reactions: hash("ReactionsMessageAttribute"),
    }
});

/// The most recently used account's database, and its build folder.
fn locate(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let mut best: Option<(SystemTime, PathBuf, PathBuf)> = None;
    for build in BUILDS {
        let base = dir.join(build);
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("account-") {
                continue;
            }
            let database = entry.path().join(DATABASE);
            // The write-ahead log holds the newest writes.
            let modified = [database.with_file_name("db_sqlite-wal"), database.clone()]
                .iter()
                .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
                .max();
            if let Some(modified) = modified
                && best.as_ref().is_none_or(|(t, ..)| modified > *t)
            {
                best = Some((modified, base.clone(), database));
            }
        }
    }
    best.map(|(_, base, database)| (base, database))
        .with_context(|| format!("Telegram database not found in {}", dir.display()))
}

/// The database key and salt, as hex: `.tempkeyEncrypted` decrypted with the default
/// passcode, then checked against the hash stored with it.
fn database_key(base: &Path) -> Result<String> {
    let mut data = std::fs::read(base.join(".tempkeyEncrypted"))
        .context("Cannot read Telegram's database key")?;
    if data.len() < 64 || !data.len().is_multiple_of(16) {
        bail!("Telegram's database key has an unknown format")
    }
    let hash = Sha512::digest(DEFAULT_PASSCODE);
    let plain = cbc::Decryptor::<aes::Aes256>::new(hash[..32].into(), hash[48..].into())
        .decrypt_padded_mut::<NoPadding>(&mut data)
        .map_err(|_| anyhow!("Telegram's database key has an unknown format"))?;
    let stored = u32::from_le_bytes([plain[48], plain[49], plain[50], plain[51]]);
    if murmur3(&plain[..48], SEED) != stored {
        bail!(
            "Telegram's database key is locked with a passcode; msgmcp reads Telegram only when Passcode Lock (Settings > Privacy and Security) is off"
        )
    }
    Ok(plain[..48].iter().map(|b| format!("{b:02x}")).collect())
}

pub fn open(dir: &Path) -> Result<Connection> {
    let (base, path) = locate(dir)?;
    let key = database_key(&base)?;
    let db = crate::common::open(&path)?;
    // Postbox leaves the first 32 bytes unencrypted. Set after the key, which creates the
    // connection's cipher settings, rather than with cipher_default_plaintext_header_size as
    // Telegram does: SQLCipher's defaults change every later connection in the process,
    // Signal's too.
    db.execute_batch(&format!(
        "PRAGMA key=\"x'{key}'\"; PRAGMA cipher_plaintext_header_size=32;"
    ))?;
    db.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .context("Telegram's database key did not open its database")?;
    // tg_real(value): false for service messages (group changes, calls, pins and the like).
    db.create_scalar_function(
        "tg_real",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| Ok(ctx.get_raw(0).as_blob().map_or(true, is_real)),
    )?;
    Ok(db)
}

/// A little-endian reader that fails, rather than panics, on a short value.
struct Buf<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Buf<'a> {
    fn new(data: &'a [u8]) -> Self {
        Buf { data, at: 0 }
    }
    fn done(&self) -> bool {
        self.at >= self.data.len()
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.data.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn count(&mut self) -> Option<usize> {
        usize::try_from(self.i32()?).ok()
    }
    /// A length-prefixed run of bytes.
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.count()?;
        self.take(n)
    }
}

/// A PostboxCoding value. Only the types msgmcp reads are kept.
enum Field {
    I32(i32),
    I64(i64),
    Str(String),
    Object(i32, Object),
    Objects(Vec<(i32, Object)>),
    Bytes(Vec<u8>),
    Skipped,
}

/// A PostboxCoding object: keyed fields, each a one-byte key length, the key, a type byte
/// and the value.
#[derive(Default)]
struct Object(Vec<(String, Field)>);

impl Object {
    fn parse(data: &[u8]) -> Option<Object> {
        let mut b = Buf::new(data);
        let mut fields = Vec::new();
        while !b.done() {
            let key_length = b.u8()? as usize;
            let key = String::from_utf8_lossy(b.take(key_length)?).into_owned();
            let field = match b.u8()? {
                0 => Field::I32(b.i32()?),
                1 => Field::I64(b.i64()?),
                2 => Field::I32(i32::from(b.u8()?)),
                3 => b.take(8).map(|_| Field::Skipped)?,
                4 => Field::Str(String::from_utf8_lossy(b.bytes()?).into_owned()),
                5 => {
                    let hash = b.i32()?;
                    Field::Object(hash, Object::parse(b.bytes()?)?)
                }
                6 => b
                    .count()
                    .and_then(|n| b.take(n.checked_mul(4)?))
                    .map(|_| Field::Skipped)?,
                7 => b
                    .count()
                    .and_then(|n| b.take(n.checked_mul(8)?))
                    .map(|_| Field::Skipped)?,
                8 => {
                    let mut items = Vec::new();
                    for _ in 0..b.count()? {
                        let hash = b.i32()?;
                        items.push((hash, Object::parse(b.bytes()?)?));
                    }
                    Field::Objects(items)
                }
                9 => {
                    for _ in 0..b.count()? {
                        // Key type, key, value type, value.
                        b.i32()?;
                        b.bytes()?;
                        b.i32()?;
                        b.bytes()?;
                    }
                    Field::Skipped
                }
                10 => Field::Bytes(b.bytes()?.to_vec()),
                11 => Field::Skipped,
                12 | 13 => {
                    for _ in 0..b.count()? {
                        b.bytes()?;
                    }
                    Field::Skipped
                }
                _ => return None,
            };
            fields.push((key, field));
        }
        Some(Object(fields))
    }
    fn get(&self, key: &str) -> Option<&Field> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, f)| f)
    }
    fn i32(&self, key: &str) -> Option<i32> {
        match self.get(key)? {
            Field::I32(v) => Some(*v),
            _ => None,
        }
    }
    fn i64(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Field::I64(v) => Some(*v),
            _ => None,
        }
    }
    /// A non-empty string field.
    fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Field::Str(v) if !v.trim().is_empty() => Some(v),
            _ => None,
        }
    }
    fn bytes(&self, key: &str) -> Option<&[u8]> {
        match self.get(key)? {
            Field::Bytes(v) => Some(v),
            _ => None,
        }
    }
    fn objects(&self, key: &str) -> &[(i32, Object)] {
        match self.get(key) {
            Some(Field::Objects(items)) => items,
            _ => &[],
        }
    }
}

/// The type hash of a root object, stored as the object field "_", without decoding it.
fn root_hash(data: &[u8]) -> Option<i32> {
    match data.get(..7)? {
        [1, b'_', 5, hash @ ..] => Some(i32::from_le_bytes(hash.try_into().ok()?)),
        _ => None,
    }
}

/// A root object's type hash and fields.
fn root(data: &[u8]) -> Option<(i32, Object)> {
    let Object(fields) = Object::parse(data)?;
    fields.into_iter().find_map(|(key, field)| match field {
        Field::Object(hash, object) if key == "_" => Some((hash, object)),
        _ => None,
    })
}

/// A message value as Postbox stores it, with attributes and embedded media still encoded.
struct Raw<'a> {
    flags: u32,
    author: Option<i64>,
    text: String,
    attributes: Vec<&'a [u8]>,
    media: Vec<&'a [u8]>,
    /// (namespace, ID) of media stored once in t6 and shared with other messages.
    referenced: Vec<(i32, i64)>,
}

fn raw(value: &[u8]) -> Option<Raw<'_>> {
    let mut b = Buf::new(value);
    if b.u8()? != 0 {
        return None;
    }
    b.take(8)?; // stable ID and version
    let data_flags = b.u8()?;
    // Globally unique ID, global tags, grouping key, group info, local tags, thread ID.
    for (bit, size) in [8, 4, 8, 4, 4, 8].into_iter().enumerate() {
        if data_flags & (1 << bit) != 0 {
            b.take(size)?;
        }
    }
    let flags = b.i32()? as u32;
    b.take(4)?; // tags
    let forward = b.u8()?;
    if forward != 0 {
        b.take(12)?; // author and date
        if forward & (1 << 1) != 0 {
            b.take(8)?;
        }
        if forward & (1 << 2) != 0 {
            b.take(16)?;
        }
        if forward & (1 << 3) != 0 {
            b.bytes()?;
        }
        if forward & (1 << 4) != 0 {
            b.bytes()?;
        }
        if forward & (1 << 5) != 0 {
            b.take(4)?;
        }
    }
    let author = if b.u8()? == 1 { Some(b.i64()?) } else { None };
    let text = String::from_utf8_lossy(b.bytes()?).into_owned();
    let attributes = (0..b.count()?).map(|_| b.bytes()).collect::<Option<_>>()?;
    let media = (0..b.count()?).map(|_| b.bytes()).collect::<Option<_>>()?;
    let referenced = (0..b.count()?)
        .map(|_| Some((b.i32()?, b.i64()?)))
        .collect::<Option<_>>()?;
    Some(Raw {
        flags,
        author,
        text,
        attributes,
        media,
        referenced,
    })
}

/// False for service messages; true for anything else, including values msgmcp can't
/// decode, so they show up as decode failures rather than vanish.
fn is_real(value: &[u8]) -> bool {
    raw(value).is_none_or(|r| !r.media.iter().any(|m| root_hash(m) == Some(TYPES.action)))
}

/// A t7 key: (peer ID, namespace, timestamp, message ID), each big-endian.
#[derive(Clone, Copy)]
struct Key {
    peer: i64,
    namespace: i32,
    timestamp: i32,
    id: i32,
}

impl Key {
    fn parse(key: &[u8]) -> Option<Key> {
        let int = |at: usize| Some(i32::from_be_bytes(key.get(at..at + 4)?.try_into().ok()?));
        Some(Key {
            peer: i64::from_be_bytes(key.get(..8)?.try_into().ok()?),
            namespace: int(8)?,
            timestamp: int(12)?,
            id: int(16)?,
        })
    }
    fn bytes(&self) -> Vec<u8> {
        [
            &self.peer.to_be_bytes()[..],
            &self.namespace.to_be_bytes(),
            &self.timestamp.to_be_bytes(),
            &self.id.to_be_bytes(),
        ]
        .concat()
    }
    /// The key in `ORDER` form, which the message cursor holds.
    fn order(&self) -> [u8; 20] {
        let mut order = [0; 20];
        order[..4].copy_from_slice(&self.timestamp.to_be_bytes());
        order[4..12].copy_from_slice(&self.peer.to_be_bytes());
        order[12..16].copy_from_slice(&self.namespace.to_be_bytes());
        order[16..].copy_from_slice(&self.id.to_be_bytes());
        order
    }
    fn local_id(&self) -> String {
        format!("{}.{}.{}", self.peer, self.namespace, self.id)
    }
}

/// The range of t7 keys that belong to one peer.
fn peer_range(peer: i64) -> (Vec<u8>, Vec<u8>) {
    (
        peer.to_be_bytes().to_vec(),
        peer.saturating_add(1).to_be_bytes().to_vec(),
    )
}

/// The ID that Telegram users see, without the namespace Postbox packs into peer IDs.
fn telegram_id(peer: i64) -> i64 {
    let data = peer as u64;
    let namespace = (data >> 32) & 0x7;
    let low = data & 0xffff_ffff;
    let high = (data >> 35) << 32;
    if namespace == 3 && high == 0 {
        i64::from(low as u32 as i32)
    } else {
        (high | low) as i64
    }
}

enum PeerKind {
    User,
    Group,
    Channel {
        broadcast: bool,
    },
    /// A secret chat with the user whose peer ID this is.
    Secret(i64),
}

struct Peer {
    kind: PeerKind,
    /// Group and channel title.
    title: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    username: Option<String>,
    phone: Option<String>,
    /// A bot, or Telegram's service notifications.
    automated: bool,
    /// Flagged by Telegram as a scam or fake account.
    flagged: bool,
}

/// Every peer Telegram knows, the user's Telegram contacts, and the user's own ID.
struct Directory {
    peers: HashMap<i64, Peer>,
    contacts: BTreeSet<i64>,
    me: Option<i64>,
}

impl Directory {
    fn load(db: &Connection) -> Result<Directory> {
        let mut peers = HashMap::new();
        let mut stmt = db.prepare("SELECT key, value FROM t2")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let Some((hash, o)) = root(row.get_ref(1)?.as_blob()?) else {
                continue;
            };
            let kind = match hash {
                h if h == TYPES.user => PeerKind::User,
                h if h == TYPES.group => PeerKind::Group,
                h if h == TYPES.channel => PeerKind::Channel {
                    broadcast: o.i32("i.t") == Some(0),
                },
                h if h == TYPES.secret_chat => match o.i64("r") {
                    Some(user) => PeerKind::Secret(user),
                    None => continue,
                },
                _ => continue,
            };
            let text = |key| o.str(key).map(str::to_owned);
            peers.insert(
                id,
                Peer {
                    kind,
                    title: text("t"),
                    first_name: text("fn"),
                    last_name: text("ln"),
                    username: text("un"),
                    phone: text("p"),
                    automated: matches!(o.get("bi"), Some(Field::Object(..)))
                        || telegram_id(id) == SERVICE_NOTIFICATIONS,
                    flagged: o.i32("fl").is_some_and(|f| f & (SCAM | FAKE) != 0),
                },
            );
        }
        // t16 keys are big-endian peer IDs.
        let mut contacts = BTreeSet::new();
        let mut stmt = db.prepare("SELECT key FROM t16")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if let Ok(key) = <[u8; 8]>::try_from(row.get_ref(0)?.as_blob()?) {
                contacts.insert(i64::from_be_bytes(key));
            }
        }
        Ok(Directory {
            me: own_id(db)?,
            contacts,
            peers,
        })
    }

    fn person(&self, peer: i64, contacts: &Contacts) -> Person {
        let Some(info) = self.peers.get(&peer) else {
            return Person {
                telegram_id: Some(telegram_id(peer).to_string()),
                ..crate::common::person(None, contacts)
            };
        };
        if let PeerKind::Secret(user) = info.kind {
            return self.person(user, contacts);
        }
        let address = info.phone.as_deref().map(|p| {
            let digits = p.trim_start_matches('+');
            display_address(&format!("+{digits}"))
        });
        let (contact, ambiguous) = address
            .as_deref()
            .map_or((None, false), |a| contact_name(a, contacts));
        let full = [&info.first_name, &info.last_name]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        let name = contact.or_else(|| {
            (!ambiguous)
                .then(|| {
                    info.title
                        .clone()
                        .or((!full.is_empty()).then_some(full))
                        .or(info.username.clone())
                })
                .flatten()
        });
        Person {
            address,
            jid: None,
            signal_id: None,
            telegram_id: Some(telegram_id(peer).to_string()),
            telegram_username: info.username.clone(),
            name,
            ambiguous,
        }
    }
}

/// The user's own peer ID: the author of their newest outgoing message.
fn own_id(db: &Connection) -> Result<Option<i64>> {
    let mut stmt = db.prepare(&format!(
        "SELECT value FROM t7 WHERE {NAMESPACES} ORDER BY substr(key,13,4) DESC"
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if let Some(raw) = raw(row.get_ref(0)?.as_blob()?)
            && raw.flags & INCOMING == 0
            && let Some(author) = raw.author
        {
            return Ok(Some(author));
        }
    }
    Ok(None)
}

/// A peer's read state in one message namespace.
enum ReadState {
    ById {
        max_read: i32,
        count: i32,
        marked: bool,
    },
    ByIndex {
        max_read: (i32, i32),
        count: i32,
        marked: bool,
    },
}

/// Read states by peer and namespace.
fn read_states(db: &Connection) -> Result<HashMap<(i64, i32), ReadState>> {
    let mut states = HashMap::new();
    let mut stmt = db.prepare("SELECT key, value FROM t14")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let peer: i64 = row.get(0)?;
        let mut b = Buf::new(row.get_ref(1)?.as_blob()?);
        let mut read = || -> Option<()> {
            for _ in 0..b.count()? {
                let namespace = b.i32()?;
                let state = if b.u8()? == 0 {
                    let max_read = b.i32()?;
                    b.take(8)?; // newest read outgoing and newest known IDs
                    let count = b.i32()?;
                    ReadState::ById {
                        max_read,
                        count,
                        marked: b.i32()? & 1 != 0,
                    }
                } else {
                    let timestamp = b.i32()?;
                    b.take(12)?; // peer and namespace
                    let id = b.i32()?;
                    b.take(20)?; // newest read outgoing message
                    let count = b.i32()?;
                    ReadState::ByIndex {
                        max_read: (timestamp, id),
                        count,
                        marked: b.i32()? & 1 != 0,
                    }
                };
                states.insert((peer, namespace), state);
            }
            Some(())
        };
        read();
    }
    Ok(states)
}

fn is_read(states: &HashMap<(i64, i32), ReadState>, key: &Key) -> Option<bool> {
    Some(match states.get(&(key.peer, key.namespace))? {
        ReadState::ById { max_read, .. } => key.id <= *max_read,
        ReadState::ByIndex { max_read, .. } => (key.timestamp, key.id) <= *max_read,
    })
}

/// A message's media, embedded or shared, as (type hash, object).
fn media(db: &Connection, raw: &Raw<'_>) -> Vec<(i32, Object)> {
    let mut items: Vec<_> = raw.media.iter().filter_map(|m| root(m)).collect();
    for &(namespace, id) in &raw.referenced {
        if let Some(item) = shared_media(db, namespace, id) {
            items.push(item);
        }
    }
    items
}

/// Media stored in t6: either the media itself or a pointer to the message that embeds it.
fn shared_media(db: &Connection, namespace: i32, id: i64) -> Option<(i32, Object)> {
    let key = [&namespace.to_be_bytes()[..], &id.to_be_bytes()].concat();
    let value: Vec<u8> = db
        .query_row("SELECT value FROM t6 WHERE key=?", [key], |r| r.get(0))
        .optional()
        .ok()??;
    let mut b = Buf::new(&value);
    match b.u8()? {
        0 => root(b.bytes()?),
        1 => {
            let (peer, ns, msg, timestamp) = (b.i64()?, b.i32()?, b.i32()?, b.i32()?);
            let key = Key {
                peer,
                namespace: ns,
                timestamp,
                id: msg,
            };
            let value: Vec<u8> = db
                .query_row("SELECT value FROM t7 WHERE key=?", [key.bytes()], |r| {
                    r.get(0)
                })
                .optional()
                .ok()??;
            let owner = raw(&value)?;
            // Media IDs are encoded as (namespace, ID), little-endian.
            let wanted = [&namespace.to_le_bytes()[..], &id.to_le_bytes()].concat();
            owner
                .media
                .iter()
                .filter_map(|m| root(m))
                .find(|(_, o)| o.bytes("i") == Some(&wanted[..]))
        }
        _ => None,
    }
}

// TelegramMediaFileAttribute types.
const FILE_NAME: i32 = 0;
const STICKER: i32 = 1;
const ANIMATED: i32 = 3;
const VIDEO: i32 = 4;
const AUDIO: i32 = 5;
const CUSTOM_EMOJI: i32 = 10;

/// The message kind, and a document's file name.
fn kind(media: &[(i32, Object)]) -> (Kind, Option<String>) {
    let Some((hash, o)) = media.first() else {
        return (Kind::Text, None);
    };
    let t = &*TYPES;
    let kind = match *hash {
        h if h == t.image => Kind::Image,
        h if h == t.webpage => Kind::Link,
        h if h == t.contact => Kind::Contact,
        h if h == t.map => Kind::Location,
        h if h == t.file => {
            let attributes: Vec<i32> = o
                .objects("at")
                .iter()
                .filter_map(|(_, a)| a.i32("t"))
                .collect();
            let has = |t: i32| attributes.contains(&t);
            let name = o
                .objects("at")
                .iter()
                .find(|(_, a)| a.i32("t") == Some(FILE_NAME))
                .and_then(|(_, a)| a.str("fn"))
                .map(str::to_owned);
            let kind = if has(STICKER) || has(CUSTOM_EMOJI) {
                Kind::Sticker
            } else if has(ANIMATED) {
                Kind::Gif
            } else if has(VIDEO) {
                Kind::Video
            } else if has(AUDIO) {
                Kind::Audio
            } else {
                return (Kind::Document, name);
            };
            return (kind, None);
        }
        _ => Kind::Other,
    };
    let text = (*hash == t.contact)
        .then(|| {
            let name = [o.str("n.f"), o.str("n.l")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            (!name.is_empty()).then_some(name)
        })
        .flatten();
    (kind, text)
}

fn time(seconds: i32) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(i64::from(seconds), 0)
}

fn timestamp(seconds: i32) -> Option<String> {
    time(seconds).map(|t| t.to_rfc3339())
}

/// Seconds since 1970 as Telegram stores them, rounded up: a message at second `s` is at
/// or after `t` exactly when `s >= ceil(t)`.
fn seconds(t: DateTime<Utc>) -> i32 {
    let s = t.timestamp() + i64::from(t.timestamp_subsec_nanos() > 0);
    s.clamp(0, i64::from(i32::MAX)) as i32
}

struct Reader<'a> {
    db: &'a Connection,
    directory: &'a Directory,
    reads: &'a HashMap<(i64, i32), ReadState>,
    contacts: &'a Contacts,
}

impl Reader<'_> {
    fn message(&self, key: &Key, value: &[u8]) -> Message {
        let mut item = Message {
            source: Source::Telegram,
            message_id: format!("{PREFIX}:{}", key.local_id()),
            guid: Some(key.id.to_string()),
            conversation_id: format!("{PREFIX}:{}", key.peer),
            sent_at: timestamp(key.timestamp),
            is_from_me: false,
            is_read: None,
            sender: crate::common::person(None, self.contacts),
            kind: None,
            text: None,
            text_status: TextStatus::DecodeFailed,
            reply_to_guid: None,
            reactions: Vec::new(),
        };
        let Some(raw) = raw(value) else {
            return item;
        };
        let from_me = raw.flags & INCOMING == 0;
        let (kind, fallback) = kind(&media(self.db, &raw));
        let text = Some(raw.text.clone())
            .filter(|t| !t.trim().is_empty())
            .or(fallback);
        let attributes: Vec<_> = raw.attributes.iter().filter_map(|a| root(a)).collect();
        let reply_to_guid =
            attributes
                .iter()
                .find(|(h, _)| *h == TYPES.reply)
                .and_then(|(_, o)| {
                    // (namespace, ID) packed as namespace | ID << 32, and the peer.
                    let packed = o.i64("i")?;
                    (o.i64("p")? == key.peer).then(|| (packed >> 32).to_string())
                });
        let mut reactions: Vec<(i32, Reaction)> = attributes
            .iter()
            .filter(|(h, _)| *h == TYPES.reactions)
            .flat_map(|(_, o)| o.objects("rp"))
            .filter_map(|(_, r)| {
                let emoji = r.str("v")?.to_owned();
                let is_from_me = r.i32("my").is_some_and(|m| m != 0)
                    || r.i64("p").is_some_and(|p| Some(p) == self.directory.me);
                let sender = if is_from_me {
                    Person::me()
                } else {
                    self.directory.person(r.i64("p")?, self.contacts)
                };
                Some((
                    r.i32("ts").unwrap_or(0),
                    Reaction {
                        emoji,
                        is_from_me,
                        sender,
                    },
                ))
            })
            .collect();
        reactions.sort_by_key(|(at, _)| *at);
        item.is_from_me = from_me;
        item.is_read = if from_me {
            None
        } else {
            is_read(self.reads, key)
        };
        item.sender = if from_me {
            Person::me()
        } else {
            // Messages in direct chats and secret chats have the other person as author;
            // channel posts may have none, and come from the channel.
            self.directory
                .person(raw.author.unwrap_or(key.peer), self.contacts)
        };
        item.kind = Some(kind);
        item.text_status = if text.is_some() {
            TextStatus::Ok
        } else {
            TextStatus::NoText
        };
        item.text = text;
        item.reply_to_guid = reply_to_guid;
        item.reactions = reactions.into_iter().map(|(_, r)| r).collect();
        item
    }
}

fn ensure_conversation(directory: &Directory, peer: i64) -> Result<()> {
    if !directory.peers.contains_key(&peer) {
        bail!("Unknown conversation ID")
    }
    Ok(())
}

/// Every chat in Telegram's chat list, including archived ones, that has a cached message
/// other than service messages or is pinned, and whose newest message is at or after
/// `active_after`.
fn conversations(
    dir: &Path,
    contacts: &Contacts,
    active_after: Option<DateTime<Utc>>,
) -> Result<Vec<Listed>> {
    let db = open(dir)?;
    let directory = Directory::load(&db)?;
    let reads = read_states(&db)?;
    // One pass over the cached messages: each chat's newest real message, and the people
    // who wrote in groups.
    let mut latest: HashMap<i64, i32> = HashMap::new();
    let mut authors: HashMap<i64, BTreeSet<i64>> = HashMap::new();
    let mut written: BTreeSet<i64> = BTreeSet::new();
    let mut stmt = db.prepare(&format!("SELECT key, value FROM t7 WHERE {NAMESPACES}"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let Some(key) = Key::parse(row.get_ref(0)?.as_blob()?) else {
            continue;
        };
        let value = row.get_ref(1)?.as_blob()?;
        let Some(raw) = raw(value) else {
            continue;
        };
        if raw.media.iter().any(|m| root_hash(m) == Some(TYPES.action)) {
            continue;
        }
        if raw.flags & INCOMING == 0 {
            written.insert(key.peer);
        }
        let newest = latest.entry(key.peer).or_insert(key.timestamp);
        *newest = (*newest).max(key.timestamp);
        if let Some(author) = raw.author
            && Some(author) != directory.me
            && author != key.peer
        {
            authors.entry(key.peer).or_default().insert(author);
        }
    }
    let mut stmt = db.prepare("SELECT key, value FROM t8")?;
    let mut rows = stmt.query([])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let peer: i64 = row.get(0)?;
        let Some((folder, pinned)) = chat_list_entry(row.get_ref(1)?.as_blob()?) else {
            continue;
        };
        let Some(info) = directory.peers.get(&peer) else {
            continue;
        };
        let newest = latest.get(&peer).copied();
        // A chat with nothing but service messages, such as "joined Telegram", unless the
        // user pinned it.
        if newest.is_none() && !pinned {
            continue;
        }
        if let Some(after) = active_after
            && newest.is_none_or(|t| t < seconds(after))
        {
            continue;
        }
        let (chat_type, participants) = match info.kind {
            PeerKind::User => (ChatType::Direct, vec![directory.person(peer, contacts)]),
            PeerKind::Secret(user) => (ChatType::Direct, vec![directory.person(user, contacts)]),
            PeerKind::Group | PeerKind::Channel { broadcast: false } => (
                ChatType::Group,
                authors
                    .get(&peer)
                    .into_iter()
                    .flatten()
                    .map(|a| directory.person(*a, contacts))
                    .collect(),
            ),
            PeerKind::Channel { broadcast: true } => (ChatType::Channel, Vec::new()),
        };
        // Like Messages' filtered chats: automated senders, scam and fake accounts, and
        // people outside the user's Telegram contacts whom the user has never written to,
        // which is how most spam arrives.
        let user = match info.kind {
            PeerKind::User => Some(peer),
            PeerKind::Secret(user) => Some(user),
            _ => None,
        };
        let filtered = user.is_some_and(|user| {
            let account = directory.peers.get(&user);
            account.is_some_and(|a| a.automated || a.flagged)
                || (!directory.contacts.contains(&user) && !written.contains(&peer))
        });
        let (name, identifier) = match chat_type {
            ChatType::Direct => {
                let person = &participants[0];
                let identifier = person
                    .address
                    .clone()
                    .or(person.telegram_username.as_ref().map(|u| format!("@{u}")))
                    .or(person.telegram_id.clone());
                (person.name.clone(), identifier)
            }
            _ => (
                info.title.clone(),
                info.username.as_ref().map(|u| format!("@{u}")),
            ),
        };
        let (mut unread, mut marked) = (0u64, false);
        for namespace in 0..3 {
            match reads.get(&(peer, namespace)) {
                Some(
                    ReadState::ById {
                        count, marked: m, ..
                    }
                    | ReadState::ByIndex {
                        count, marked: m, ..
                    },
                ) => {
                    unread += (*count).max(0) as u64;
                    marked |= *m;
                }
                None => {}
            }
        }
        items.push(Listed {
            latest: newest.and_then(time),
            id: peer.to_string(),
            conversation: Conversation {
                source: Source::Telegram,
                conversation_id: format!("{PREFIX}:{peer}"),
                identifier,
                name,
                chat_type: Some(chat_type),
                participant_count: participants.len(),
                participants,
                latest_message_at: newest.and_then(timestamp),
                unread_count: if unread == 0 && marked { 1 } else { unread },
                marked_unread: marked,
                archived: folder == ARCHIVE_GROUP,
                filtered,
            },
        });
    }
    Ok(items)
}

/// A t8 chat list entry's folder and whether it is pinned, or `None` when Telegram doesn't
/// list the chat: it lists a chat that has a newest message or is pinned. Telegram keeps
/// entries without either for many people it knows of, such as contacts never written to.
fn chat_list_entry(value: &[u8]) -> Option<(i32, bool)> {
    let mut b = Buf::new(value);
    let has_message = b.u8()? & 1 != 0;
    if has_message {
        b.take(12)?; // newest message
    }
    if b.u8()? != 1 {
        return None;
    }
    let pinned = u16::from_le_bytes(b.take(2)?.try_into().ok()?) != 0;
    if b.u8()? != 0 {
        b.take(4)?; // minimum timestamp
    }
    let folder = b.i32()?;
    (has_message || pinned).then_some((folder, pinned))
}

fn parse_cursor(cursor: Option<&str>, newest_first: bool) -> Result<[u8; 20]> {
    let Some(cursor) = cursor else {
        return Ok([if newest_first { 0xff } else { 0 }; 20]);
    };
    let bytes: Option<Vec<u8>> = (0..cursor.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(cursor.get(i..i + 2)?, 16).ok())
        .collect();
    bytes
        .and_then(|b| b.try_into().ok())
        .context("Invalid cursor")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn messages(
    dir: &Path,
    contacts: &Contacts,
    filter: &Filter,
    cursor: Option<&str>,
    cap: usize,
    budget: usize,
) -> Result<Page> {
    let start = parse_cursor(cursor, filter.newest_first)?;
    let db = open(dir)?;
    let directory = Directory::load(&db)?;
    let reads = read_states(&db)?;
    let (cmp, order) = if filter.newest_first {
        ("<", "DESC")
    } else {
        (">", "ASC")
    };
    let mut sql = format!("SELECT key, value FROM t7 WHERE {NAMESPACES} AND {ORDER} {cmp} (?,?,?)");
    let mut args = order_values(&start).to_vec();
    if let Some(conversation) = &filter.conversation {
        let peer = number(conversation)?;
        ensure_conversation(&directory, peer)?;
        let (low, high) = peer_range(peer);
        sql += " AND key >= ? AND key < ?";
        args.extend([Sql::Blob(low), Sql::Blob(high)]);
    }
    if let Some(after) = filter.after {
        sql += " AND substr(key,13,4) >= ?";
        args.push(Sql::Blob(seconds(after).to_be_bytes().to_vec()));
    }
    if let Some(before) = filter.before {
        sql += " AND substr(key,13,4) < ?";
        args.push(Sql::Blob(seconds(before).to_be_bytes().to_vec()));
    }
    sql += &format!(" AND tg_real(value) ORDER BY {}", order_by(order));
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
    let reader = Reader {
        db: &db,
        directory: &directory,
        reads: &reads,
        contacts,
    };
    page(
        &mut rows,
        filter,
        start,
        (cap, budget),
        |row| {
            let key_bytes = row.get_ref(0)?.as_blob()?;
            let key = Key::parse(key_bytes).ok_or(rusqlite::Error::InvalidQuery)?;
            Ok((
                reader.message(&key, row.get_ref(1)?.as_blob()?),
                key.order(),
            ))
        },
        |order| hex(&order),
    )
}

fn context(
    dir: &Path,
    contacts: &Contacts,
    conversation: &str,
    message: &str,
    (before, after): (usize, usize),
) -> Result<Vec<Message>> {
    let peer = number(conversation)?;
    let not_found = || anyhow!("Message not found in conversation");
    let parts: Vec<&str> = message.split('.').collect();
    let [message_peer, namespace, id] = parts.as_slice() else {
        return Err(not_found());
    };
    let (Ok(message_peer), Ok(namespace), Ok(id)) = (
        message_peer.parse::<i64>(),
        namespace.parse::<i32>(),
        id.parse::<i32>(),
    ) else {
        return Err(not_found());
    };
    let db = open(dir)?;
    let directory = Directory::load(&db)?;
    ensure_conversation(&directory, peer)?;
    if message_peer != peer {
        return Err(not_found());
    }
    let (low, high) = peer_range(peer);
    let select = format!(
        "SELECT key, value FROM t7 WHERE key >= ?1 AND key < ?2 AND {NAMESPACES} AND tg_real(value)"
    );
    let found: Option<Vec<u8>> = db
        .query_row(
            &format!("{select} AND substr(key,9,4)=?3 AND substr(key,17,4)=?4"),
            rusqlite::params![low, high, namespace.to_be_bytes(), id.to_be_bytes()],
            |r| r.get(0),
        )
        .optional()?;
    let order = found
        .as_deref()
        .and_then(Key::parse)
        .ok_or_else(not_found)?
        .order();
    let reads = read_states(&db)?;
    let reader = Reader {
        db: &db,
        directory: &directory,
        reads: &reads,
        contacts,
    };
    let [time, peer_and_namespace, id] = order_values(&order);
    let mut values = Vec::new();
    for (op, direction, count) in [("<", "DESC", before), (">=", "ASC", after + 1)] {
        let mut stmt = db.prepare(&format!(
            "{select} AND {ORDER} {op} (?3,?4,?5) ORDER BY {} LIMIT ?6",
            order_by(direction)
        ))?;
        let mut rows = stmt.query(rusqlite::params![
            low,
            high,
            time,
            peer_and_namespace,
            id,
            count as i64
        ])?;
        let mut part = Vec::new();
        while let Some(row) = rows.next()? {
            if let Some(key) = Key::parse(row.get_ref(0)?.as_blob()?) {
                part.push(reader.message(&key, row.get_ref(1)?.as_blob()?));
            }
        }
        if op == "<" {
            part.reverse();
        }
        values.extend(part);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn murmur_matches_telegram() {
        // Telegram stores the hash of the key and salt as a signed 32-bit integer computed
        // with seed -137723950; an empty input hashes to the finalized seed.
        assert_eq!(SEED as i32, -137_723_950);
        assert_eq!(murmur3(b"", 0), 0);
        assert_eq!(murmur3(b"hello", 0), 0x248b_fa47);
    }

    #[test]
    fn peer_ids_unpack() {
        // A user (namespace 0), a group (1) and a channel (2) with a 32-bit ID.
        assert_eq!(telegram_id(777_000), 777_000);
        assert_eq!(telegram_id((1 << 32) | 42), 42);
        assert_eq!(telegram_id((2 << 32) | 1_234_567_890), 1_234_567_890);
        // An ID above 32 bits keeps its high bits above the namespace.
        assert_eq!(telegram_id((1 << 35) | 5), (1 << 32) | 5);
    }
}
