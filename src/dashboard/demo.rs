//! Made-up data for the dashboard at `/?demo=1`, for screenshots: the same page with no
//! names, addresses, paths or tailnet of the real Mac. The events come from a fixed seed at
//! fixed offsets from now, so every load shows the same week.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{Value, json};

const HOME: &str = "/Users/you";
const TAILNET: &str = "tail4f2e1c.ts.net";
const DAYS: i64 = 7;

/// splitmix64: enough randomness for plausible traffic, with no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

pub fn status(now: DateTime<Utc>) -> Value {
    let store = |source: &str, names: &[&str], path: &str| {
        json!({"source": source, "names": names, "path": format!("{HOME}/{path}"),
               "readable": true, "error": null})
    };
    json!({
        "service": "gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "pid": 4321,
        "started_at": time(now - Duration::hours(26)),
        "endpoint": "http://127.0.0.1:8765/mcp",
        "allowed_hosts": [format!("my-mac.{TAILNET}")],
        "send_enabled": true,
        "tools": ["get_message_context", "list_conversations", "read_conversation",
                  "search_contacts", "search_messages", "send_message"],
        "stores": [
            store("imessage", &["iMessage", "SMS"], "Library/Messages/chat.db"),
            store("whatsapp", &["WhatsApp"],
                  "Library/Group Containers/group.net.whatsapp.WhatsApp.shared"),
            store("signal", &["Signal"], "Library/Application Support/Signal"),
            store("telegram", &["Telegram"],
                  "Library/Group Containers/6N38VWS5BX.ru.keepcoder.Telegram"),
        ],
        "contacts": {"path": format!("{HOME}/Library/Application Support/AddressBook"),
                     "databases": 2},
        "audit_log": format!("{HOME}/Library/Logs/msgmcp/audit.log"),
    })
}

fn time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, false)
}

fn agent(name: &str, ip: &str) -> Value {
    json!({"ip": ip, "device": format!("{name}.{TAILNET}"), "tags": ["tag:agent"]})
}

/// Arguments for one call of `tool`, like an agent's.
fn arguments(rng: &mut Rng, tool: &str, now: DateTime<Utc>) -> Value {
    let conversation = *rng.pick(&[
        "imessage:2771",
        "imessage:3012",
        "whatsapp:45",
        "whatsapp:118",
        "signal:7c1e2a90-0000-4000-8000-000000000001",
        "telegram:4416",
    ]);
    let person = *rng.pick(&["sarah", "marco", "mom", "priya", "book club", "dana"]);
    match tool {
        "list_conversations" => match rng.below(3) {
            0 => json!({"query": person, "limit": 5}),
            1 => json!({"unread_only": true}),
            _ => json!({"active_after": time(now - Duration::days(1)), "limit": 10}),
        },
        "read_conversation" => {
            json!({"conversation_id": conversation, "order": "newest_first", "limit": 20})
        }
        "get_message_context" => json!({"conversation_id": conversation,
            "message_id": format!("{}:{}", conversation.split(':').next().unwrap_or(""),
                                  1000 + rng.below(9000)),
            "before": 3, "after": 3}),
        "search_messages" => json!({
            "text": *rng.pick(&["dinner", "flight", "address", "birthday", "invoice"]),
            "limit": 10}),
        "search_contacts" => json!({"query": person}),
        _ => json!({"recipient": "+15555550123",
                    "text": *rng.pick(&["Running 10 minutes late", "See you at 7!"])}),
    }
}

