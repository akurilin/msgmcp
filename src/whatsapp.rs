//! WhatsApp Desktop backend (read-only).
//!
//! Reads the Core Data stores in WhatsApp's group container:
//! - `ChatStorage.sqlite`: chats (`ZWACHATSESSION`), messages (`ZWAMESSAGE`),
//!   group members, media items (captions, reply metadata) and receipt info (reactions).
//! - `LID.sqlite` and `ContactsV2.sqlite`: optional; map opaque `@lid` identities to
//!   phone numbers and WhatsApp's own contact names.
use crate::common::{APPLE_EPOCH, Contacts, Filter, Listed, Page, contact_name, e164, open};
use crate::types::{ChatType, Conversation, Kind, Message, Person, Reaction, Source, TextStatus};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::{collections::HashMap, path::Path};

pub const PREFIX: &str = "whatsapp";
const CHAT_STORAGE: &str = "ChatStorage.sqlite";

// ZSESSIONTYPE 0 direct, 1 group, 2 broadcast list, 4 community; 3 (status updates) is excluded.
const SESSION_FILTER: &str = "s.ZSESSIONTYPE IN (0,1,2,4) AND COALESCE(s.ZREMOVED,0)=0 AND s.ZCONTACTJID NOT LIKE '%@status'";
// ZMESSAGETYPE 6 (group events) and 10 (security/number-change notices) are system rows.
const MESSAGE_FILTER: &str = "m.ZMESSAGETYPE NOT IN (6,10)";

const MESSAGE_SELECT: &str = "SELECT m.Z_PK, m.ZSTANZAID, m.ZCHATSESSION, m.ZMESSAGEDATE, m.ZISFROMME, COALESCE(gm.ZMEMBERJID, m.ZFROMJID), m.ZTEXT, m.ZMESSAGETYPE, mi.ZTITLE, mi.ZMETADATA, i.ZRECEIPTINFO FROM ZWAMESSAGE m JOIN ZWACHATSESSION s ON s.Z_PK=m.ZCHATSESSION LEFT JOIN ZWAGROUPMEMBER gm ON gm.Z_PK=m.ZGROUPMEMBER LEFT JOIN ZWAMEDIAITEM mi ON mi.Z_PK=m.ZMEDIAITEM LEFT JOIN ZWAMESSAGEINFO i ON i.Z_PK=m.ZMESSAGEINFO";

fn chat_storage(dir: &Path) -> Result<Connection> {
    let path = dir.join(CHAT_STORAGE);
    if !path.exists() {
        bail!("WhatsApp database not found at {}", path.display())
    }
    open(&path)
}

/// One person in WhatsApp's address book: the name saved for their number on the user's
/// phone, as WhatsApp synced it.
pub struct AddressBookEntry {
    pub name: String,
    /// E.164.
    pub phone: Option<String>,
    /// Normalized JIDs: the phone JID and the `@lid` identity.
    pub jids: Vec<String>,
}

/// WhatsApp's address book, from `ContactsV2.sqlite`.
pub fn address_book(dir: &Path) -> Result<Vec<AddressBookEntry>> {
    let path = dir.join("ContactsV2.sqlite");
    if !path.exists() {
        bail!("WhatsApp address book not found at {}", path.display())
    }
    let db = open(&path)?;
    let mut stmt =
        db.prepare("SELECT ZWHATSAPPID, ZLID, ZPHONENUMBER, ZFULLNAME FROM ZWAADDRESSBOOKCONTACT")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (wa, lid, phone, name) = row?;
        let Some(name) = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) else {
            continue;
        };
        let jids: Vec<String> = [wa, lid]
            .into_iter()
            .flatten()
            .filter(|j| j.contains('@'))
            .map(|j| normalize_jid(&j))
            .collect();
        // The number is stored in E.164; a phone JID carries it too.
        let phone = phone.as_deref().and_then(e164).or_else(|| {
            jids.iter()
                .find_map(|j| j.strip_suffix("@s.whatsapp.net"))
                .and_then(|d| e164(&format!("+{}", digits(d))))
        });
        entries.push(AddressBookEntry { name, phone, jids });
    }
    Ok(entries)
}

