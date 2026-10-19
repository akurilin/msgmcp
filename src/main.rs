mod audit;
mod caller;
mod client;
mod common;
mod contacts;
mod gateway;
mod imessage;
mod socket;
mod types;
mod whatsapp;

use anyhow::{Context, Result, bail};
use client::Endpoint;
use common::{
    Filter, MAX_CONTEXT, SCAN_LIMIT, e164, is_contact, keep_conversation, limit,
    load_contact_records, load_contacts, parse_time,
};
use rmcp::{
    ServerHandler,
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    model::{Extensions, Implementation, JsonObject, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command, sync::Arc};
use types::{
    ALL_SOURCES, ContactList, ConversationList, MessageContext, MessageList, SendReceipt, Source,
    Unavailable,
};

/// Parse a source-qualified ID such as `imessage:2771` or `whatsapp:45`.
fn parse_id(id: &str) -> Result<(Source, i64)> {
    id.split_once(':')
        .and_then(|(p, n)| Some((Source::parse(p)?, n.parse().ok()?)))
        .with_context(|| format!("Invalid ID {id:?}; IDs look like imessage:123 or whatsapp:45"))
}

/// Parse a pagination cursor `<source>:<backend cursor>`; an empty backend part means "start".
fn parse_cursor(cursor: &str) -> Result<(Source, Option<String>)> {
    let (prefix, rest) = cursor.split_once(':').context("Invalid cursor")?;
    let source = Source::parse(prefix).context("Invalid cursor")?;
    Ok((source, Some(rest.to_owned()).filter(|r| !r.is_empty())))
}

#[derive(Clone)]
struct Paths {
    messages: PathBuf,
    contacts: PathBuf,
    whatsapp: PathBuf,
}

#[derive(Clone)]
struct Server {
    paths: Arc<Paths>,
    tool_router: Arc<ToolRouter<Server>>,
    /// Socket connections: the process on the other end. Gateway requests carry their
    /// caller in the HTTP request instead.
    caller: Option<Arc<caller::Caller>>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct Conversations {
    /// Case-insensitive substring of a chat name, identifier, participant address, JID, or contact name.
    /// Phone numbers match in any format and in part: "(555) 123-4567", "5551234567" and
    /// "555-1234" all find +15551234567.
    query: Option<String>,
    /// Restrict to one source; omit to list both, merged.
    source: Option<Source>,
    /// RFC 3339 time, inclusive: only conversations whose newest message is at or after it.
    active_after: Option<String>,
    /// Only conversations with unread messages (unread_count above 0). Skips archived and
    /// filtered chats, as the apps' unread views do, unless include_archived or
    /// include_filtered is true.
    unread_only: Option<bool>,
    /// With unread_only: also return archived chats.
    include_archived: Option<bool>,
    /// With unread_only: also return filtered iMessage chats (unknown senders, automated
    /// senders and junk).
    include_filtered: Option<bool>,
    /// Page size, 1 to 200; default 50.
    limit: Option<usize>,
    /// Opaque continuation cursor from the previous response.
    cursor: Option<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum MessageOrder {
    /// Chronological; the default.
    #[default]
    OldestFirst,
    /// Reverse chronological: with `limit: N`, the first page is the latest N messages.
    NewestFirst,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct Messages {
    /// Source-qualified conversation ID, e.g. "imessage:2771" or "whatsapp:45".
    conversation_id: Option<String>,
    /// Restrict to one source when conversation_id is omitted; omit to search both.
    source: Option<Source>,
    /// Case-insensitive literal text. Required for search_messages.
    text: Option<String>,
    /// Exact address, WhatsApp JID, contact-name substring, or "me". A phone number matches
    /// in any format; one without a country code is taken as North American (+1).
    sender: Option<String>,
    /// RFC 3339 time, inclusive.
    after: Option<String>,
    /// RFC 3339 time, exclusive.
    before: Option<String>,
    /// Message order within each source; default oldest_first.
    order: Option<MessageOrder>,
    /// Page size, 1 to 200; default 50.
    limit: Option<usize>,
    /// Opaque continuation cursor from the previous response.
    cursor: Option<String>,
}
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ContextRequest {
    /// Source-qualified message ID, e.g. "whatsapp:123".
    message_id: String,
    /// Source-qualified conversation ID containing the message.
    conversation_id: String,
    /// Number of earlier messages to include, 0 to 100; default 5.
    before: Option<usize>,
    /// Number of later messages to include, 0 to 100; default 5.
    after: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ContactSearch {
    /// Required. Case-insensitive substring of a name, phone number, email address or
    /// WhatsApp JID. Phone numbers match in any format and in part, as in list_conversations.
    query: String,
    /// Most people to return, 1 to 200; default 50.
    limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SendRequest {
    /// Phone number or email address saved in Contacts; not a contact name. Any phone format
    /// works; a number without a country code is taken as North American (+1).
    recipient: String,
    /// Message text; must not be empty.
    text: String,
}

/// Decide which backends to query. Explicit requests surface backend errors; the default
/// "all sources" mode reports an unreadable store in `unavailable_sources` and continues.
fn plan(
    source: Option<Source>,
    conversation: Option<&str>,
) -> Result<(Vec<Source>, Option<i64>, bool)> {
    match (conversation.map(parse_id).transpose()?, source) {
        (Some((s, _)), Some(requested)) if s != requested => {
            bail!(
                "source {requested} does not match conversation_id {}",
                conversation.unwrap_or_default()
            )
        }
        (Some((s, id)), _) => Ok((vec![s], Some(id), true)),
        (None, Some(s)) => Ok((vec![s], None, true)),
        (None, None) => Ok((ALL_SOURCES.to_vec(), None, false)),
    }
}

/// Where pagination starts: the cursor's source position and its backend-local cursor.
fn start(order: &[Source], cursor: Option<&str>) -> Result<(usize, Option<String>)> {
    let Some(cursor) = cursor else {
        return Ok((0, None));
    };
    let (source, local) = parse_cursor(cursor)?;
    let index = order
        .iter()
        .position(|s| *s == source)
        .context("Cursor does not match the requested source; repeat the same filters")?;
    Ok((index, local))
}

/// list_conversations order: newest activity first, then conversations without messages,
/// then source and ID. Times are microseconds so every valid date fits in an i64.
type ListKey = (std::cmp::Reverse<Option<i64>>, Source, i64);

fn list_key(latest: Option<chrono::DateTime<chrono::Utc>>, source: Source, id: i64) -> ListKey {
    (
        std::cmp::Reverse(latest.map(|t| t.timestamp_micros())),
        source,
        id,
    )
}

/// A list_conversations cursor is the key of the last returned item: `<micros|->:<source>:<id>`.
fn format_list_cursor((std::cmp::Reverse(time), source, id): &ListKey) -> String {
    let time = time.map_or_else(|| "-".to_owned(), |t| t.to_string());
    format!("{time}:{source}:{id}")
}

fn parse_list_cursor(cursor: &str) -> Result<ListKey> {
    let mut parts = cursor.splitn(3, ':');
    let (Some(time), Some(source), Some(id)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("Invalid cursor")
    };
    let time = match time {
        "-" => None,
        t => Some(t.parse().context("Invalid cursor")?),
    };
    let source = Source::parse(source).context("Invalid cursor")?;
    Ok((
        std::cmp::Reverse(time),
        source,
        id.parse().context("Invalid cursor")?,
    ))
}

impl Paths {
    /// Cheap readability probe used before skipping a source in "all sources" mode.
    fn check(&self, source: Source) -> Result<()> {
        let path = match source {
            Source::Imessage => self.messages.clone(),
            Source::Whatsapp => self.whatsapp.join("ChatStorage.sqlite"),
        };
        if !path.exists() {
            bail!("{} not found", path.display())
        }
        common::open(&path)?.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
        Ok(())
    }
}

fn none_readable(missing: &[Unavailable]) -> anyhow::Error {
    anyhow::anyhow!(
        "No message store is readable: {}",
        serde_json::to_string(missing).unwrap_or_default()
    )
}

/// Tool arguments as the audit log records them.
fn audit_arguments<A: Serialize>(arguments: &A) -> Value {
    serde_json::to_value(arguments).unwrap_or_default()
}

/// Describe a tool result as it is serialized: always-present nullable fields are required
/// and fields omitted when empty are optional. (rmcp's default describes deserialization.)
fn output_schema<T: JsonSchema>() -> Arc<JsonObject> {
    let schema = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>();
    let serde_json::Value::Object(mut object) = serde_json::to_value(schema).unwrap_or_default()
    else {
        unreachable!("a struct schema is an object")
    };
    object.remove("title");
    Arc::new(object)
}

impl Server {
    /// `send` false leaves send_message out of tools/list and refuses calls to it.
    fn new(paths: Paths, send: bool) -> Self {
        let mut tool_router = Self::tool_router();
        if !send {
            tool_router.remove_route("send_message");
        }
        Self {
            paths: Arc::new(paths),
            tool_router: Arc::new(tool_router),
            caller: None,
        }
    }

    fn with_caller(&self, caller: caller::Caller) -> Self {
        Self {
            caller: Some(Arc::new(caller)),
            ..self.clone()
        }
    }

    fn caller(&self, extensions: &Extensions) -> Option<caller::Caller> {
        extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<caller::Caller>())
            .cloned()
            .or_else(|| self.caller.as_deref().cloned())
    }

    /// Run one tool call off the async runtime and record it in the audit log.
    async fn run<T, F>(
        &self,
        tool: &str,
        arguments: Value,
        extensions: &Extensions,
        job: F,
    ) -> Result<Json<T>, String>
    where
        F: FnOnce(&Paths) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let paths = self.paths.clone();
        let started = std::time::Instant::now();
        let result = match tokio::task::spawn_blocking(move || job(&paths)).await {
            Ok(Ok(value)) => Ok(Json(value)),
            Ok(Err(err)) => Err(format!("{err:#}")),
            Err(_) => Err("Server task failed".to_owned()),
        };
        audit::record(
            "tool_call",
            json!({
                "tool": tool,
                "caller": self.caller(extensions),
                "arguments": arguments,
                "outcome": if result.is_ok() { "ok" } else { "error" },
                "error": result.as_ref().err(),
                "duration_ms": started.elapsed().as_millis() as u64,
            }),
        );
        result
    }
}
#[tool_router]
impl Server {
    #[tool(
        output_schema = output_schema::<SendReceipt>(),
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        ),
        description = "Send an iMessage through the macOS Messages app (WhatsApp sending is not supported). Supply the recipient's phone number or email address, as in list_conversations participants; the recipient must be saved in the user's Contacts; contact names and group chats are not supported. No SMS fallback. Use only when the user asks to send. Returns submitted, not delivery confirmation. Never retry automatically."
    )]
    async fn send_message(
        &self,
        Parameters(p): Parameters<SendRequest>,
        extensions: Extensions,
    ) -> Result<Json<SendReceipt>, String> {
        self.run("send_message", audit_arguments(&p), &extensions, move |paths| {
            // Only people the user saved in Contacts: an unknown number may be a stranger
            // steering an agent through the messages it reads.
            let (contacts, readable) = load_contacts(&paths.contacts);
            if readable == 0 {
                bail!("Contacts could not be read, so the recipient can't be checked. Nothing was sent.")
            }
            let given = p.recipient.trim();
            let recipient = match e164(given) {
                Some(number) => number,
                None if given.contains('@') => given.to_owned(),
                None => bail!(
                    "{given} is not a phone number with a country code or an email address; contact names are not supported. Nothing was sent."
                ),
            };
            if !is_contact(&recipient, &contacts) {
                bail!(
                    "{given} is not in Contacts; send_message only sends to phone numbers and email addresses saved in Contacts. Nothing was sent."
                )
            }
            imessage::send(&recipient, &p.text)
        })
        .await
    }
    #[tool(
        output_schema = output_schema::<ConversationList>(),
        annotations(read_only_hint = true),
        description = "List iMessage/SMS and WhatsApp conversations, newest activity first across both sources. Filter by recent activity (active_after), unread messages (unread_only, which skips archived and filtered chats by default), chat name, participant address or JID, or contact name. Each item has an unread_count and a source-qualified conversation_id (imessage:N or whatsapp:N). Follow next_cursor until complete."
    )]
    async fn list_conversations(
        &self,
        Parameters(p): Parameters<Conversations>,
        extensions: Extensions,
    ) -> Result<Json<ConversationList>, String> {
        self.run(
            "list_conversations",
            audit_arguments(&p),
            &extensions,
            move |paths| {
                let cap = limit(p.limit)?;
                let (order, _, explicit) = plan(p.source, None)?;
                let after = p.cursor.as_deref().map(parse_list_cursor).transpose()?;
                let active_after = parse_time(p.active_after.as_deref())?;
                let query = p.query.as_deref().unwrap_or("").to_lowercase();
                let unread_only = p.unread_only.unwrap_or(false);
                let include_archived = p.include_archived.unwrap_or(false);
                let include_filtered = p.include_filtered.unwrap_or(false);
                if !unread_only && (p.include_archived.is_some() || p.include_filtered.is_some()) {
                    bail!("include_archived and include_filtered require unread_only")
                }
                let (contacts, readable) = load_contacts(&paths.contacts);
                let (mut items, mut missing) = (Vec::new(), Vec::new());
                for source in order.iter().copied() {
                    if !explicit && let Err(error) = paths.check(source) {
                        missing.push(Unavailable {
                            source,
                            error: format!("{error:#}"),
                        });
                        continue;
                    }
                    let listed = match source {
                        Source::Imessage => {
                            imessage::conversations(&paths.messages, &contacts, active_after)?
                        }
                        Source::Whatsapp => {
                            whatsapp::conversations(&paths.whatsapp, &contacts, active_after)?
                        }
                    };
                    for mut item in listed {
                        let key = list_key(item.latest, source, item.id);
                        if after.is_none_or(|a| key > a)
                            && (!unread_only || {
                                let c = &item.conversation;
                                c.unread_count > 0
                                    && (include_archived || !c.archived)
                                    && (include_filtered || !c.filtered)
                            })
                            && keep_conversation(&mut item.conversation, &query)
                        {
                            items.push((key, item.conversation));
                        }
                    }
                }
                if missing.len() == order.len() {
                    return Err(none_readable(&missing));
                }
                items.sort_by_key(|a| a.0);
                let more = items.len() > cap;
                items.truncate(cap);
                let next = more
                    .then(|| items.last().map(|(key, _)| format_list_cursor(key)))
                    .flatten();
                Ok(ConversationList {
                    conversations: items.into_iter().map(|(_, c)| c).collect(),
                    complete: next.is_none(),
                    next_cursor: next,
                    contacts_databases_read: readable,
                    unavailable_sources: missing,
                })
            },
        )
        .await
    }
    #[tool(
        output_schema = output_schema::<MessageList>(),
        annotations(read_only_hint = true),
        description = "Search decoded iMessage/SMS and WhatsApp text using a case-insensitive literal substring. Optional source, conversation, date and sender filters. Without a source or conversation, iMessage results come first, then WhatsApp. Scans at most 10000 rows per call; follow next_cursor until complete."
    )]
    async fn search_messages(
        &self,
        Parameters(p): Parameters<Messages>,
        extensions: Extensions,
    ) -> Result<Json<MessageList>, String> {
        self.messages(p, true, &extensions).await
    }
    #[tool(
        output_schema = output_schema::<MessageList>(),
        annotations(read_only_hint = true),
        description = "Read one conversation. conversation_id (e.g. imessage:2771 or whatsapp:45) is required. Messages are oldest first; pass order newest_first with limit N to get the latest N. Use next_cursor for more messages; date filters use RFC 3339."
    )]
    async fn read_conversation(
        &self,
        Parameters(p): Parameters<Messages>,
        extensions: Extensions,
    ) -> Result<Json<MessageList>, String> {
        self.messages(p, false, &extensions).await
    }
    #[tool(
        output_schema = output_schema::<ContactList>(),
        annotations(read_only_hint = true),
        description = "Look up people in the user's macOS Contacts and WhatsApp's address book by name, phone number, email address or WhatsApp JID; a query is required. Entries that share a phone number, email address or JID are merged into one person, and each name carries its source (contacts or whatsapp). Use it to find someone's number or email for send_message (in_contacts must be true), or the addresses to look for with list_conversations."
    )]
    async fn search_contacts(
        &self,
        Parameters(p): Parameters<ContactSearch>,
        extensions: Extensions,
    ) -> Result<Json<ContactList>, String> {
        self.run(
            "search_contacts",
            audit_arguments(&p),
            &extensions,
            move |paths| {
                let cap = limit(p.limit)?;
                if p.query.trim().is_empty() {
                    bail!("query is required and must not be empty")
                }
                let (records, readable) = load_contact_records(&paths.contacts);
                let (whatsapp, missing) = match whatsapp::address_book(&paths.whatsapp) {
                    Ok(entries) => (entries, Vec::new()),
                    Err(error) => (
                        Vec::new(),
                        vec![Unavailable {
                            source: Source::Whatsapp,
                            error: format!("{error:#}"),
                        }],
                    ),
                };
                if readable == 0 && !missing.is_empty() {
                    bail!(
                        "Neither macOS Contacts nor WhatsApp's address book could be read: {}",
                        missing[0].error
                    )
                }
                let (contacts, complete) = contacts::search(records, whatsapp, &p.query, cap);
                Ok(ContactList {
                    contacts,
                    complete,
                    contacts_databases_read: readable,
                    unavailable_sources: missing,
                })
            },
        )
        .await
    }
    #[tool(
        output_schema = output_schema::<MessageContext>(),
        annotations(read_only_hint = true),
        description = "Read a message and nearby messages in its conversation. Source-qualified IDs come from search_messages or read_conversation."
    )]
    async fn get_message_context(
        &self,
        Parameters(p): Parameters<ContextRequest>,
        extensions: Extensions,
    ) -> Result<Json<MessageContext>, String> {
        self.run(
            "get_message_context",
            audit_arguments(&p),
            &extensions,
            move |paths| {
                let before = p.before.unwrap_or(5);
                let after = p.after.unwrap_or(5);
                if before > MAX_CONTEXT || after > MAX_CONTEXT {
                    bail!("before and after must not exceed {MAX_CONTEXT}")
                }
                let (source, conversation) = parse_id(&p.conversation_id)?;
                let (message_source, message) = parse_id(&p.message_id)?;
                if source != message_source {
                    bail!("message_id and conversation_id must come from the same source")
                }
                let (contacts, readable) = load_contacts(&paths.contacts);
                let messages = match source {
                    Source::Imessage => imessage::context(
                        &paths.messages,
                        &contacts,
                        conversation,
                        message,
                        before,
                        after,
                    )?,
                    Source::Whatsapp => whatsapp::context(
                        &paths.whatsapp,
                        &contacts,
                        conversation,
                        message,
                        before,
                        after,
                    )?,
                };
                Ok(MessageContext {
                    messages,
                    contacts_databases_read: readable,
                })
            },
        )
        .await
    }
}
impl Server {
    async fn messages(
        &self,
        p: Messages,
        search: bool,
        extensions: &Extensions,
    ) -> Result<Json<MessageList>, String> {
        let tool = if search {
            "search_messages"
        } else {
            "read_conversation"
        };
        self.run(tool, audit_arguments(&p), extensions, move |paths| {
            let cap = limit(p.limit)?;
            if search && p.text.as_deref().is_none_or(|t| t.trim().is_empty()) {
                bail!("text is required and must not be empty")
            }
            if !search && p.conversation_id.is_none() {
                bail!("conversation_id is required")
            }
            let (order, conversation, explicit) = plan(p.source, p.conversation_id.as_deref())?;
            let filter = Filter {
                conversation,
                after: parse_time(p.after.as_deref())?,
                before: parse_time(p.before.as_deref())?,
                text: p.text.map(|x| x.to_lowercase()),
                sender: p.sender.map(|x| x.to_lowercase()),
                newest_first: p.order.unwrap_or_default() == MessageOrder::NewestFirst,
            };
            if filter.after.zip(filter.before).is_some_and(|(a, b)| a >= b) {
                bail!("after must be earlier than before")
            }
            let (first, local) = start(&order, p.cursor.as_deref())?;
            let (contacts, readable) = load_contacts(&paths.contacts);
            let (mut items, mut scanned, mut failures) = (Vec::new(), 0, 0);
            let (mut next, mut missing) = (None, Vec::new());
            for (index, source) in order.iter().copied().enumerate().skip(first) {
                let cursor = if index == first { local.clone() } else { None };
                if items.len() == cap || scanned == SCAN_LIMIT {
                    next = Some(format!("{source}:{}", cursor.unwrap_or_default()));
                    break;
                }
                if !explicit && let Err(error) = paths.check(source) {
                    missing.push(Unavailable {
                        source,
                        error: format!("{error:#}"),
                    });
                    continue;
                }
                let (left, budget) = (cap - items.len(), SCAN_LIMIT - scanned);
                let page = match source {
                    Source::Imessage => imessage::messages(
                        &paths.messages,
                        &contacts,
                        &filter,
                        cursor.as_deref(),
                        left,
                        budget,
                    )?,
                    Source::Whatsapp => whatsapp::messages(
                        &paths.whatsapp,
                        &contacts,
                        &filter,
                        cursor.as_deref(),
                        left,
                        budget,
                    )?,
                };
                items.extend(page.items);
                scanned += page.scanned;
                failures += page.decode_failures;
                if let Some(local) = page.next {
                    next = Some(format!("{source}:{local}"));
                    break;
                }
            }
            if missing.len() == order.len() {
                return Err(none_readable(&missing));
            }
            Ok(MessageList {
                messages: items,
                complete: next.is_none(),
                next_cursor: next,
                scanned_rows: scanned,
                decode_failures: failures,
                contacts_databases_read: readable,
                unavailable_sources: missing,
            })
        })
        .await
    }
}
const INSTRUCTIONS: &str = "Search and read the user’s iMessage, SMS and WhatsApp history on this Mac. To find messages by person, first call list_conversations with their name; for unread conversations, call list_conversations with unread_only; for recent activity, call list_conversations with active_after, then read_conversation with order newest_first and a small limit; a person can have both an imessage: and a whatsapp: conversation. IDs are source-qualified strings such as imessage:2771 or whatsapp:45.";
const SEND_INSTRUCTIONS: &str = " Send messages (iMessage only) only when the user asks. Sending returns submission, not delivery confirmation; never retry automatically.";
const COMMON_INSTRUCTIONS: &str = " Follow continuation cursors with the same filters. Times use RFC 3339. Phone numbers are E.164 in results and match in any format in arguments. Contact names are optional. To look up a person's numbers and addresses, call search_contacts.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let send = if self.tool_router.has_route("send_message") {
            SEND_INSTRUCTIONS
        } else {
            ""
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("msgmcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!("{INSTRUCTIONS}{send}{COMMON_INSTRUCTIONS}"))
    }
}
const AUTOMATION_CHECK_SCRIPT: &str = r#"with timeout of 10 seconds
 tell application "Messages" to get count of accounts
