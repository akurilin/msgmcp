//! Signal Desktop backend (read-only).
//!
//! Signal keeps everything in one SQLCipher database, `sql/db.sqlite` in its Application
//! Support folder. The database key is in `config.json`: `encryptedKey`, which Electron's
//! safeStorage encrypted with the "Signal Safe Storage" password in the login Keychain, or
//! a plaintext `key` in installs from before 2024.
//! - `conversations`: direct chats (type `private`) and groups. `id` is a UUID; its implicit
//!   rowid is not stable, since Signal runs VACUUM. `json` holds unread counts, archive
//!   state, nicknames and group members.
//! - `messages`: `rowid` is stable. `type` is incoming or outgoing for real messages and
//!   names the event otherwise; `json` holds quotes, reactions and link previews.
//! - `message_attachments`: files, stickers and contact cards, one row per attachment;
//!   `editHistoryIndex` -1 belongs to the message itself rather than an earlier edit.
//! - `items`: key-value settings, including the user's own account ID.
use crate::backend::Backend;
use crate::common::{
    Contacts, Filter, Listed, Page, contact_name, display_address, message_number, neighbors, page,
};
use crate::contacts::AddressBookEntry;
use crate::types::{
    ChatType, Conversation, Kind, Message, NameSource, Person, Reaction, Source, TextStatus,
};
use anyhow::{Context, Result, bail};
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const PREFIX: &str = "signal";

/// Signal Desktop's Application Support folder.
struct Signal(PathBuf);

pub fn backend(dir: PathBuf) -> Box<dyn Backend> {
    Box::new(Signal(dir))
}

impl Backend for Signal {
    fn check(&self) -> Result<()> {
        open(&self.0).map(drop)
    }
    fn address_book(&self) -> Option<Result<Vec<AddressBookEntry>>> {
        Some(address_book(&self.0))
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
        context(
            &self.0,
            contacts,
            conversation,
            message_number(message)?,
            (before, after),
        )
    }
}
const DATABASE: &str = "sql/db.sqlite";
const KEYCHAIN_SERVICE: &str = "Signal Safe Storage";

// Conversations the user removed stay in the table with a removalStage.
const CONVERSATION_FILTER: &str =
    "c.type IN ('private','group') AND json_extract(c.json,'$.removalStage') IS NULL";
// Every other type is an event: group changes, safety-number changes, calls, timers, stories.
const MESSAGE_FILTER: &str = "m.type IN ('incoming','outgoing') AND m.sent_at IS NOT NULL";

const MESSAGE_SELECT: &str = "SELECT m.rowid, m.id, m.conversationId, m.sent_at, m.type, m.sourceServiceId, m.body, m.readStatus, m.json, (SELECT q.id FROM messages q WHERE q.conversationId=m.conversationId AND q.sent_at=json_extract(m.json,'$.quote.id') AND q.rowid<>m.rowid LIMIT 1), (SELECT json_object('type',a.attachmentType,'contentType',a.contentType,'fileName',a.fileName,'flags',a.flags) FROM message_attachments a WHERE a.messageId=m.id AND a.editHistoryIndex=-1 AND a.attachmentType IN ('attachment','sticker','contact') ORDER BY a.orderInMessage LIMIT 1) FROM messages m JOIN conversations c ON c.id=m.conversationId";

/// `encryptedKey` → database key, so the Keychain is asked once per process.
static KEY_CACHE: Mutex<Option<(String, String)>> = Mutex::new(None);

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Decrypt Chromium's macOS OSCrypt format, which Electron's safeStorage uses: `v10`, then
/// AES-128-CBC with a key derived from the Keychain password (PBKDF2-SHA1, salt
/// "saltysalt", 1003 iterations) and an IV of 16 spaces.
fn decrypt_safe_storage(password: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let ciphertext = data
        .strip_prefix(b"v10")
        .context("Signal's encryptedKey is not in the expected format")?;
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1003, &mut key);
    cbc::Decryptor::<aes::Aes128>::new(&key.into(), &[b' '; 16].into())
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .ok()
        .context("Signal's encryptedKey did not decrypt with the Keychain password")
}