/// Strip a device suffix: `123:4@s.whatsapp.net` → `123@s.whatsapp.net`.
fn normalize_jid(jid: &str) -> String {
    match jid.split_once('@') {
        Some((user, server)) => {
            format!("{}@{server}", user.split(':').next().unwrap_or(user))
        }
        None => jid.to_owned(),
    }
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// WhatsApp identity data: phone numbers for opaque JIDs and WhatsApp-side names.
#[derive(Default)]
struct Directory {
    /// Normalized JID → phone digits including country code.
    phones: HashMap<String, String>,
    /// Normalized JID → name, in priority order (first insert wins).
    names: HashMap<String, String>,
    /// Normalized JID → self-chosen WhatsApp profile name.
    push_names: HashMap<String, String>,
}

impl Directory {
    fn load(dir: &Path, db: &Connection) -> Directory {
        let mut d = Directory::default();
        // Each source is optional; a missing table or file only reduces name coverage.
        if let Ok(contacts) = open(&dir.join("ContactsV2.sqlite")) {
            d.collect(
                &contacts,
                "SELECT ZWHATSAPPID, ZLID, ZPHONENUMBER, ZFULLNAME FROM ZWAADDRESSBOOKCONTACT",
                |d,
                 row: (
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                )| {
                    let (wa, lid, phone, name) = row;
                    for jid in [wa, lid].into_iter().flatten() {
                        let jid = normalize_jid(&jid);
                        if let Some(phone) = phone.as_deref().map(digits).filter(|p| !p.is_empty())
                        {
                            d.phones.entry(jid.clone()).or_insert(phone);
                        }
                        if let Some(name) = name.clone().filter(|n| !n.is_empty()) {
                            d.names.entry(jid).or_insert(name);
                        }
                    }
                },
            );
        }
        if let Ok(lids) = open(&dir.join("LID.sqlite")) {
            d.collect(&lids, "SELECT ZIDENTIFIER, ZPHONENUMBER, NULL, NULL FROM ZWAZACCOUNT WHERE ZPHONENUMBER IS NOT NULL", |d, row: (Option<String>, Option<String>, Option<String>, Option<String>)| {
                if let (Some(jid), Some(phone)) = (row.0, row.1) {
                    let jid = if jid.contains('@') { jid } else { format!("{jid}@lid") };
                    let phone = digits(&phone);
                    if !phone.is_empty() {
                        d.phones.entry(normalize_jid(&jid)).or_insert(phone);
                    }
                }
            });
        }
        // Direct chats pair the contact's phone JID with its @lid identity.
        d.collect(db, "SELECT ZCONTACTJID, ZCONTACTIDENTIFIER, ZPARTNERNAME, NULL FROM ZWACHATSESSION WHERE ZSESSIONTYPE=0", |d, row: (Option<String>, Option<String>, Option<String>, Option<String>)| {
            let jids: Vec<String> = [row.0, row.1].into_iter().flatten().map(|j| normalize_jid(&j)).collect();
            let phone = jids.iter().find_map(|j| j.strip_suffix("@s.whatsapp.net").map(digits));
            for jid in &jids {
                if let Some(phone) = phone.clone().filter(|p| !p.is_empty()) {
                    d.phones.entry(jid.clone()).or_insert(phone);
                }
                if let Some(name) = row.2.clone().filter(|n| !n.is_empty()) {
                    d.names.entry(jid.clone()).or_insert(name);
                }
            }
        });
        d.collect(
            db,
            "SELECT ZMEMBERJID, ZCONTACTNAME, NULL, NULL FROM ZWAGROUPMEMBER",
            |d,
             row: (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            )| {
                if let (Some(jid), Some(name)) = (row.0, row.1.filter(|n| !n.is_empty())) {
                    d.names.entry(normalize_jid(&jid)).or_insert(name);
                }
            },
        );
        d.collect(
            db,
            "SELECT ZJID, ZPUSHNAME, NULL, NULL FROM ZWAPROFILEPUSHNAME",
            |d,
             row: (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            )| {
                if let (Some(jid), Some(name)) = (row.0, row.1.filter(|n| !n.is_empty())) {
                    d.push_names.entry(normalize_jid(&jid)).or_insert(name);
                }
            },
        );
        d
    }

    fn collect<F>(&mut self, db: &Connection, sql: &str, mut f: F)
    where
        F: FnMut(
            &mut Directory,
            (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        ),
    {
        let Ok(mut stmt) = db.prepare(sql) else {
            return;
        };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        else {
            return;
        };
        for row in rows.flatten() {
            f(self, row);
        }
    }

    /// Resolve a JID to `{address, jid, name, ambiguous}`. AddressBook names win; WhatsApp
    /// contact names, then profile (push) names, are fallbacks.
    fn person(&self, jid: &str, contacts: &Contacts) -> Person {
        let jid = normalize_jid(jid);
        let phone = jid
            .strip_suffix("@s.whatsapp.net")
            .map(digits)
            .or_else(|| self.phones.get(&jid).cloned())
            .filter(|p| !p.is_empty());
        let address = phone
            .as_ref()
            .map_or_else(|| jid.clone(), |p| format!("+{p}"));
        let (mut name, ambiguous) = match &phone {
            Some(_) => contact_name(&address, contacts),
            None => (None, false),
        };
        if name.is_none() && !ambiguous {
            let phone_jid = phone.as_ref().map(|p| format!("{p}@s.whatsapp.net"));
            name = [Some(&jid), phone_jid.as_ref()]
                .into_iter()
                .flatten()
                .find_map(|j| self.names.get(j).or_else(|| self.push_names.get(j)))
                .cloned();
        }
        Person {
            address: Some(address),
            jid: Some(jid),
            name,
            ambiguous,
        }
    }
}

fn time(seconds: f64) -> Option<DateTime<Utc>> {
    let whole = seconds.floor();
    let nanos = ((seconds - whole) * 1e9).round().min(999_999_999.0) as u32;
    DateTime::<Utc>::from_timestamp(whole as i64 + APPLE_EPOCH, nanos)
}

fn timestamp(seconds: f64) -> Option<String> {
    time(seconds).map(|t| t.to_rfc3339())
}

fn apple_seconds(t: Option<DateTime<Utc>>) -> Option<f64> {
    t.map(|t| (t.timestamp() - APPLE_EPOCH) as f64 + f64::from(t.timestamp_subsec_nanos()) / 1e9)
}

fn kind(message_type: i64) -> Kind {
    match message_type {
        0 => Kind::Text,
        1 => Kind::Image,
        2 => Kind::Video,
        3 => Kind::Audio,
        4 => Kind::Contact,
        5 => Kind::Location,
        7 => Kind::Link,
        8 => Kind::Document,
        11 => Kind::Gif,
        14 => Kind::Deleted,
        15 => Kind::Sticker,
        _ => Kind::Other,
    }
}

/// Iterate `(field, wire_type, value)` over one protobuf message. Varints come back as
/// `Varint`, length-delimited fields as `Bytes`; fixed-width fields are skipped.
enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn protobuf(mut data: &[u8]) -> Option<Vec<(u64, Field<'_>)>> {
    fn varint(data: &mut &[u8]) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = data.split_first()?;
            *data = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
    let mut fields = Vec::new();
    while !data.is_empty() {
        let key = varint(&mut data)?;
        let value = match key & 7 {
            0 => Field::Varint(varint(&mut data)?),
            2 => {
                let len = usize::try_from(varint(&mut data)?).ok()?;
                let (bytes, rest) = data.split_at_checked(len)?;
                data = rest;
                Field::Bytes(bytes)
            }
            1 => {
                data = data.get(8..)?;
                Field::Fixed
            }
            5 => {
                data = data.get(4..)?;
                Field::Fixed
            }
            _ => return None,
        };
        fields.push((key >> 3, value));
    }
    Some(fields)
}

/// ZWAMEDIAITEM.ZMETADATA field 5 holds the stanza ID of the quoted message.
fn reply_to(metadata: Option<&[u8]>) -> Option<String> {
    protobuf(metadata?)?
        .into_iter()
        .find_map(|(field, value)| match value {
            Field::Bytes(b) if field == 5 => std::str::from_utf8(b).ok().map(str::to_owned),
            _ => None,
        })
}

/// ZWAMESSAGEINFO.ZRECEIPTINFO repeated field 7 holds one reaction per reactor:
/// `{1: {2: sender JID (absent for me), 3: emoji, 4: timestamp ms}}`. Keep each
/// reactor's latest entry; an empty emoji means the reaction was removed.
fn reactions(blob: Option<&[u8]>) -> Vec<(Option<String>, String)> {
    let mut latest: HashMap<Option<String>, (u64, String)> = HashMap::new();
    for (field, value) in blob.and_then(protobuf).unwrap_or_default() {
        let Field::Bytes(outer) = value else { continue };
        if field != 7 {
            continue;
        }
        for (f1, entry) in protobuf(outer).unwrap_or_default() {
            let Field::Bytes(entry) = entry else { continue };
            if f1 != 1 {
                continue;
            }
            let (mut sender, mut emoji, mut ts) = (None, None, 0);
            for (f2, v2) in protobuf(entry).unwrap_or_default() {
                match (f2, v2) {
                    (2, Field::Bytes(b)) => sender = Some(String::from_utf8_lossy(b).into_owned()),
                    (3, Field::Bytes(b)) => emoji = Some(String::from_utf8_lossy(b).into_owned()),
                    (4, Field::Varint(v)) => ts = v,
                    _ => {}
                }
            }
            if let Some(emoji) = emoji
                && latest.get(&sender).is_none_or(|(old, _)| ts >= *old)
            {
                latest.insert(sender, (ts, emoji));
            }
        }
    }
    let mut out: Vec<_> = latest
        .into_iter()
        .filter(|(_, (_, e))| !e.is_empty())
        .map(|(s, (ts, e))| (ts, s, e))
        .collect();
    out.sort();
    out.into_iter().map(|(_, s, e)| (s, e)).collect()
}

fn message(
    row: &rusqlite::Row<'_>,
    dir: &Directory,
    contacts: &Contacts,
) -> rusqlite::Result<(Message, (f64, i64))> {
    let id: i64 = row.get(0)?;
    let chat: i64 = row.get(2)?;
    let date: f64 = row.get(3)?;
    let from_me: bool = row.get::<_, Option<bool>>(4)?.unwrap_or(false);
    let sender_jid: Option<String> = row.get(5)?;
    let kind = kind(row.get::<_, Option<i64>>(7)?.unwrap_or(-1));
    let body: Option<String> = row.get::<_, Option<String>>(6)?.filter(|t| !t.is_empty());
    let caption: Option<String> = row.get::<_, Option<String>>(8)?.filter(|t| !t.is_empty());
    // Captions and file names live in the media item; deleted messages expose no text.
    let text = match kind {
        Kind::Deleted => None,
        Kind::Image | Kind::Video | Kind::Gif | Kind::Document | Kind::Other => body.or(caption),
        _ => body,
    };
    let sender = if from_me {
        Person::me()
    } else {
        match sender_jid.filter(|j| !j.ends_with("@g.us") && !j.ends_with("@broadcast")) {
            Some(jid) => dir.person(&jid, contacts),
            None => Person {
                address: None,
                jid: None,
                name: None,
                ambiguous: false,
            },
        }
    };
    let metadata: Option<Vec<u8>> = row.get(9)?;
    let receipts: Option<Vec<u8>> = row.get(10)?;
    let reactions = reactions(receipts.as_deref())
        .into_iter()
        .map(|(jid, emoji)| Reaction {
            emoji,
            is_from_me: jid.is_none(),
            sender: jid.map_or_else(Person::me, |j| dir.person(&j, contacts)),
        })
        .collect();
    let item = Message {
        source: Source::Whatsapp,
        message_id: format!("{PREFIX}:{id}"),
        guid: row.get(1)?,
        conversation_id: format!("{PREFIX}:{chat}"),
        sent_at: timestamp(date),
        is_from_me: from_me,
        is_read: None,
        sender,
        kind: Some(kind),
        text_status: if text.is_some() {
            TextStatus::Ok
        } else {
            TextStatus::NoText
        },
        text,
        reply_to_guid: reply_to(metadata.as_deref()),
        reactions,
    };
    Ok((item, (date, id)))
}

fn parse_cursor(cursor: Option<&str>, newest_first: bool) -> Result<(f64, i64)> {
    let Some(cursor) = cursor else {
        return Ok(if newest_first {
            (f64::MAX, i64::MAX)
        } else {
            (f64::MIN, 0)
        });
    };
    let (date, id) = cursor.split_once(':').context("Invalid cursor")?;
    Ok((
        date.parse()
            .ok()
            .filter(|d: &f64| d.is_finite())
            .context("Invalid cursor")?,
        id.parse().context("Invalid cursor")?,
    ))
}

fn ensure_conversation(db: &Connection, id: i64) -> Result<()> {
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM ZWACHATSESSION s WHERE s.Z_PK=? AND {SESSION_FILTER})"
    );
    if !db.query_row(&sql, [id], |r| r.get::<_, bool>(0))? {
        bail!("Unknown conversation ID")
    }
    Ok(())
}