end timeout"#;

const USAGE: &str = "\
Services (the LaunchAgent runs one; both serve the same tools):
msgmcp socket [--socket PATH] [--audit-log PATH] [STORES]
    MCP on a Unix socket, for clients running as this user on this Mac
msgmcp gateway [--listen ADDRESS] [--allow-host HOST]... [--allow-send] [--token-file PATH] [--audit-log PATH] [STORES]
    MCP over Streamable HTTP at /mcp, with a bearer token; default address 127.0.0.1:8765.
    send_message is offered only with --allow-send. --allow-host accepts another Host
    name, such as a tailnet name. The token file is created on first start.
STORES: [--database PATH] [--contacts DIRECTORY] [--whatsapp DIRECTORY]
Both services append every tool call and rejected request to the audit log, by default
~/Library/Logs/msgmcp/audit.log. send_message only sends to addresses saved in Contacts.

Clients (they open no database; ENDPOINT is --socket PATH or --url URL [--token-file PATH]):
msgmcp [ENDPOINT]                     MCP stdio adapter (what MCP clients run)
msgmcp schema [ENDPOINT]              print the server's tools, input/output schemas and instructions as JSON
msgmcp call TOOL [JSON] [ENDPOINT]    call one tool; prints its JSON result, or the error on stderr with exit code 1
Without an ENDPOINT, clients use the socket if it exists, else the gateway at
http://127.0.0.1:8765/mcp. The gateway token comes from MSGMCP_TOKEN, else the token file.