/// A week of traffic, newest first: agents and local apps in bursts, a few errors, and
/// some refused requests. The last day is the busiest and has every caller, so the default
/// 24-hour view shows them all.
pub fn events(now: DateTime<Utc>) -> Vec<Value> {
    let mut rng = Rng(0x6d73_676d_6370);
    let callers = [
        agent("claude-agent", "100.101.102.103"),
        agent("codex-agent", "100.88.12.40"),
        json!({"ip": "127.0.0.1"}),
        json!({"pid": 5120,
               "program": format!("{HOME}/Applications/msgmcp.app/Contents/MacOS/msgmcp"),
               "parent_program": "/Applications/Claude.app/Contents/MacOS/Claude"}),
    ];
    let tools = [
        ("list_conversations", 34),
        ("read_conversation", 30),
        ("get_message_context", 16),
        ("search_messages", 10),
        ("search_contacts", 8),
        ("send_message", 2),
    ];
    let mut events = Vec::new();
    for day in 0..DAYS {
        let bursts = if day == 0 { 9 } else { 3 + rng.below(4) };
        for burst in 0..bursts {
            let caller = if day == 0 && burst < callers.len() as u64 {
                callers[burst as usize].clone()
            } else {
                rng.pick(&callers).clone()
            };
            let transport = if caller.get("pid").is_some() {
                "socket"
            } else {
                "gateway"
            };
            // Seconds before now when the burst ends; the calls before it go back in time.
            let mut at = day * 86_400 + rng.below(86_400) as i64;
            for _ in 0..8 + rng.below(30) {
                at += 10 + rng.below(120) as i64;
                let mut roll = rng.below(100);
                let tool = tools
                    .iter()
                    .find(|(_, weight)| {
                        let hit = roll < *weight;
                        roll = roll.saturating_sub(*weight);
                        hit
                    })
                    .map_or("list_conversations", |(tool, _)| tool);
                let arguments = arguments(&mut rng, tool, now);
                let error = (rng.below(60) == 0).then_some(match tool {
                    "send_message" => "Recipient +15555550123 is not saved in Contacts",
                    "get_message_context" => "Unknown message ID",
                    _ => "Unknown conversation ID imessage:999",
                });
                events.push((at, json!({
                    "transport": transport, "event": "tool_call", "tool": tool,
                    "caller": caller, "arguments": arguments,
                    "outcome": if error.is_some() { "error" } else { "ok" },
                    "error": error,
                    "duration_ms": 3 + rng.below(if tool == "send_message" { 1500 } else { 250 }),
                })));
            }
            // An agent's session starts with msgmcp_test, which checks the gateway refuses it.
            if caller.get("device").is_some() {
                events.push((
                    at + 2,
                    json!({
                        "transport": "gateway", "event": "rejected", "reason": "missing token",
                        "status": 401, "probe": true, "caller": caller,
                    }),
                ));
            }
        }
        for _ in 0..1 + rng.below(3) {
            let (reason, caller) = if rng.below(2) == 0 {
                (
                    "wrong token",
                    json!({"ip": "100.64.0.9",
                    "device": format!("old-laptop.{TAILNET}"), "user": "you@example.com"}),
                )
            } else {
                ("missing token", json!({"ip": "100.70.3.21"}))
            };
            let at = day * 86_400 + rng.below(86_400) as i64;
            events.push((
                at,
                json!({"transport": "gateway", "event": "rejected",
                "reason": reason, "status": 401, "caller": caller}),
            ));
        }
    }
    events.retain(|(at, _)| *at > 60);
    events.sort_by_key(|(at, _)| *at);
    events
        .into_iter()
        .map(|(at, mut event)| {
            event["time"] = json!(time(now - Duration::seconds(at)));
            event
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_week_of_events_newest_first_and_the_same_every_time() {
        let now = Utc::now();
        let events = events(now);
        assert!(events.len() > 100);
        assert_eq!(events, super::events(now));
        let times: Vec<_> = events
            .iter()
            .map(|e| DateTime::parse_from_rfc3339(e["time"].as_str().unwrap()).unwrap())
            .collect();
        assert!(times.windows(2).all(|w| w[0] >= w[1]));
        assert!(
            times
                .iter()
                .all(|t| *t < now && *t > now - Duration::days(DAYS + 1))
        );
        for kind in ["ok", "error"] {
            assert!(events.iter().any(|e| e["outcome"] == kind));
        }
        assert!(
            events
                .iter()
                .any(|e| e["event"] == "rejected" && e["probe"] == true)
        );
        assert!(
            events
                .iter()
                .any(|e| e["event"] == "rejected" && e["probe"].is_null())
        );
        assert!(events.iter().any(|e| e["tool"] == "send_message"));
        assert!(events.iter().any(|e| e["transport"] == "socket"));
    }
}
