//! Typed tool inputs' shared enums and every tool result. Each result type's JSON schema is
//! published as the tool's `outputSchema`, so the doc comments here are the reference for
//! result fields; clients see them through `tools/list` or `msgmcp schema`.
//!
//! Enumerated result fields are plain strings in the schema, with their values listed in the
//! field's description: a client that checks results against a schema it fetched earlier
//! must not fail when a new store or kind of message adds a value.
use crate::{imessage, signal, telegram, whatsapp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Message store.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Imessage,
    Whatsapp,
    Signal,
    Telegram,
}
pub const ALL_SOURCES: [Source; 4] = [
    Source::Imessage,
    Source::Whatsapp,
    Source::Signal,
    Source::Telegram,
];

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Imessage => imessage::PREFIX,
            Source::Whatsapp => whatsapp::PREFIX,
            Source::Signal => signal::PREFIX,
            Source::Telegram => telegram::PREFIX,
        })
    }
}

impl Source {
    pub fn parse(prefix: &str) -> Option<Source> {
        ALL_SOURCES.into_iter().find(|s| s.to_string() == prefix)
    }
}

/// A message sender, reactor or conversation participant.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Person {
    /// Phone number in E.164 (`+15551234567`), or email address. WhatsApp gives the JID when no
    /// phone number is known. Null for the user ("me"), for unknown senders, for Signal
    /// accounts whose number is hidden (see `signal_id`), and for Telegram accounts whose
    /// number the user can't see (see `telegram_id`).
    pub address: Option<String>,
    /// WhatsApp only: normalized JID, such as `<digits>@s.whatsapp.net` or an opaque `<id>@lid`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jid: Option<String>,
    /// Signal only: the account's Signal ID (ACI), a lowercase UUID. Signal knows many people
    /// only by this ID, since phone numbers can be hidden.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal_id: Option<String>,
    /// Telegram only: the account's numeric Telegram ID (a channel's, for posts in a
    /// channel's name), as a string. Telegram shows most people's phone numbers only to their
    /// contacts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telegram_id: Option<String>,
    /// Telegram only: the account's public username, without the `@`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telegram_username: Option<String>,
    /// "me" for the user. Otherwise the AddressBook name; for WhatsApp, then the WhatsApp
    /// contact name, then the person's WhatsApp profile name; for Signal, then the nickname
    /// the user gave them in Signal, the name from the user's phone contacts, the person's
    /// Signal profile name, then their username; for Telegram, then the name Telegram shows
    /// (the name saved in the user's Telegram contacts, else the person's own), then their
    /// username, and for a channel posting in its own name, the channel title. Null when
    /// unknown or ambiguous.
    pub name: Option<String>,
    /// True when several AddressBook contacts share this address, so no name is given.
    /// Contacts match in any format; a number saved without a country code is taken as
    /// North American (+1).
    pub ambiguous: bool,
}

impl Person {
    /// The store-specific identifiers a sender filter or conversation query can name: WhatsApp
    /// JID, Signal ID, or Telegram user ID or username. The phone number or email address is
    /// `address`.
    pub fn ids(&self) -> impl Iterator<Item = &String> {
        [
            &self.jid,
            &self.signal_id,
            &self.telegram_id,
            &self.telegram_username,
        ]
        .into_iter()
        .flatten()
    }

    pub fn me() -> Person {
        Person {
            address: None,
            jid: None,
            signal_id: None,
            telegram_id: None,
            telegram_username: None,
            name: Some("me".to_owned()),
            ambiguous: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextStatus {
    /// `text` holds the message text.
    Ok,
    /// The message has no text, e.g. an attachment without a caption or a deleted message.
    NoText,
    /// iMessage and Telegram: the stored message could not be decoded.
    DecodeFailed,
}

/// WhatsApp, Signal and Telegram message type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Link,
    Image,
    Video,
    Audio,
    Gif,
    Sticker,
    Document,
    Contact,
    Location,
    Deleted,
    Other,
}

/// WhatsApp, Signal and Telegram chat type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChatType {
    Direct,
    Group,
    Broadcast,
    Community,
    Channel,
}

/// One person's current reaction to a WhatsApp, Signal or Telegram message.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Reaction {
    pub emoji: String,
    pub is_from_me: bool,
    pub sender: Person,
}