/// The services call this once and keep the lock: a Keychain read then fails at once
/// instead of waiting on a dialog that nobody may be at the Mac to answer. `msgmcp
/// --check-signal` reads with the dialog, so the user can choose Always Allow.
pub fn disable_keychain_prompts()
-> Result<security_framework::os::macos::keychain::KeychainUserInteractionLock> {
    security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
        .context("Cannot turn off Keychain prompts")
}

const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;

fn keychain_password() -> Result<Vec<u8>> {
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
    let found = ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(KEYCHAIN_SERVICE)
        .load_data(true)
        .limit(1)
        .search();
    match found.map(|items| items.into_iter().next()) {
        Ok(Some(SearchResult::Data(password))) => Ok(password),
        Ok(_) => bail!("\"{KEYCHAIN_SERVICE}\" has no password in the login Keychain"),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => {
            bail!("\"{KEYCHAIN_SERVICE}\" is not in the login Keychain; open Signal Desktop once")
        }
        Err(e) if e.code() == ERR_SEC_INTERACTION_NOT_ALLOWED => bail!(
            "msgmcp may not read \"{KEYCHAIN_SERVICE}\" from the login Keychain yet. On the Mac, run ~/Applications/msgmcp.app/Contents/MacOS/msgmcp --check-signal and choose Always Allow"
        ),
        Err(e) => Err(e)
            .with_context(|| format!("Cannot read \"{KEYCHAIN_SERVICE}\" from the login Keychain")),
    }
}

/// The SQLCipher key, as 64 hex digits.
fn database_key(dir: &Path) -> Result<String> {
    let path = dir.join("config.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("Cannot read Signal's config at {}", path.display()))?;
    let config: Value = serde_json::from_str(&text).context("Signal's config.json is not JSON")?;
    let key = if let Some(encrypted) = config["encryptedKey"].as_str() {
        let mut cache = KEY_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        match cache.as_ref() {
            Some((cached, key)) if cached == encrypted => key.clone(),
            _ => {
                let data = unhex(encrypted).context("Signal's encryptedKey is not hex")?;
                let key = decrypt_safe_storage(&keychain_password()?, &data)?;
                let key = String::from_utf8(key).context("Signal's database key is not text")?;
                *cache = Some((encrypted.to_owned(), key.clone()));
                key
            }
        }
    } else if let Some(key) = config["key"].as_str() {
        key.to_owned()
    } else {
        bail!("Signal's config.json has no database key")
    };
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Signal's database key is not 64 hex digits")
    }
    Ok(key)
}

pub fn open(dir: &Path) -> Result<Connection> {
    let path = dir.join(DATABASE);
    if !path.exists() {
        bail!("Signal database not found at {}", path.display())
    }
    let key = database_key(dir)?;
    let db = crate::common::open(&path)?;
    db.execute_batch(&format!("PRAGMA key = \"x'{key}'\";"))?;
    if let Err(error) = db.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(())) {
        *KEY_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return Err(error).context("Signal's database key did not open its database");
    }
    Ok(db)
}

/// For `msgmcp --check-signal`: the number of conversations and messages msgmcp reads.
pub fn counts(dir: &Path) -> Result<(i64, i64)> {
    let db = open(dir)?;
    let count = |sql: String| db.query_row(&sql, [], |r| r.get::<_, i64>(0));
    Ok((
        count(format!(
            "SELECT count(*) FROM conversations c WHERE {CONVERSATION_FILTER} AND c.active_at IS NOT NULL"
        ))?,
        count(format!(
            "SELECT count(*) FROM messages m JOIN conversations c ON c.id=m.conversationId WHERE {MESSAGE_FILTER} AND {CONVERSATION_FILTER}"
        ))?,
    ))
}

/// A Signal account as one of the user's direct conversations describes it.
struct Profile {
    e164: Option<String>,
    service_id: Option<String>,
    name: Option<String>,
}