/// Every conversation whose newest message is at or after `active_after`. The newest
/// message is computed from the messages themselves: ZLASTMESSAGEDATE is not a reliable
/// time (pinned chats store a date far in the future) and counts excluded system rows.
/// ZUNREADCOUNT is -1 for a chat the user marked unread.
pub fn conversations(
    dir: &Path,
    contacts: &Contacts,
    active_after: Option<DateTime<Utc>>,
) -> Result<Vec<Listed>> {
    let db = chat_storage(dir)?;
    let directory = Directory::load(dir, &db);
    let mut stmt = db.prepare(&format!(
        "SELECT s.Z_PK, s.ZCONTACTJID, s.ZPARTNERNAME, s.ZSESSIONTYPE, l.latest, COALESCE(s.ZUNREADCOUNT, 0), COALESCE(s.ZARCHIVED, 0) FROM ZWACHATSESSION s LEFT JOIN (SELECT m.ZCHATSESSION, MAX(m.ZMESSAGEDATE) AS latest FROM ZWAMESSAGE m WHERE {MESSAGE_FILTER} GROUP BY m.ZCHATSESSION) l ON l.ZCHATSESSION=s.Z_PK WHERE {SESSION_FILTER} AND (?1 IS NULL OR l.latest>=?1)"
    ))?;
    let mut members = db.prepare(
        "SELECT ZMEMBERJID FROM ZWAGROUPMEMBER WHERE ZCHATSESSION=? AND COALESCE(ZISACTIVE,1)=1 AND ZMEMBERJID IS NOT NULL ORDER BY Z_PK",
    )?;
    let mut rows = stmt.query([apple_seconds(active_after)])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let jid: Option<String> = row.get(1)?;
        let session_type: i64 = row.get(3)?;
        let latest: Option<f64> = row.get(4)?;
        let unread: i64 = row.get(5)?;
        let participants: Vec<Person> = match session_type {
            0 => jid.iter().map(|j| directory.person(j, contacts)).collect(),
            1 | 4 => members
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .iter()
                .map(|j| directory.person(j, contacts))
                .collect(),
            _ => Vec::new(),
        };
        items.push(Listed {
            latest: latest.and_then(time),
            id,
            conversation: Conversation {
                source: Source::Whatsapp,
                conversation_id: format!("{PREFIX}:{id}"),
                identifier: jid,
                name: row.get(2)?,
                chat_type: Some(match session_type {
                    0 => ChatType::Direct,
                    1 => ChatType::Group,
                    2 => ChatType::Broadcast,
                    _ => ChatType::Community,
                }),
                participant_count: participants.len(),
                participants,
                latest_message_at: latest.and_then(timestamp),
                unread_count: if unread < 0 { 1 } else { unread as u64 },
                marked_unread: unread < 0,
                archived: row.get::<_, i64>(6)? != 0,
                filtered: false,
            },
        });
    }
    Ok(items)
}