/// A message. iMessage reactions, events (such as group changes), edit history and recall
/// state are not returned; WhatsApp group events and security notices are excluded; Signal
/// events (group changes, safety-number changes, calls, timer changes) and stories are
/// excluded, and an edited Signal message has its latest text. Telegram service messages
/// (joins, group changes, pins, calls) and scheduled messages are excluded, an edited
/// message has its latest text, and only the history Telegram for macOS has cached is read:
/// older messages are often missing.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Message {
    /// Message store: "imessage", "whatsapp", "signal" or "telegram". Other stores may be
    /// added.
    #[schemars(with = "String")]
    pub source: Source,
    /// Source-qualified message ID, e.g. "imessage:85544", "whatsapp:9120", "signal:310" or
    /// "telegram:4416.0.812". Pass it back exactly as returned.
    pub message_id: String,
    /// The store's own message ID (iMessage GUID, WhatsApp stanza ID, Signal message UUID, or
    /// Telegram's message number within the chat); other messages' `reply_to_guid` refers
    /// to it.
    pub guid: Option<String>,
    /// Source-qualified conversation ID, e.g. "imessage:2771", "whatsapp:45", "signal:<uuid>"
    /// or "telegram:4416".
    pub conversation_id: String,
    /// RFC 3339 time in UTC. Signal uses the sender's time.
    pub sent_at: Option<String>,
    pub is_from_me: bool,
    /// iMessage, Signal and Telegram, present only on incoming messages: whether the user has
    /// read it (iMessage: on any of their Apple devices; Signal and Telegram: as the desktop
    /// app last synced).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_read: Option<bool>,
    pub sender: Person,
    /// WhatsApp, Signal and Telegram only: "text", "link", "image", "video", "audio", "gif",
    /// "sticker", "document", "contact", "location", "deleted" or "other". Other kinds may be
    /// added.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub kind: Option<Kind>,
    /// Message text. iMessage attachments are not read, so an attachment-only message is the
    /// object replacement character U+FFFC. WhatsApp media text is the caption or file name;
    /// Signal and Telegram media text is the message body, or a document's file name, and for
    /// a Telegram contact card the contact's name; deleted messages have none.
    pub text: Option<String>,
    /// "ok": `text` holds the message text. "no_text": the message has none, such as an
    /// attachment without a caption or a deleted message. "decode_failed", iMessage and
    /// Telegram: the stored message could not be decoded. Other values may be added.
    #[schemars(with = "String")]
    pub text_status: TextStatus,
    /// `guid` of the message this one replies to. iMessage uses the thread originator, which
    /// can be the thread root; WhatsApp, Signal and Telegram use the quoted message (Telegram:
    /// only a message in the same chat).
    pub reply_to_guid: Option<String>,
    /// WhatsApp, Signal and Telegram only, present only when the message has reactions: each
    /// person's current reaction, oldest first. Telegram lists the recent reactions it shows
    /// with names, which in large groups is not everyone's, and leaves out custom emoji.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
}