/// Every account Signal knows, from its direct conversations.
struct Directory {
    /// Conversation ID → account.
    profiles: HashMap<String, Profile>,
    /// Lowercased service ID (ACI) → conversation ID.
    services: HashMap<String, String>,
    /// The user's own ACI and Note to Self conversation ID.
    me: Option<String>,
    my_conversation: Option<String>,
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

impl Directory {
    fn load(db: &Connection) -> Result<Directory> {
        // The value is "<aci>.<device>".
        let me: Option<String> = db
            .query_row(
                "SELECT json_extract(json,'$.value') FROM items WHERE id='uuid_id'",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .and_then(|v| v.split('.').next().map(str::to_lowercase));
        let mut stmt = db.prepare("SELECT id, e164, serviceId, name, profileFullName, json_extract(json,'$.nicknameGivenName'), json_extract(json,'$.nicknameFamilyName'), json_extract(json,'$.username') FROM conversations WHERE type='private'")?;
        let mut rows = stmt.query([])?;
        let (mut profiles, mut services) = (HashMap::new(), HashMap::new());
        let mut my_conversation = None;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let service_id = nonempty(row.get(2)?).map(|s| s.to_lowercase());
            let nickname = nonempty(Some(format!(
                "{} {}",
                row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                row.get::<_, Option<String>>(6)?.unwrap_or_default()
            )));
            // Signal's own order: the user's nickname, the name from their phone's
            // contacts, the person's profile name, then their username.
            let name = nickname
                .or(nonempty(row.get(3)?))
                .or(nonempty(row.get(4)?))
                .or(nonempty(row.get(7)?));
            if let Some(service) = &service_id {
                if me.as_ref() == Some(service) {
                    my_conversation = Some(id.clone());
                }
                services.insert(service.clone(), id.clone());
            }
            profiles.insert(
                id,
                Profile {
                    e164: nonempty(row.get(1)?),
                    service_id,
                    name,
                },
            );
        }
        Ok(Directory {
            profiles,
            services,
            me,
            my_conversation,
        })
    }

    /// AddressBook names win; Signal's name for the account is the fallback.
    fn person(&self, profile: &Profile, contacts: &Contacts) -> Person {
        let address = profile.e164.as_deref().map(display_address);
        let (mut name, ambiguous) = address
            .as_deref()
            .map_or((None, false), |a| contact_name(a, contacts));
        if name.is_none() && !ambiguous {
            name = profile.name.clone();
        }
        Person {
            address,
            jid: None,
            signal_id: profile.service_id.clone(),
            telegram_id: None,
            telegram_username: None,
            name,
            ambiguous,
        }
    }

    fn by_conversation(&self, id: &str, contacts: &Contacts) -> Option<Person> {
        self.profiles.get(id).map(|p| self.person(p, contacts))
    }

    fn by_service(&self, service_id: &str, contacts: &Contacts) -> Person {
        let service_id = service_id.to_lowercase();
        self.services
            .get(&service_id)
            .and_then(|id| self.by_conversation(id, contacts))
            .unwrap_or(Person {
                address: None,
                jid: None,
                signal_id: Some(service_id),
                telegram_id: None,
                telegram_username: None,
                name: None,
                ambiguous: false,
            })
    }
}

/// Only direct chats the user used belong in contact search. Signal keeps other accounts
/// in this table after a group message or profile lookup.
fn address_book(dir: &Path) -> Result<Vec<AddressBookEntry>> {
    let db = open(dir)?;
    let directory = Directory::load(&db)?;
    let mut stmt = db.prepare(&format!(
        "SELECT c.id FROM conversations c WHERE {CONVERSATION_FILTER} AND c.type='private' AND (c.active_at IS NOT NULL OR EXISTS (SELECT 1 FROM messages m WHERE m.conversationId=c.id AND {MESSAGE_FILTER}))"
    ))?;
    let mut rows = stmt.query([])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let Some(profile) = directory.profiles.get(&id) else {
            continue;
        };
        if profile.service_id == directory.me {
            continue;
        }
        entries.push(AddressBookEntry {
            source: NameSource::Signal,
            name: profile.name.clone(),
            phone: profile.e164.as_deref().map(display_address),
            jids: Vec::new(),
            signal_id: profile.service_id.clone(),
            telegram_id: None,
            telegram_username: None,
        });
    }
    Ok(entries)
}

