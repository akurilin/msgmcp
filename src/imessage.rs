//! iMessage/SMS backend: `~/Library/Messages/chat.db` (read-only) and Messages automation.
use crate::common::{APPLE_EPOCH, Contacts, Filter, Listed, Page, number, open, person};
use crate::types::{
    Conversation, Message, Person, SendReceipt, SendStatus, Service, Source, TextStatus,
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::{path::Path, process::Command};

const NANOS_PER_SECOND: i64 = 1_000_000_000;
pub const PREFIX: &str = "imessage";

// Pass data as arguments so message text cannot become AppleScript code.
const SEND_SCRIPT: &str = r#"on run argv
    set destination to item 1 of argv
    set messageText to item 2 of argv
    with timeout of 30 seconds
        tell application "Messages"
            set targetAccount to first account whose service type is iMessage
            set targetParticipant to participant destination of targetAccount
            send messageText to targetParticipant
        end tell
    end timeout
end run"#;

// Require an address to prevent an ambiguous name from selecting the recipient.
pub fn send(recipient: &str, text: &str) -> Result<SendReceipt> {
    if text.trim().is_empty() {
        bail!("text must not be empty")
    }
    let recipient = recipient.trim().to_owned();
    let phone = recipient.starts_with('+')
        && recipient.len() > 1
        && recipient[1..].chars().all(|c| c.is_ascii_digit());
    let email = recipient.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && !domain.is_empty() && !recipient.chars().any(char::is_whitespace)
    });
    if !phone && !email {
        bail!("recipient must be an exact phone number with country code or email address")
    }
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", SEND_SCRIPT, "--", &recipient, text])
        .output()
        .context("Cannot start Messages automation")?;
    if !output.status.success() {
        // An automation failure can occur after submission; never retry automatically.
        bail!(
            "Messages automation failed; submission is unknown. Check Messages before retrying. {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(SendReceipt {
        status: SendStatus::Submitted,
        service: Service::Imessage,
        recipient,
    })
}

fn time(date: i64) -> Option<DateTime<Utc>> {
    let seconds = date.div_euclid(NANOS_PER_SECOND) + APPLE_EPOCH;
    DateTime::<Utc>::from_timestamp(seconds, date.rem_euclid(NANOS_PER_SECOND) as u32)
}

fn timestamp(date: i64) -> Option<String> {
    time(date).map(|t| t.to_rfc3339())
}

fn apple_nanos(t: Option<DateTime<Utc>>) -> Result<Option<i64>> {
    t.map(|t| {
        t.timestamp()
            .checked_sub(APPLE_EPOCH)
            .and_then(|x| x.checked_mul(NANOS_PER_SECOND))
            .and_then(|x| x.checked_add(t.timestamp_subsec_nanos() as i64))
            .context("Date is outside the supported range")
    })
    .transpose()
}

fn decode(text: Option<String>, blob: Option<Vec<u8>>) -> (Option<String>, TextStatus) {
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        return (Some(text), TextStatus::Ok);
    }
    let Some(blob) = blob else {
        return (None, TextStatus::NoText);
    };
    // Read the attributed string's backing text with a typedstream parser.
    let mut decoder = crabstep::TypedStreamDeserializer::new(&blob);
    if let Ok(mut properties) = decoder.iter_root()
        && let Some(text) = properties.find_map(|p| p.as_string().map(str::to_owned))
    {
        return (Some(text), TextStatus::Ok);
    }
    (None, TextStatus::DecodeFailed)
}

// Real messages only: reactions (associated_message_type) and events such as group changes
// (item_type other than 0) have no text of their own, and Messages never marks events read.
const REAL_MESSAGE: &str = "COALESCE(m.associated_message_type,0)=0 AND m.item_type=0";
const MESSAGE_SELECT: &str = "SELECT m.ROWID,m.guid,cmj.chat_id,m.date,m.is_from_me,h.id,m.text,m.attributedBody,m.thread_originator_guid,m.is_read FROM message m JOIN chat_message_join cmj ON cmj.message_id=m.ROWID LEFT JOIN handle h ON h.ROWID=m.handle_id";

fn message(
    row: &rusqlite::Row<'_>,
    contacts: &Contacts,
) -> rusqlite::Result<(Message, (i64, i64, i64))> {
    let id: i64 = row.get(0)?;
    let chat: i64 = row.get(2)?;
    let date: i64 = row.get(3)?;
    let from_me: bool = row.get(4)?;
    let (text, status) = decode(row.get(6)?, row.get(7)?);
    let sender = if from_me {
        Person::me()
    } else {
        person(row.get(5)?, contacts)
    };
    Ok((
        Message {
            source: Source::Imessage,
            message_id: format!("{PREFIX}:{id}"),
            guid: row.get(1)?,
            conversation_id: format!("{PREFIX}:{chat}"),
            sent_at: timestamp(date),
            is_from_me: from_me,
            is_read: (!from_me).then(|| row.get(9)).transpose()?,
            sender,
            kind: None,
            text,
            text_status: status,
            reply_to_guid: row.get(8)?,
            reactions: Vec::new(),
        },
        (date, id, chat),
    ))
}

fn parse_cursor(cursor: Option<&str>, newest_first: bool) -> Result<(i64, i64, i64)> {
    let Some(cursor) = cursor else {
        return Ok(if newest_first {
            (i64::MAX, i64::MAX, i64::MAX)
        } else {
            (i64::MIN, 0, 0)
        });
    };
    let parts: Vec<_> = cursor
        .split(':')
        .map(str::parse::<i64>)
        .collect::<std::result::Result<_, _>>()
        .context("Invalid cursor")?;
    match parts[..] {
        [date, id, chat] => Ok((date, id, chat)),
        _ => bail!("Invalid cursor"),
    }
}