pub fn messages(
    dir: &Path,
    contacts: &Contacts,
    filter: &Filter,
    cursor: Option<&str>,
    cap: usize,
    budget: usize,
) -> Result<Page> {
    let cursor = parse_cursor(cursor, filter.newest_first)?;
    let (cmp, dir_sql) = if filter.newest_first {
        ("<", "DESC")
    } else {
        (">", "ASC")
    };
    let db = chat_storage(dir)?;
    if let Some(id) = filter.conversation {
        ensure_conversation(&db, id)?;
    }
    let directory = Directory::load(dir, &db);
    let sql = format!(
        "{MESSAGE_SELECT} WHERE (?1 IS NULL OR m.ZCHATSESSION=?1) AND (?2 IS NULL OR m.ZMESSAGEDATE>=?2) AND (?3 IS NULL OR m.ZMESSAGEDATE<?3) AND (m.ZMESSAGEDATE,m.Z_PK){cmp}(?4,?5) AND {MESSAGE_FILTER} AND {SESSION_FILTER} ORDER BY m.ZMESSAGEDATE {dir_sql},m.Z_PK {dir_sql} LIMIT ?6"
    );
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt.query(params![
        filter.conversation,
        apple_seconds(filter.after),
        apple_seconds(filter.before),
        cursor.0,
        cursor.1,
        (budget + 1) as i64
    ])?;
    let mut page = Page {
        items: Vec::new(),
        scanned: 0,
        decode_failures: 0,
        next: None,
    };
    let mut last = cursor;
    while let Some(row) = rows.next()? {
        if page.scanned == budget || page.items.len() == cap {
            page.next = Some(format!("{}:{}", last.0, last.1));
            break;
        }
        page.scanned += 1;
        let (item, key) = message(row, &directory, contacts)?;
        last = key;
        if filter.matches(&item) {
            page.items.push(item);
        }
    }
    Ok(page)
}