fn time(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(ms)
}

fn timestamp(ms: i64) -> Option<String> {
    time(ms).map(|t| t.to_rfc3339())
}

fn millis(t: Option<DateTime<Utc>>) -> Option<i64> {
    t.map(|t| t.timestamp_millis())
}

// Signal's AttachmentPointer flags.
const GIF_FLAG: u64 = 8;

/// `attachment` is the message's first file, sticker or contact card.
fn kind(json: &Value, attachment: &Value) -> Kind {
    let present = |field: &str| match &json[field] {
        Value::Null | Value::Bool(false) => false,
        Value::Array(items) => !items.is_empty(),
        _ => true,
    };
    if present("deletedForEveryone") || present("isErased") {
        return Kind::Deleted;
    }
    if present("isViewOnce") || present("poll") {
        return Kind::Other;
    }
    if present("sticker") || attachment["type"] == "sticker" {
        return Kind::Sticker;
    }
    if present("contact") || attachment["type"] == "contact" {
        return Kind::Contact;
    }
    if attachment["type"] == "attachment" {
        let content_type = attachment["contentType"].as_str().unwrap_or_default();
        let flags = attachment["flags"].as_u64().unwrap_or(0);
        return match content_type.split('/').next() {
            _ if flags & GIF_FLAG != 0 || content_type == "image/gif" => Kind::Gif,
            Some("image") => Kind::Image,
            Some("video") => Kind::Video,
            Some("audio") => Kind::Audio,
            _ => Kind::Document,
        };
    }
    if present("preview") {
        return Kind::Link;
    }
    Kind::Text
}

fn message(
    row: &rusqlite::Row<'_>,
    dir: &Directory,
    contacts: &Contacts,
) -> rusqlite::Result<(Message, (i64, i64))> {
    let id: i64 = row.get(0)?;
    let chat: String = row.get(2)?;
    let sent: i64 = row.get(3)?;
    let from_me = row.get::<_, Option<String>>(4)?.as_deref() == Some("outgoing");
    let source: Option<String> = row.get(5)?;
    let body = nonempty(row.get(6)?);
    let read_status: Option<i64> = row.get(7)?;
    let json: Value = row
        .get::<_, Option<String>>(8)?
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let attachment: Value = row
        .get::<_, Option<String>>(10)?
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let kind = kind(&json, &attachment);
    let text = match kind {
        Kind::Deleted => None,
        Kind::Document => {
            body.or_else(|| nonempty(attachment["fileName"].as_str().map(str::to_owned)))
        }
        Kind::Other => {
            body.or_else(|| nonempty(json["poll"]["question"].as_str().map(str::to_owned)))
        }
        _ => body,
    };
    let sender = if from_me {
        Person::me()
    } else {
        match source {
            Some(service) => dir.by_service(&service, contacts),
            // Messages from before Signal used account IDs carry only the phone number.
            None => crate::common::person(json["source"].as_str().map(str::to_owned), contacts),
        }
    };
    let mut reactions: Vec<(i64, Reaction)> = json["reactions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let emoji = r["emoji"].as_str().filter(|e| !e.is_empty())?.to_owned();
            let from = r["fromId"].as_str()?;
            let is_from_me = dir.my_conversation.as_deref() == Some(from);
            let sender = if is_from_me {
                Person::me()
            } else {
                dir.by_conversation(from, contacts)?
            };
            Some((
                r["timestamp"].as_i64().unwrap_or(0),
                Reaction {
                    emoji,
                    is_from_me,
                    sender,
                },
            ))
        })
        .collect();
    reactions.sort_by_key(|(at, _)| *at);
    let item = Message {
        source: Source::Signal,
        message_id: format!("{PREFIX}:{id}"),
        guid: row.get(1)?,
        conversation_id: format!("{PREFIX}:{chat}"),
        sent_at: timestamp(sent),
        is_from_me: from_me,
        // ReadStatus: 0 read, 1 unread, 2 viewed.
        is_read: (!from_me).then_some(read_status != Some(1)),
        sender,
        kind: Some(kind),
        text_status: if text.is_some() {
            TextStatus::Ok
        } else {
            TextStatus::NoText
        },
        text,
        reply_to_guid: row.get(9)?,
        reactions: reactions.into_iter().map(|(_, r)| r).collect(),
    };
    Ok((item, (sent, id)))
}

