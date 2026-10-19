//! Helpers shared by the message store backends.
use crate::types::{Conversation, Message, Person};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

pub const APPLE_EPOCH: i64 = 978_307_200;
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;
pub const SCAN_LIMIT: usize = 10_000;
pub const MAX_CONTEXT: usize = 100;
pub const MAX_PARTICIPANTS: usize = 50;
const SQLITE_TIMEOUT_SECONDS: u64 = 2;

/// Normalized address → contact names from the macOS AddressBook.
pub type Contacts = BTreeMap<String, BTreeSet<String>>;

pub fn open(path: &Path) -> Result<Connection> {
    let db =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| {
            format!(
                "Cannot read {}. Check the path and macOS Full Disk Access.",
                path.display()
            )
        })?;
    db.busy_timeout(Duration::from_secs(SQLITE_TIMEOUT_SECONDS))?;
    Ok(db)
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// True for text that can only be a phone number: digits with formatting, no letters or `@`.
fn phone_like(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit()) && !s.chars().any(|c| c.is_alphabetic() || c == '@')
}

/// A phone number in E.164 (`+15551234567`) when its country is known: it starts with `+`,
/// or it is North American, which Messages assumes for a number without a country code
/// (`(555) 123-4567`, `1 555 123 4567`). Other national formats are left alone rather
/// than guessed.
pub fn e164(address: &str) -> Option<String> {
    if !phone_like(address) {
        return None;
    }
    let d = digits(address);
    let plus = address
        .trim_start_matches(|c: char| !c.is_ascii_digit() && c != '+')
        .starts_with('+');
    let nanp = |n: &str| n.len() == 10 && !n.starts_with(['0', '1']);
    if plus {
        (7..=15).contains(&d.len()).then(|| format!("+{d}"))
    } else if nanp(&d) {
        Some(format!("+1{d}"))
    } else if d.starts_with('1') && nanp(&d[1..]) {
        Some(format!("+{d}"))
    } else {
        None
    }
}

/// The form every address comparison uses, so a number matches in any format: E.164 digits
/// for phone numbers (`15551234567`), the digits of other numbers such as short codes, and
/// lowercase for email addresses, JIDs and sender IDs.
pub fn address_key(address: &str) -> String {
    match e164(address) {
        Some(number) => number[1..].to_owned(),
        None if phone_like(address) => digits(address),
        None => address.trim().to_lowercase(),
    }
}

/// An address as results report it: phone numbers in E.164 when the country is known,
/// anything else unchanged.
pub fn display_address(address: &str) -> String {
    e164(address).unwrap_or_else(|| address.to_owned())
}

/// Substring match of a list_conversations query against an address: a query that looks
/// like a phone number matches by digits, ignoring formatting and the country code.
pub fn address_contains(address: &str, query: &str) -> bool {
    address.to_lowercase().contains(query)
        || (phone_like(query)
            && phone_like(address)
            && address_key(address).contains(&digits(query)))
}

pub fn load_contacts(root: &Path) -> (Contacts, usize) {
    let (records, readable) = load_contact_records(root);
    let mut contacts = Contacts::new();
    for record in records {
        for address in &record.addresses {
            let key = address_key(address);
            if !key.is_empty() {
                contacts.entry(key).or_default().insert(record.name.clone());
            }
        }
    }
    (contacts, readable)
}

/// One person saved in macOS Contacts, with every phone number and email address saved
/// for them, as saved.
pub struct ContactRecord {
    pub name: String,
    pub addresses: Vec<String>,
}

/// Every AddressBook record that has a name and an address, and how many AddressBook
/// databases were read. Each account (iCloud, Google, On My Mac) has its own database.
pub fn load_contact_records(root: &Path) -> (Vec<ContactRecord>, usize) {
    let mut records = Vec::new();
    let mut readable = 0;
    let pattern = root
        .join("Sources/*/AddressBook-v22.abcddb")
        .to_string_lossy()
        .into_owned();
    let mut paths: Vec<_> = glob::glob(&pattern)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    let primary = root.join("AddressBook-v22.abcddb");
    if primary.exists() {
        paths.push(primary);
    }
    for path in paths {
        let Ok(db) = open(&path) else { continue };
        let sql = "SELECT r.Z_PK, p.ZFULLNUMBER, r.ZFIRSTNAME, r.ZLASTNAME, r.ZNICKNAME, r.ZORGANIZATION FROM ZABCDPHONENUMBER p JOIN ZABCDRECORD r ON p.ZOWNER=r.Z_PK UNION ALL SELECT r.Z_PK, e.ZADDRESS, r.ZFIRSTNAME, r.ZLASTNAME, r.ZNICKNAME, r.ZORGANIZATION FROM ZABCDEMAILADDRESS e JOIN ZABCDRECORD r ON e.ZOWNER=r.Z_PK";
        let Ok(mut stmt) = db.prepare(sql) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        }) else {
            continue;
        };
        readable += 1;
        let mut by_record = BTreeMap::<Option<i64>, ContactRecord>::new();
        for (id, address, first, last, nickname, organization) in rows.flatten() {
            let Some(address) = address.filter(|a| !address_key(a).is_empty()) else {
                continue;
            };
            let full = format!("{} {}", first.unwrap_or_default(), last.unwrap_or_default())
                .trim()
                .to_owned();
            let name = if !full.is_empty() {
                full
            } else {
                nickname.or(organization).unwrap_or_default()
            };
            if !name.is_empty() {
                by_record
                    .entry(id)
                    .or_insert_with(|| ContactRecord {
                        name,
                        addresses: Vec::new(),
                    })
                    .addresses
                    .push(address);
            }
        }
        records.extend(by_record.into_values());
    }
    (records, readable)
}

