//! The message stores msgmcp can serve. A store is a module that implements `Backend` and
//! an entry in `STORES`; the tools, cursors, `--source` and the instructions come from here.
use crate::common::{Contacts, Filter, Listed, Page};
use crate::contacts::AddressBookEntry;
use crate::types::{Message, Source};
use crate::{imessage, signal, telegram, whatsapp};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::PathBuf;

/// One store, read-only. Conversation and message IDs are the store's own, without the
/// source prefix.
pub trait Backend: Send + Sync {
    /// A cheap readability probe, used before skipping the store in "all sources" mode.
    fn check(&self) -> Result<()>;
    /// Every conversation whose newest message is at or after `active_after`.
    fn conversations(
        &self,
        contacts: &Contacts,
        active_after: Option<DateTime<Utc>>,
    ) -> Result<Vec<Listed>>;
    /// One page of messages in (time, ID) order, examining at most `budget` rows.
    fn messages(
        &self,
        contacts: &Contacts,
        filter: &Filter,
        cursor: Option<&str>,
        cap: usize,
        budget: usize,
    ) -> Result<Page>;
    /// A message with `before` earlier and `after` later messages of its conversation.
    fn context(
        &self,
        contacts: &Contacts,
        conversation: &str,
        message: &str,
        before: usize,
        after: usize,
    ) -> Result<Vec<Message>>;
    /// People saved in the app or with a real conversation, for search_contacts.
    fn address_book(&self) -> Option<Result<Vec<AddressBookEntry>>> {
        None
    }
}

pub struct Store {
    pub source: Source,
    /// The option that overrides the default path, and what the path names.
    pub option: &'static str,
    pub value: &'static str,
    /// Relative to the home directory.
    pub default_path: &'static str,
    /// How the instructions name the store's messages, and an example conversation ID.
    pub names: &'static [&'static str],
    pub example_id: &'static str,
    /// Conversation IDs are integers; otherwise any text without spaces.
    pub numeric_ids: bool,
    pub open: fn(PathBuf) -> Box<dyn Backend>,
}

/// In `Source` order, which is the order results come in.
pub const STORES: [Store; 4] = [
    Store {
        source: Source::Imessage,
        option: "--database",
        value: "PATH",
        default_path: "Library/Messages/chat.db",
        names: &["iMessage", "SMS"],
        example_id: "imessage:2771",
        numeric_ids: true,
        open: imessage::backend,
    },
    Store {
        source: Source::Whatsapp,
        option: "--whatsapp",
        value: "DIRECTORY",
        default_path: "Library/Group Containers/group.net.whatsapp.WhatsApp.shared",
        names: &["WhatsApp"],
        example_id: "whatsapp:45",
        numeric_ids: true,
        open: whatsapp::backend,
    },
    Store {
        source: Source::Signal,
        option: "--signal",
        value: "DIRECTORY",
        default_path: "Library/Application Support/Signal",
        names: &["Signal"],
        example_id: "signal:<uuid>",
        numeric_ids: false,
        open: signal::backend,
    },
    Store {
        source: Source::Telegram,
        option: "--telegram",
        value: "DIRECTORY",
        default_path: "Library/Group Containers/6N38VWS5BX.ru.keepcoder.Telegram",
        names: &["Telegram"],
        example_id: "telegram:4416",
        numeric_ids: true,
        open: telegram::backend,
    },
];

pub fn store(source: Source) -> &'static Store {
    STORES
        .iter()
        .find(|s| s.source == source)
        .expect("every source has a store")
}

/// "a, b and c".
pub fn and_list<S: AsRef<str>>(items: &[S], last: &str) -> String {
    match items {
        [] => String::new(),
        [only] => only.as_ref().to_owned(),
        [rest @ .., end] => format!(
            "{} {last} {}",
            rest.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            end.as_ref()
        ),
    }
}