fn parse_cursor(cursor: Option<&str>, newest_first: bool) -> Result<(i64, i64)> {
    let Some(cursor) = cursor else {
        return Ok(if newest_first {
            (i64::MAX, i64::MAX)
        } else {
            (i64::MIN, 0)
        });
    };
    let (sent, id) = cursor.split_once(':').context("Invalid cursor")?;
    Ok((
        sent.parse().context("Invalid cursor")?,
        id.parse().context("Invalid cursor")?,
    ))
}

fn ensure_conversation(db: &Connection, id: &str) -> Result<()> {
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM conversations c WHERE c.id=? AND {CONVERSATION_FILTER})"
    );
    if !db.query_row(&sql, [id], |r| r.get::<_, bool>(0))? {
        bail!("Unknown conversation ID")
    }
    Ok(())
}

/// Every conversation the user has used (Signal also keeps one for each contact it has
/// heard of) whose newest message is at or after `active_after`.
fn conversations(
    dir: &Path,
    contacts: &Contacts,
    active_after: Option<DateTime<Utc>>,
) -> Result<Vec<Listed>> {
    let db = open(dir)?;
    let directory = Directory::load(&db)?;
    let mut stmt = db.prepare(&format!(
        "SELECT c.id, c.type, c.name, c.e164, c.serviceId, c.groupId, l.latest, COALESCE(json_extract(c.json,'$.unreadCount'),0), COALESCE(json_extract(c.json,'$.markedUnread'),0), COALESCE(json_extract(c.json,'$.isArchived'),0), json_extract(c.json,'$.membersV2') FROM conversations c LEFT JOIN (SELECT m.conversationId, MAX(m.sent_at) AS latest FROM messages m WHERE {MESSAGE_FILTER} GROUP BY m.conversationId) l ON l.conversationId=c.id WHERE {CONVERSATION_FILTER} AND (c.active_at IS NOT NULL OR l.latest IS NOT NULL) AND (?1 IS NULL OR l.latest>=?1)"
    ))?;
    let mut rows = stmt.query([millis(active_after)])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let group = row.get::<_, String>(1)? == "group";
        let latest: Option<i64> = row.get(6)?;
        let unread: i64 = row.get(7)?;
        let marked = row.get::<_, i64>(8)? != 0;
        let (name, identifier, participants) = if group {
            let members: Vec<Value> = row
                .get::<_, Option<String>>(10)?
                .and_then(|m| serde_json::from_str(&m).ok())
                .unwrap_or_default();
            let participants: Vec<Person> = members
                .iter()
                .filter_map(|m| m["aci"].as_str())
                .filter(|aci| directory.me.as_deref() != Some(&aci.to_lowercase()))
                .map(|aci| directory.by_service(aci, contacts))
                .collect();
            (row.get(2)?, row.get(5)?, participants)
        } else {
            let person = directory.by_conversation(&id, contacts);
            let identifier = nonempty(row.get(3)?)
                .or(nonempty(row.get::<_, Option<String>>(4)?).map(|s| s.to_lowercase()));
            let name = directory.profiles.get(&id).and_then(|p| p.name.clone());
            (name, identifier, person.into_iter().collect())
        };
        items.push(Listed {
            latest: latest.and_then(time),
            id: id.clone(),
            conversation: Conversation {
                source: Source::Signal,
                conversation_id: format!("{PREFIX}:{id}"),
                identifier,
                name,
                chat_type: Some(if group {
                    ChatType::Group
                } else {
                    ChatType::Direct
                }),
                participant_count: participants.len(),
                participants,
                latest_message_at: latest.and_then(timestamp),
                unread_count: match unread.max(0) as u64 {
                    0 if marked => 1,
                    n => n,
                },
                marked_unread: marked,
                archived: row.get::<_, i64>(9)? != 0,
                filtered: false,
            },
        });
    }
    Ok(items)
}