/// Names saved in Contacts for `address`, in whatever format either side has it.
fn lookup<'a>(address: &str, contacts: &'a Contacts) -> Option<&'a BTreeSet<String>> {
    let key = address_key(address);
    (!key.is_empty()).then(|| contacts.get(&key)).flatten()
}

/// True when `address` is saved in Contacts.
pub fn is_contact(address: &str, contacts: &Contacts) -> bool {
    lookup(address, contacts).is_some()
}

/// AddressBook lookup: `(name, ambiguous)`. A name is returned only for a unique match.
pub fn contact_name(address: &str, contacts: &Contacts) -> (Option<String>, bool) {
    match lookup(address, contacts) {
        Some(names) if names.len() == 1 => (names.first().cloned(), false),
        Some(_) => (None, true),
        None => (None, false),
    }
}

pub fn person(address: Option<String>, contacts: &Contacts) -> Person {
    let (name, ambiguous) = address
        .as_deref()
        .map_or((None, false), |a| contact_name(a, contacts));
    Person {
        address: address.as_deref().map(display_address),
        jid: None,
        signal_id: None,
        name,
        ambiguous,
    }
}

/// A backend-local ID that the store keeps as an integer.
pub fn number(id: &str) -> Result<i64> {
    id.parse().ok().context("Unknown conversation ID")
}

pub fn limit(value: Option<usize>) -> Result<usize> {
    let value = value.unwrap_or(DEFAULT_LIMIT);
    if value == 0 || value > MAX_LIMIT {
        bail!("limit must be between 1 and {MAX_LIMIT}");
    }
    Ok(value)
}

pub fn parse_time(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    value
        .map(|s| {
            DateTime::parse_from_rfc3339(s)
                .map(|t| t.with_timezone(&Utc))
                .context("Use RFC 3339 dates with a time zone")
        })
        .transpose()
}

/// Message filters shared by search_messages and read_conversation.
pub struct Filter {
    /// Backend-local conversation ID.
    pub conversation: Option<String>,
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    /// Lowercased literal substring.
    pub text: Option<String>,
    /// Lowercased address, JID, contact-name substring, or "me".
    pub sender: Option<String>,
    /// Page backwards from the newest message instead of forwards from the oldest.
    pub newest_first: bool,
}

impl Filter {
    pub fn matches(&self, item: &Message) -> bool {
        let sender_matches = self.sender.as_ref().is_none_or(|s| {
            if s == "me" {
                return item.is_from_me;
            }
            let key = address_key(s);
            let sender = &item.sender;
            !item.is_from_me
                && ([&sender.address, &sender.jid, &sender.signal_id]
                    .into_iter()
                    .flatten()
                    .any(|a| a.to_lowercase() == *s || (!key.is_empty() && address_key(a) == key))
                    || sender
                        .name
                        .as_ref()
                        .is_some_and(|n| n.to_lowercase().contains(s)))
        });
        let text_matches = self.text.as_ref().is_none_or(|q| {
            item.text
                .as_ref()
                .is_some_and(|t| t.to_lowercase().contains(q))
        });
        sender_matches && text_matches
    }
}

/// One backend's share of a paginated response.
pub struct Page {
    pub items: Vec<Message>,
    pub scanned: usize,
    pub decode_failures: usize,
    /// Backend-local continuation cursor; `None` when this backend is exhausted.
    pub next: Option<String>,
}

/// A conversation with what list_conversations sorts on.
pub struct Listed {
    /// Time of the newest message; `None` for a conversation without messages.
    pub latest: Option<DateTime<Utc>>,
    /// Backend-local conversation ID.
    pub id: String,
    pub conversation: Conversation,
}

/// Case-insensitive match on name, identifier, and every participant; on a match, cap
/// the participant list at `MAX_PARTICIPANTS` (the count keeps the full number).
pub fn keep_conversation(item: &mut Conversation, query: &str) -> bool {
    let hit = |v: &Option<String>| v.as_ref().is_some_and(|s| s.to_lowercase().contains(query));
    let address = |v: &Option<String>| v.as_ref().is_some_and(|s| address_contains(s, query));
    let matched = query.is_empty()
        || address(&item.identifier)
        || hit(&item.name)
        || item
            .participants
            .iter()
            .any(|p| address(&p.address) || hit(&p.name) || hit(&p.jid) || hit(&p.signal_id));
    if matched {
        item.participants.truncate(MAX_PARTICIPANTS);
    }
    matched
}
