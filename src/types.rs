//! Typed tool inputs' shared enums and every tool result. Each result type's JSON schema is
//! published as the tool's `outputSchema`, so the doc comments here are the reference for
//! result fields; clients see them through `tools/list` or `msgmcp schema`.
use crate::{imessage, signal, whatsapp};
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
}
pub const ALL_SOURCES: [Source; 3] = [Source::Imessage, Source::Whatsapp, Source::Signal];

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Imessage => imessage::PREFIX,
            Source::Whatsapp => whatsapp::PREFIX,
            Source::Signal => signal::PREFIX,
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
    /// phone number is known. Null for the user ("me"), for unknown senders, and for Signal
    /// accounts whose number is hidden (see `signal_id`).
    pub address: Option<String>,
    /// WhatsApp only: normalized JID, such as `<digits>@s.whatsapp.net` or an opaque `<id>@lid`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jid: Option<String>,
    /// Signal only: the account's Signal ID (ACI), a lowercase UUID. Signal knows many people
    /// only by this ID, since phone numbers can be hidden.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal_id: Option<String>,
    /// "me" for the user. Otherwise the AddressBook name; for WhatsApp, then the WhatsApp
    /// contact name, then the person's WhatsApp profile name; for Signal, then the nickname
    /// the user gave them in Signal, the name from the user's phone contacts, the person's
    /// Signal profile name, then their username. Null when unknown or ambiguous.
    pub name: Option<String>,
    /// True when several AddressBook contacts share this address, so no name is given.
    /// Contacts match in any format; a number saved without a country code is taken as
    /// North American (+1).
    pub ambiguous: bool,
}

impl Person {
    pub fn me() -> Person {
        Person {
            address: None,
            jid: None,
            signal_id: None,
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
    /// iMessage only: the stored attributed text could not be decoded.
    DecodeFailed,
}

/// WhatsApp and Signal message type.
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

/// WhatsApp and Signal chat type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChatType {
    Direct,
    Group,
    Broadcast,
    Community,
}

/// One person's current reaction to a WhatsApp or Signal message.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Reaction {
    pub emoji: String,
    pub is_from_me: bool,
    pub sender: Person,
}

/// A message. iMessage reactions, events (such as group changes), edit history and recall
/// state are not returned; WhatsApp group events and security notices are excluded; Signal
/// events (group changes, safety-number changes, calls, timer changes) and stories are
/// excluded, and an edited Signal message has its latest text.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Message {
    pub source: Source,
    /// Source-qualified message ID, e.g. "imessage:85544", "whatsapp:9120" or "signal:310".
    /// Pass it back exactly as returned.
    pub message_id: String,
    /// The store's own message ID (iMessage GUID, WhatsApp stanza ID or Signal message UUID);
    /// other messages' `reply_to_guid` refers to it.
    pub guid: Option<String>,
    /// Source-qualified conversation ID, e.g. "imessage:2771", "whatsapp:45" or
    /// "signal:<uuid>".
    pub conversation_id: String,
    /// RFC 3339 time in UTC. Signal uses the sender's time.
    pub sent_at: Option<String>,
    pub is_from_me: bool,
    /// iMessage and Signal, present only on incoming messages: whether the user has read it
    /// (iMessage: on any of their Apple devices; Signal: as Signal Desktop last synced).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_read: Option<bool>,
    pub sender: Person,
    /// WhatsApp and Signal only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    /// Message text. iMessage attachments are not read, so an attachment-only message is the
    /// object replacement character U+FFFC. WhatsApp media text is the caption or file name;
    /// Signal media text is the message body, or a document's file name; deleted messages
    /// have none.
    pub text: Option<String>,
    pub text_status: TextStatus,
    /// `guid` of the message this one replies to. iMessage uses the thread originator, which
    /// can be the thread root; WhatsApp and Signal use the quoted message.
    pub reply_to_guid: Option<String>,
    /// WhatsApp and Signal only, present only when the message has reactions: each person's
    /// current reaction, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
}

/// A conversation. WhatsApp status updates and removed chats are excluded, as are Signal
/// conversations the user removed or never used (Signal keeps one for every account it has
/// heard of).
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Conversation {
    pub source: Source,
    /// Source-qualified conversation ID, e.g. "imessage:2771", "whatsapp:45" or
    /// "signal:<uuid>". Pass it back exactly as returned.
    pub conversation_id: String,
    /// iMessage chat identifier (phone, email or group ID), WhatsApp chat JID, or for Signal
    /// the phone number (else Signal ID) of a direct chat or the group ID.
    pub identifier: Option<String>,
    /// Chat display name; empty or null for most direct chats.
    pub name: Option<String>,
    /// WhatsApp and Signal only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_type: Option<ChatType>,
    /// Other participants, at most 50. WhatsApp and Signal groups list current members only.
    pub participants: Vec<Person>,
    /// Number of participants before the 50-entry cap.
    pub participant_count: usize,
    /// RFC 3339 time in UTC of the newest message, as read_conversation returns them (iMessage
    /// reactions and events, and WhatsApp and Signal system events, don't count). Null when
    /// there are no messages.
    pub latest_message_at: Option<String>,
    /// Unread incoming messages; 0 when everything is read. iMessage counts messages not yet
    /// read on any of the user's Apple devices. WhatsApp and Signal use the desktop app's
    /// count, which can lag behind the phone; a chat the user marked unread counts as 1.
    pub unread_count: u64,
    /// WhatsApp and Signal only, present only when true: the user marked the chat unread by
    /// hand.
    #[serde(skip_serializing_if = "is_false")]
    pub marked_unread: bool,
    /// Present only when true: the user archived the chat.
    #[serde(skip_serializing_if = "is_false")]
    pub archived: bool,
    /// iMessage only, present only when true: Messages files the chat outside its main list
    /// of known senders, for example an unknown sender, an automated or business sender, or
    /// junk. The user has almost never replied in such chats.
    #[serde(skip_serializing_if = "is_false")]
    pub filtered: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A message store skipped because it could not be read.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Unavailable {
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
    /// first by default). iMessage results come first, then WhatsApp, then Signal, so sources
    /// are not interleaved by date.
    pub messages: Vec<Message>,
    /// Pass as `cursor`, with the same filters, for the next page. Null when complete. A page
    /// can be empty and still have a cursor. Cursors don't snapshot the database, so new
    /// messages can shift pages.
    pub next_cursor: Option<String>,
    /// True when there are no more pages.
    pub complete: bool,
    /// Rows examined for this page (at most 10000 per call).
    pub scanned_rows: usize,
    /// iMessage rows whose text could not be decoded, including rows that did not match.
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
    pub status: SendStatus,
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
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
pub struct ContactName {
    pub name: String,
    pub source: NameSource,
}

/// One person, merged from every entry in macOS Contacts and WhatsApp's address book that
/// shares a phone number, email address or WhatsApp JID with it.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Contact {
    /// Every name saved for this person, macOS Contacts first, each with its source. Two
    /// entries that share an address (a family email, say) merge, so names can differ.
    pub names: Vec<ContactName>,
    /// Phone numbers in E.164 when the country is known, otherwise as saved.
    pub phone_numbers: Vec<String>,
    pub email_addresses: Vec<String>,
    /// WhatsApp JIDs, such as `<digits>@s.whatsapp.net` or an opaque `<id>@lid`. Omitted when
    /// the person isn't in WhatsApp's address book.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub whatsapp_jids: Vec<String>,
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
    /// Present only when WhatsApp's address book could not be read; the macOS Contacts
    /// results are still returned.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable_sources: Vec<Unavailable>,
}