/// A conversation. WhatsApp status updates and removed chats are excluded, as are Signal
/// conversations the user removed or never used (Signal keeps one for every account it has
/// heard of), and Telegram chats with nothing but service messages, such as "joined
/// Telegram" notices, unless pinned.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Conversation {
    /// Message store: "imessage", "whatsapp", "signal" or "telegram". Other stores may be
    /// added.
    #[schemars(with = "String")]
    pub source: Source,
    /// Source-qualified conversation ID, e.g. "imessage:2771", "whatsapp:45", "signal:<uuid>"
    /// or "telegram:4416". Pass it back exactly as returned.
    pub conversation_id: String,
    /// iMessage chat identifier (phone, email or group ID), WhatsApp chat JID, for Signal the
    /// phone number (else Signal ID) of a direct chat or the group ID, and for Telegram the
    /// phone number (else @username, else Telegram user ID) of a direct chat, or a public
    /// group's or channel's @username.
    pub identifier: Option<String>,
    /// Chat display name; empty or null for most direct chats.
    pub name: Option<String>,
    /// WhatsApp, Signal and Telegram only: "direct", "group", for WhatsApp "broadcast" or
    /// "community", or for Telegram "channel". Other types may be added.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub chat_type: Option<ChatType>,
    /// Other participants, at most 50. WhatsApp and Signal groups list current members only;
    /// Telegram groups list the people who wrote in the cached messages, and channels none.
    pub participants: Vec<Person>,
    /// Number of participants before the 50-entry cap.
    pub participant_count: usize,
    /// RFC 3339 time in UTC of the newest message, as read_conversation returns them (iMessage
    /// reactions and events, and WhatsApp, Signal and Telegram system events, don't count).
    /// Null when there are no messages.
    pub latest_message_at: Option<String>,
    /// Unread incoming messages; 0 when everything is read. iMessage counts messages not yet
    /// read on any of the user's Apple devices. WhatsApp, Signal and Telegram use the desktop
    /// app's count, which can lag behind the phone; a chat the user marked unread counts as 1.
    pub unread_count: u64,
    /// WhatsApp, Signal and Telegram only, present only when true: the user marked the chat
    /// unread by hand.
    #[serde(skip_serializing_if = "is_false")]
    pub marked_unread: bool,
    /// Present only when true: the user archived the chat.
    #[serde(skip_serializing_if = "is_false")]
    pub archived: bool,
    /// iMessage and Telegram, present only when true: a chat outside the user's known
    /// senders, where the user has almost never replied. iMessage: Messages files it outside
    /// its main list, for example an unknown sender, an automated or business sender, or junk.
    /// Telegram: a direct chat with a bot, Telegram's service notifications, an account
    /// Telegram flags as a scam or fake, or someone outside the user's Telegram contacts
    /// whom the user has never written to in the cached history.
    #[serde(skip_serializing_if = "is_false")]
    pub filtered: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A message store skipped because it could not be read.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Unavailable {
    /// Message store: "imessage", "whatsapp", "signal" or "telegram". Other stores may be
    /// added.
    #[schemars(with = "String")]
    pub source: Source,
    pub error: String,
}

/// Result of list_conversations.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ConversationList {
    /// Newest `latest_message_at` first, merged across sources; conversations without
    /// messages come last.
    pub conversations: Vec<Conversation>,
    /// Pass as `cursor`, with the same filters, for the next page. Null when complete.
    pub next_cursor: Option<String>,
    /// True when there are no more pages.
    pub complete: bool,
    /// Number of AddressBook databases read for contact names; 0 means no names from Contacts.
    pub contacts_databases_read: usize,
    /// Present only when no source was requested and a store could not be read; the other
    /// stores' results are still returned.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable_sources: Vec<Unavailable>,
}

/// Result of search_messages and read_conversation.
#[derive(Debug, Serialize, JsonSchema)]
pub struct MessageList {
    /// Sorted by date, then message ID, within each source, in the requested order (oldest
    /// first by default). iMessage results come first, then WhatsApp, Signal and Telegram, so
    /// sources are not interleaved by date.
    pub messages: Vec<Message>,
    /// Pass as `cursor`, with the same filters, for the next page. Null when complete. A page
    /// can be empty and still have a cursor. Cursors don't snapshot the database, so new
    /// messages can shift pages.
    pub next_cursor: Option<String>,
    /// True when there are no more pages.
    pub complete: bool,
    /// Rows examined for this page (at most 10000 per call).
    pub scanned_rows: usize,
    /// iMessage and Telegram rows that could not be decoded, including rows that did not match.
    pub decode_failures: usize,
    /// Number of AddressBook databases read for contact names; 0 means no names from Contacts.
    pub contacts_databases_read: usize,
    /// Present only when no source or conversation was requested and a store could not be
    /// read; the other stores' results are still returned.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable_sources: Vec<Unavailable>,
}