fn ensure_conversation(db: &Connection, id: i64) -> Result<()> {
    if !db.query_row(
        "SELECT EXISTS(SELECT 1 FROM chat WHERE ROWID=?)",
        [id],
        |r| r.get::<_, bool>(0),
    )? {
        bail!("Unknown conversation ID")
    }
    Ok(())
}

/// Every conversation whose newest real message is at or after `active_after`, with its
/// unread incoming messages, matching what read_conversation returns.
pub fn conversations(
    path: &Path,
    contacts: &Contacts,
    active_after: Option<DateTime<Utc>>,
) -> Result<Vec<Listed>> {
    let db = open(path)?;
    let mut stmt = db.prepare(&format!("SELECT c.ROWID,c.chat_identifier,c.display_name,l.latest,COALESCE(l.unread,0),COALESCE(c.is_archived,0),COALESCE(c.is_filtered,0) FROM chat c LEFT JOIN (SELECT j.chat_id,MAX(m.date) AS latest,SUM(m.is_from_me=0 AND m.is_read=0) AS unread FROM chat_message_join j JOIN message m ON m.ROWID=j.message_id WHERE {REAL_MESSAGE} GROUP BY j.chat_id) l ON l.chat_id=c.ROWID WHERE ?1 IS NULL OR l.latest>=?1"))?;
    let mut handles = db.prepare("SELECT h.id FROM chat_handle_join j JOIN handle h ON h.ROWID=j.handle_id WHERE j.chat_id=? ORDER BY h.ROWID")?;
    let mut rows = stmt.query([apple_nanos(active_after)?])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let latest: Option<i64> = row.get(3)?;
        let participants: Vec<Person> = handles
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|a| person(Some(a), contacts))
            .collect();
        items.push(Listed {
            latest: latest.and_then(time),
            id: id.to_string(),
            conversation: Conversation {
                source: Source::Imessage,
                conversation_id: format!("{PREFIX}:{id}"),
                identifier: row.get(1)?,
                name: row.get(2)?,
                chat_type: None,
                participant_count: participants.len(),
                participants,
                latest_message_at: latest.and_then(timestamp),
                unread_count: row.get::<_, i64>(4)?.max(0) as u64,
                marked_unread: false,
                archived: row.get::<_, i64>(5)? != 0,
                filtered: row.get::<_, i64>(6)? != 0,
            },
        });
    }
    Ok(items)
}

pub fn messages(
    path: &Path,
    contacts: &Contacts,
    filter: &Filter,
    cursor: Option<&str>,
    cap: usize,
    budget: usize,
) -> Result<Page> {
    let cursor = parse_cursor(cursor, filter.newest_first)?;
    let (after, before) = (apple_nanos(filter.after)?, apple_nanos(filter.before)?);
    let (cmp, dir) = if filter.newest_first {
        ("<", "DESC")
    } else {
        (">", "ASC")
    };
    let db = open(path)?;
    let conversation = filter.conversation.as_deref().map(number).transpose()?;
    if let Some(id) = conversation {
        ensure_conversation(&db, id)?;
    }
    let sql = format!(
        "{MESSAGE_SELECT} WHERE (?1 IS NULL OR cmj.chat_id=?1) AND (?2 IS NULL OR m.date>=?2) AND (?3 IS NULL OR m.date<?3) AND (m.date,m.ROWID,cmj.chat_id){cmp}(?4,?5,?6) AND {REAL_MESSAGE} ORDER BY m.date {dir},m.ROWID {dir},cmj.chat_id {dir} LIMIT ?7"
    );
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt.query(params![
        conversation,
        after,
        before,
        cursor.0,
        cursor.1,
        cursor.2,
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
            page.next = Some(format!("{}:{}:{}", last.0, last.1, last.2));
            break;
        }
        page.scanned += 1;
        let (item, key) = message(row, contacts)?;
        last = key;
        if item.text_status == TextStatus::DecodeFailed {
            page.decode_failures += 1;
        }
        if filter.matches(&item) {
            page.items.push(item);
        }
    }
    Ok(page)
}

pub fn context(
    path: &Path,
    contacts: &Contacts,
    conversation: i64,
    message_id: i64,
    before: usize,
    after: usize,
) -> Result<Vec<Message>> {
    let db = open(path)?;
    let date: i64 = db
        .query_row(
            &format!("SELECT m.date FROM message m JOIN chat_message_join j ON j.message_id=m.ROWID WHERE m.ROWID=? AND j.chat_id=? AND {REAL_MESSAGE}"),
            params![message_id, conversation],
            |r| r.get(0),
        )
        .context("Message not found in conversation")?;
    let mut values = Vec::new();
    for (op, order, count) in [("<", "DESC", before), (">=", "ASC", after + 1)] {
        let sql = format!(
            "{MESSAGE_SELECT} WHERE cmj.chat_id=? AND {REAL_MESSAGE} AND (m.date,m.ROWID) {op} (?,?) ORDER BY m.date {order},m.ROWID {order} LIMIT ?"
        );
        let mut stmt = db.prepare(&sql)?;
        let mut part = stmt
            .query_map(params![conversation, date, message_id, count as i64], |r| {
                message(r, contacts)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if op == "<" {
            part.reverse();
        }
        values.extend(part.into_iter().map(|x| x.0));
    }
    Ok(values)
}