msgmcp --check-automation             check Automation access to Messages (read-only)";

enum Mode {
    Adapter,
    Schema,
    Call(String, Option<String>),
    Socket,
    Gateway,
}

impl Mode {
    fn is_client(&self) -> bool {
        matches!(self, Mode::Adapter | Mode::Schema | Mode::Call(..))
    }
}

fn check_automation() -> Result<()> {
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", AUTOMATION_CHECK_SCRIPT])
        .output()?;
    if !output.status.success() {
        bail!(
            "Messages Automation access failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    println!(
        "Messages Automation access succeeded; account count: {}",
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let state = home.join("Library/Application Support/MSGMCP");
    let mut paths = Paths {
        messages: home.join("Library/Messages/chat.db"),
        contacts: home.join("Library/Application Support/AddressBook"),
        whatsapp: home.join("Library/Group Containers/group.net.whatsapp.WhatsApp.shared"),
    };
    let default_socket = state.join("run/service.sock");
    let mut audit_log = home.join("Library/Logs/msgmcp/audit.log");
    let (mut socket, mut url, mut token_file) = (None, None, state.join("gateway.token"));
    let mut listen = gateway::DEFAULT_LISTEN.to_owned();
    let (mut allowed_hosts, mut allow_send) = (Vec::new(), false);
    // Options that only one kind of command accepts, checked once the mode is known.
    let (mut store_option, mut gateway_option) = (None, None);
    let mut mode = Mode::Adapter;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "socket" if matches!(mode, Mode::Adapter) => mode = Mode::Socket,
            "gateway" if matches!(mode, Mode::Adapter) => mode = Mode::Gateway,
            "schema" if matches!(mode, Mode::Adapter) => mode = Mode::Schema,
            "call" if matches!(mode, Mode::Adapter) => {
                let tool = args
                    .next_if(|a| !a.starts_with("--"))
                    .context("call needs a tool name")?;
                mode = Mode::Call(tool, args.next_if(|a| !a.starts_with("--")));
            }
            "--check-automation" => return check_automation(),
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--url" => url = Some(value()?),
            "--token-file" => token_file = PathBuf::from(value()?),
            "--database" => paths.messages = PathBuf::from(value()?),
            "--contacts" => paths.contacts = PathBuf::from(value()?),
            "--whatsapp" => paths.whatsapp = PathBuf::from(value()?),
            "--audit-log" => audit_log = PathBuf::from(value()?),
            "--listen" => listen = value()?,
            "--allow-host" => allowed_hosts.push(value()?),
            "--allow-send" => allow_send = true,
            "--help" | "-h" | "help" => {
                eprintln!("{USAGE}");
                return Ok(());
            }
            _ => bail!("Unexpected argument: {arg}\n{USAGE}"),
        }
        match arg.as_str() {
            "--database" | "--contacts" | "--whatsapp" | "--audit-log" => store_option = Some(arg),
            "--listen" | "--allow-host" | "--allow-send" => gateway_option = Some(arg),
            _ => {}
        }
    }
    if let Some(option) = store_option.filter(|_| mode.is_client()) {
        bail!("{option} requires socket or gateway");
    }
    if let Some(option) = gateway_option.filter(|_| !matches!(mode, Mode::Gateway)) {
        bail!("{option} requires gateway");
    }
    if url.is_some() && !mode.is_client() {
        bail!("--url is for client commands; the gateway listens with --listen");
    }
    if socket.is_some() && matches!(mode, Mode::Gateway) {
        bail!("--socket is for the socket service and clients");
    }
    match mode {
        Mode::Socket => {
            audit::init(audit_log, "socket")?;
            let socket = socket.unwrap_or(default_socket);
            socket::serve(Server::new(paths, true), socket).await
        }
        Mode::Gateway => {
            audit::init(audit_log, "gateway")?;
            let options = gateway::Options {
                listen,
                token_file,
                allowed_hosts,
            };
            gateway::serve(Server::new(paths, allow_send), options).await
        }
        client => {
            if socket.is_some() && url.is_some() {
                bail!("Pass --socket or --url, not both");
            }
            let endpoint = match (socket, url) {
                (Some(path), _) => Endpoint::Socket(path),
                (None, None) if default_socket.exists() => Endpoint::Socket(default_socket),
                (None, url) => {
                    let token = match std::env::var("MSGMCP_TOKEN") {
                        Ok(token) if !token.is_empty() => token,
                        _ => gateway::read_token(&token_file)?,
                    };
                    let url = url.unwrap_or_else(|| {
                        format!("http://{}{}", gateway::DEFAULT_LISTEN, gateway::MCP_PATH)
                    });
                    Endpoint::Gateway { url, token }
                }
            };
            match client {
                Mode::Schema => client::schema(&endpoint).await,
                Mode::Call(tool, arguments) => client::call(&endpoint, tool, arguments).await,
                _ => client::adapter(&endpoint).await,
            }
        }
    }
}