/// Result of get_message_context.
#[derive(Debug, Serialize, JsonSchema)]
pub struct MessageContext {
    /// The requested message with its neighbors, oldest first.
    pub messages: Vec<Message>,
    /// Number of AddressBook databases read for contact names; 0 means no names from Contacts.
    pub contacts_databases_read: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SendStatus {
    /// Handed to Messages. Delivery is not confirmed.
    Submitted,
}

#[derive(Debug, Serialize, JsonSchema)]
pub enum Service {
    #[serde(rename = "iMessage")]
    Imessage,
}

/// Result of send_message.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SendReceipt {
    /// "submitted": handed to the app. Delivery is not confirmed.
    #[schemars(with = "String")]
    pub status: SendStatus,
    /// "iMessage". Other services may be added.
    #[schemars(with = "String")]
    pub service: Service,
    /// The recipient address as sent: a phone number in E.164, or the email address.
    pub recipient: String,
}

/// Where a contact's name came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NameSource {
    /// macOS Contacts, as the user saved it.
    Contacts,
    /// WhatsApp's address book: the name saved for this number on the user's phone, as
    /// WhatsApp synced it. Not the person's self-chosen WhatsApp profile name.
    Whatsapp,
    /// Telegram's contacts: the name the user saved for this person in Telegram.
    Telegram,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
pub struct ContactName {
    pub name: String,
    /// "contacts": macOS Contacts, as the user saved it. "whatsapp": WhatsApp's address
    /// book, the name saved for this number on the user's phone as WhatsApp synced it, not
    /// the person's own profile name. "telegram": the user's Telegram contacts, the name
    /// the user saved in Telegram. Other address books may be added.
    #[schemars(with = "String")]
    pub source: NameSource,
}

/// One person, merged from every entry in macOS Contacts, WhatsApp's address book and the
/// user's Telegram contacts that shares a phone number, email address, WhatsApp JID or
/// Telegram ID with it.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Contact {
    /// Every name saved for this person, macOS Contacts first, then WhatsApp, then Telegram,
    /// each with its source. Two entries that share an address (a family email, say) merge,
    /// so names can differ.
    pub names: Vec<ContactName>,
    /// Phone numbers in E.164 when the country is known, otherwise as saved.
    pub phone_numbers: Vec<String>,
    pub email_addresses: Vec<String>,
    /// WhatsApp JIDs, such as `<digits>@s.whatsapp.net` or an opaque `<id>@lid`. Omitted when
    /// the person isn't in WhatsApp's address book.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub whatsapp_jids: Vec<String>,
    /// Telegram user IDs, as strings, the `telegram_id` that messages from this person carry.
    /// Many Telegram users hide their phone number, so this is often the only way to match
    /// them. Omitted when the person isn't in the user's Telegram contacts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub telegram_ids: Vec<String>,
    /// Telegram public usernames, without the `@`. Omitted when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub telegram_usernames: Vec<String>,
    /// True when the person is saved in macOS Contacts. send_message accepts only phone
    /// numbers and email addresses saved there.
    pub in_contacts: bool,
}

/// Result of search_contacts.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ContactList {
    /// Sorted by first name, case-insensitively.
    pub contacts: Vec<Contact>,
    /// False when more people matched than `limit`; narrow the query to see the rest.
    pub complete: bool,
    /// Number of AddressBook databases read; 0 means nothing from macOS Contacts.
    pub contacts_databases_read: usize,
    /// Present only when an enabled app's address book (WhatsApp's or Telegram's) could
    /// not be read; the results from the other address books are still returned.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable_sources: Vec<Unavailable>,
}