fn messages(
    dir: &Path,
    contacts: &Contacts,
    filter: &Filter,
    cursor: Option<&str>,
    cap: usize,
    budget: usize,
) -> Result<Page> {
    let cursor = parse_cursor(cursor, filter.newest_first)?;
    let (cmp, order) = if filter.newest_first {
        ("<", "DESC")
    } else {
        (">", "ASC")
    };
    let db = open(dir)?;
    if let Some(id) = &filter.conversation {
        ensure_conversation(&db, id)?;
    }
    let directory = Directory::load(&db)?;
    let sql = format!(
        "{MESSAGE_SELECT} WHERE (?1 IS NULL OR m.conversationId=?1) AND (?2 IS NULL OR m.sent_at>=?2) AND (?3 IS NULL OR m.sent_at<?3) AND (m.sent_at,m.rowid){cmp}(?4,?5) AND {MESSAGE_FILTER} AND {CONVERSATION_FILTER} ORDER BY m.sent_at {order},m.rowid {order} LIMIT ?6"
    );
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt.query(params![
        filter.conversation,
        millis(filter.after),
        millis(filter.before),
        cursor.0,
        cursor.1,
        (budget + 1) as i64
    ])?;
    page(
        &mut rows,
        filter,
        cursor,
        (cap, budget),
        |row| message(row, &directory, contacts),
        |(sent, id)| format!("{sent}:{id}"),
    )
}

fn context(
    dir: &Path,
    contacts: &Contacts,
    conversation: &str,
    message_id: i64,
    around: (usize, usize),
) -> Result<Vec<Message>> {
    let db = open(dir)?;
    let sent: i64 = db
        .query_row(
            &format!("SELECT m.sent_at FROM messages m JOIN conversations c ON c.id=m.conversationId WHERE m.rowid=? AND m.conversationId=? AND {MESSAGE_FILTER} AND {CONVERSATION_FILTER}"),
            params![message_id, conversation],
            |r| r.get(0),
        )
        .context("Message not found in conversation")?;
    let directory = Directory::load(&db)?;
    neighbors(
        &db,
        conversation,
        (sent, message_id),
        around,
        |op, order| {
            format!(
                "{MESSAGE_SELECT} WHERE m.conversationId=? AND {MESSAGE_FILTER} AND (m.sent_at,m.rowid) {op} (?,?) ORDER BY m.sent_at {order},m.rowid {order} LIMIT ?"
            )
        },
        |r| message(r, &directory, contacts).map(|x| x.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Made with Python's hashlib and the openssl command line, independently of this code.
    const ENCRYPTED_KEY: &str = "763130ce2f2e08a2c40fcef8ada844ce5f7ca21f73643b9c94cb2b739d764d275833e221d57792894590d56d8c7a4123425cae635ce001cd60f5380bbd5724e1775ebfa52ccbd89cf37f711e149ba613cacc90";

    #[test]
    fn decrypts_safe_storage_key() {
        let data = unhex(ENCRYPTED_KEY).unwrap();
        let key = decrypt_safe_storage(b"example-safe-storage-password", &data).unwrap();
        assert_eq!(key, "0123456789abcdef".repeat(4).as_bytes());
        assert!(decrypt_safe_storage(b"wrong", &data).is_err());
        assert!(decrypt_safe_storage(b"x", &data[3..]).is_err());
    }
}