pub fn context(
    dir: &Path,
    contacts: &Contacts,
    conversation: i64,
    message_id: i64,
    before: usize,
    after: usize,
) -> Result<Vec<Message>> {
    let db = chat_storage(dir)?;
    let date: f64 = db
        .query_row(
            &format!("SELECT m.ZMESSAGEDATE FROM ZWAMESSAGE m JOIN ZWACHATSESSION s ON s.Z_PK=m.ZCHATSESSION WHERE m.Z_PK=? AND m.ZCHATSESSION=? AND m.ZMESSAGEDATE IS NOT NULL AND {SESSION_FILTER}"),
            params![message_id, conversation],
            |r| r.get(0),
        )
        .context("Message not found in conversation")?;
    let directory = Directory::load(dir, &db);
    let mut values = Vec::new();
    for (op, order, count) in [("<", "DESC", before), (">=", "ASC", after + 1)] {
        let sql = format!(
            "{MESSAGE_SELECT} WHERE m.ZCHATSESSION=? AND {MESSAGE_FILTER} AND (m.ZMESSAGEDATE,m.Z_PK) {op} (?,?) ORDER BY m.ZMESSAGEDATE {order},m.Z_PK {order} LIMIT ?"
        );
        let mut stmt = db.prepare(&sql)?;
        let mut part = stmt
            .query_map(params![conversation, date, message_id, count as i64], |r| {
                message(r, &directory, contacts)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if op == "<" {
            part.reverse();
        }
        values.extend(part.into_iter().map(|x| x.0));
    }
    Ok(values)
}
