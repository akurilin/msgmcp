//! The dashboard page, rendered on the server.
//!
//! The page has no scripts. Every string that reaches the HTML (from the status document,
//! the audit log or the URL query) goes through `esc`, the only place that writes the
//! characters that matter to HTML. Links are built from the parsed query by `Params::href`
//! and escaped as a whole when they are written into an attribute.

use chrono::{DateTime, Duration, Local};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

const DEFAULT_ROWS: usize = 200;
const MAX_ROWS: usize = 5000;
const MORE_STEP: usize = 500;
const ARGS_CHARS: usize = 120;
const MAX_BUCKETS: i64 = 48;
const REFRESH_SECONDS: u32 = 30;
/// Bucket sizes for the "all" range, in seconds.
const STEPS: [i64; 18] = [
    60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172_800, 604_800,
    1_209_600, 2_592_000, 7_776_000, 31_536_000,
];

/// Escapes text for HTML text and attribute values.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------- query parameters

#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    Hour,
    Day,
    Week,
    All,
}

impl Range {
    const ALL: [Range; 4] = [Range::Hour, Range::Day, Range::Week, Range::All];

    fn key(self) -> &'static str {
        match self {
            Range::Hour => "1h",
            Range::Day => "24h",
            Range::Week => "7d",
            Range::All => "all",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Range::Hour => "1 h",
            Range::Day => "24 h",
            Range::Week => "7 d",
            Range::All => "All loaded",
        }
    }

    fn window(self) -> Option<Duration> {
        match self {
            Range::Hour => Some(Duration::hours(1)),
            Range::Day => Some(Duration::hours(24)),
            Range::Week => Some(Duration::days(7)),
            Range::All => None,
        }
    }

    fn parse(key: &str) -> Option<Range> {
        Range::ALL.into_iter().find(|r| r.key() == key)
    }
}

const OUTCOMES: [&str; 4] = ["ok", "error", "rejected", "other"];

/// Whether the query asks for made-up data instead of this service's own.
pub fn is_demo(query: &[(String, String)]) -> bool {
    query.iter().any(|(k, v)| k == "demo" && v == "1")
}

#[derive(Clone)]
struct Params {
    range: Range,
    caller: String,
    tool: String,
    outcome: String,
    q: String,
    rows: usize,
    refresh: bool,
    /// Made-up data (`demo=1`), kept by every link.
    demo: bool,
}

impl Params {
    fn from_query(query: &[(String, String)]) -> Params {
        let get = |key: &str| {
            query
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let text = |key: &str, max: usize| {
            get(key)
                .map(|v| v.chars().take(max).collect::<String>())
                .unwrap_or_default()
        };
        Params {
            range: get("range").and_then(Range::parse).unwrap_or(Range::Day),
            caller: text("caller", 300),
            tool: text("tool", 300),
            outcome: get("outcome")
                .filter(|v| OUTCOMES.contains(v))
                .unwrap_or("")
                .to_string(),
            q: text("q", 200),
            rows: get("rows")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n >= 1)
                .map_or(DEFAULT_ROWS, |n| n.min(MAX_ROWS)),
            refresh: get("refresh") != Some("0"),
            demo: is_demo(query),
        }
    }

    /// A link to this page with `change` applied to the current parameters. Defaults are
    /// left out. The result is unescaped; escape it when writing it into an attribute.
    fn href(&self, change: impl FnOnce(&mut Params), fragment: &str) -> String {
        let mut p = self.clone();
        change(&mut p);
        let mut pairs = form_urlencoded::Serializer::new(String::new());
        if p.demo {
            pairs.append_pair("demo", "1");
        }
        if p.range != Range::Day {
            pairs.append_pair("range", p.range.key());
        }
        for (key, value) in [
            ("caller", &p.caller),
            ("tool", &p.tool),
            ("outcome", &p.outcome),
            ("q", &p.q),
        ] {
            if !value.is_empty() {
                pairs.append_pair(key, value);
            }
        }
        if p.rows != DEFAULT_ROWS {
            pairs.append_pair("rows", &p.rows.to_string());
        }
        if !p.refresh {
            pairs.append_pair("refresh", "0");
        }
        let query = pairs.finish();
        if query.is_empty() {
            format!("/{fragment}")
        } else {
            format!("/?{query}{fragment}")
        }
    }
}

// ---------------------------------------------------------------- events

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ok,
    Error,
    Rejected,
    Other,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Ok => "ok",
            Kind::Error => "error",
            Kind::Rejected => "rejected",
            Kind::Other => "other",
        }
    }
}

struct Ev<'a> {
    raw: &'a Value,
    time: Option<DateTime<Local>>,
    kind: Kind,
    event: String,
    tool: Option<String>,
    transport: String,
    label: String,
    detail: String,
    text: String,
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A value as plain text: strings as they are, everything else as JSON.
fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn base_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// `/Applications/Claude.app/Contents/MacOS/Claude` is "Claude"; other paths give their
/// last component.
fn app_name(path: &str) -> String {
    for part in path.split('/') {
        if let Some(name) = part.strip_suffix(".app").filter(|n| !n.is_empty()) {
            return name.to_string();
        }
    }
    base_name(path).to_string()
}

/// The caller's label and a secondary line.
fn caller_of(event: &Value) -> (String, String) {
    let caller = event.get("caller").unwrap_or(&Value::Null);
    let get = |key: &str| str_of(caller, key);
    let tags = caller
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .map(plain)
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let who = if tags.is_empty() {
        get("user").unwrap_or("").to_string()
    } else {
        tags
    };
    let pid = caller
        .get("pid")
        .filter(|p| !p.is_null())
        .map(|p| format!("pid {}", plain(p)))
        .unwrap_or_default();
    let join = |parts: &[&str]| {
        parts
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" · ")
    };
    if let Some(device) = get("device") {
        (device.to_string(), join(&[&who, get("ip").unwrap_or("")]))
    } else if let Some(ip) = get("ip") {
        (ip.to_string(), who)
    } else if let Some(path) = get("parent_program") {
        (app_name(path), pid)
    } else if let Some(path) = get("program") {
        (app_name(path), pid)
    } else {
        ("unknown".to_string(), String::new())
    }
}

fn parse_event(raw: &Value) -> Ev<'_> {
    let event = str_of(raw, "event").unwrap_or("unknown").to_string();
    let kind = match event.as_str() {
        "rejected" => Kind::Rejected,
        "tool_call" => match str_of(raw, "outcome") {
            Some("ok") => Kind::Ok,
            Some("error") => Kind::Error,
            None if raw.get("error").is_some_and(|e| !e.is_null()) => Kind::Error,
            _ => Kind::Other,
        },
        _ => Kind::Other,
    };
    let (label, detail) = caller_of(raw);
    Ev {
        raw,
        time: str_of(raw, "time")
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Local)),
        kind,
        tool: str_of(raw, "tool").map(str::to_string),
        transport: str_of(raw, "transport").unwrap_or("").to_string(),
        text: raw.to_string().to_lowercase(),
        event,
        label,
        detail,
    }
}

// ---------------------------------------------------------------- formatting helpers

fn span_text(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

fn ago(secs: i64) -> String {
    if secs < 5 {
        "just now".to_string()
    } else {
        format!("{} ago", span_text(secs))
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn args_summary(raw: &Value) -> String {
    let Some(arguments) = raw.get("arguments") else {
        return String::new();
    };
    let text = match arguments {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={}", plain(v)))
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null => String::new(),
        other => plain(other),
    };
    let one_line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    truncate(&one_line, ARGS_CHARS)
}

fn duration_text(ms: u64) -> String {
    if ms >= 10_000 {
        format!("{:.0} s", ms as f64 / 1000.0)
    } else if ms >= 1000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

// ---------------------------------------------------------------- sections

fn header(out: &mut String, status: &Value, params: &Params, now: DateTime<Local>) {
    let service = str_of(status, "service").unwrap_or("service");
    let version = str_of(status, "version").unwrap_or("?");
    let uptime = str_of(status, "started_at")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|s| {
            format!(
                "up {}",
                span_text((now - s.with_timezone(&Local)).num_seconds())
            )
        })
        .unwrap_or_else(|| "running".to_string());
    let _ = write!(
        out,
        "<header class=\"top\"><div class=\"top-inner\"><h1 class=\"brand\">msgmcp</h1>\
         <div class=\"pill pill-up\" role=\"status\"><span class=\"dot\"></span>{} · {} · v{}</div>\
         <div class=\"top-spacer\"></div><span class=\"muted small\">Updated {}</span>",
        esc(service),
        esc(&uptime),
        esc(version),
        now.format("%H:%M:%S"),
    );
    if params.refresh {
        let _ = write!(
            out,
            "<a class=\"btn\" href=\"{}\">Pause auto-refresh</a>",
            esc(&params.href(|p| p.refresh = false, ""))
        );
        out.push_str("<span class=\"muted small\">Pause to keep expanded rows open.</span>");
    } else {
        let _ = write!(
            out,
            "<a class=\"btn pressed\" href=\"{}\">Resume auto-refresh</a>",
            esc(&params.href(|p| p.refresh = true, ""))
        );
    }
    let _ = write!(
        out,
        "<a class=\"btn\" href=\"{}\">Refresh</a></div></header>",
        esc(&params.href(|_| {}, ""))
    );
}

fn fact(out: &mut String, name: &str, value: &str, mono: bool) {
    let _ = write!(
        out,
        "<div><dt>{}</dt><dd{}>{}</dd></div>",
        esc(name),
        if mono { " class=\"mono\"" } else { "" },
        esc(value)
    );
}

fn configuration(out: &mut String, status: &Value, now: DateTime<Local>) {
    out.push_str("<section id=\"config\" aria-labelledby=\"h-config\"><h2 id=\"h-config\">Configuration</h2>");
    match status.get("send_enabled").and_then(Value::as_bool) {
        Some(true) => out.push_str(
            "<div class=\"send-state send-on\"><strong>Sending is enabled</strong>\
             <span class=\"muted\">send_message can send real iMessages; it is the only tool with side effects.</span></div>",
        ),
        Some(false) => out.push_str(
            "<div class=\"send-state send-off\"><strong>Sending is disabled</strong>\
             <span class=\"muted\">Every tool is read-only.</span></div>",
        ),
        None => out.push_str(
            "<div class=\"send-state\"><strong>Sending state unknown</strong></div>",
        ),
    }

    let hosts: Vec<String> = status
        .get("allowed_hosts")
        .and_then(Value::as_array)
        .map(|h| h.iter().map(plain).collect())
        .unwrap_or_default();
    let started = str_of(status, "started_at")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|s| {
            let s = s.with_timezone(&Local);
            format!(
                "{} ({} ago)",
                s.format("%b %-d %H:%M:%S"),
                span_text((now - s).num_seconds())
            )
        })
        .unwrap_or_else(|| "—".to_string());
    out.push_str("<dl class=\"facts\">");
    fact(
        out,
        "Service",
        str_of(status, "service").unwrap_or("—"),
        false,
    );
    fact(
        out,
        "Endpoint",
        str_of(status, "endpoint").unwrap_or("—"),
        true,
    );
    fact(
        out,
        "Remote hosts",
        &if hosts.is_empty() {
            "local only".to_string()
        } else {
            hosts.join(", ")
        },
        false,
    );
    fact(
        out,
        "Version",
        str_of(status, "version").unwrap_or("—"),
        false,
    );
    fact(
        out,
        "Process ID",
        &status.get("pid").map_or("—".to_string(), plain),
        false,
    );
    fact(out, "Started", &started, false);
    fact(
        out,
        "Audit log",
        str_of(status, "audit_log").unwrap_or("—"),
        true,
    );
    out.push_str("</dl>");

    out.push_str("<div class=\"subhead\">Message stores</div><div class=\"cards\">");
    let stores = status
        .get("stores")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if stores.is_empty() {
        out.push_str("<div class=\"card warn\">No message stores are enabled.</div>");
    }
    for store in &stores {
        let names: Vec<String> = store
            .get("names")
            .and_then(Value::as_array)
            .map(|n| n.iter().map(plain).collect())
            .unwrap_or_default();
        let title = if names.is_empty() {
            str_of(store, "source").unwrap_or("store").to_string()
        } else {
            names.join(", ")
        };
        let readable = store.get("readable").and_then(Value::as_bool) == Some(true);
        let _ = write!(
            out,
            "<div class=\"card{}\"><div class=\"card-title\"><span>{}</span><span class=\"{}\">{}</span></div>\
             <div class=\"path mono\">{}</div>",
            if readable { "" } else { " bad" },
            esc(&title),
            if readable { "status-ok" } else { "status-bad" },
            if readable {
                "✓ Readable"
            } else {
                "✗ Not readable"
            },
            esc(str_of(store, "path").unwrap_or("")),
        );
        if let Some(error) = str_of(store, "error") {
            let _ = write!(out, "<div class=\"err\">{}</div>", esc(error));
        }
        out.push_str("</div>");
    }
    out.push_str("</div><div class=\"subhead\">Contacts</div><div class=\"cards\">");
    let contacts = status.get("contacts").unwrap_or(&Value::Null);
    let databases = contacts.get("databases").and_then(Value::as_u64);
    let found = databases.is_some_and(|n| n > 0);
    let _ = write!(
        out,
        "<div class=\"card{}\"><div class=\"card-title\"><span>Contacts</span><span class=\"{}\">{}</span></div>\
         <div class=\"path mono\">{}</div>",
        if found { "" } else { " warn" },
        if found { "status-ok" } else { "status-warn" },
        match databases {
            Some(n) if n > 0 => format!("✓ {n} database{}", if n == 1 { "" } else { "s" }),
            _ => "⚠ No databases".to_string(),
        },
        esc(str_of(contacts, "path").unwrap_or("")),
    );
    if !found {
        out.push_str("<div class=\"err\">No contact databases were found, so results show phone numbers and addresses without names.</div>");
    }
    out.push_str("</div></div><div class=\"subhead\">Tools</div><div class=\"chips\">");
    let tools = status
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for tool in &tools {
        let name = plain(tool);
        let _ = write!(
            out,
            "<span class=\"chip{}\">{}</span>",
            if name == "send_message" { " side" } else { "" },
            esc(&name)
        );
    }
    out.push_str("</div></section>");
}

fn local_naive_secs(t: DateTime<Local>) -> i64 {
    t.naive_local().and_utc().timestamp()
}

fn step_text(step: i64) -> String {
    match step {
        s if s % 86400 == 0 => format!("{}-day", s / 86400),
        s if s % 3600 == 0 => format!("{}-hour", s / 3600),
        s => format!("{}-minute", s / 60),
    }
}

fn nice_max(max: u32) -> u32 {
    let mut scale = 1u32;
    loop {
        for m in [1, 2, 5] {
            if m * scale >= max {
                return m * scale;
            }
        }
        scale *= 10;
    }
}

/// The stacked bar chart of events per time bucket, as an SVG with native tooltips.
fn chart(events: &[&Ev], range: Range, now: DateTime<Local>) -> (String, Option<String>) {
    if events.is_empty() {
        return (
            "<div class=\"chart-empty\">No events in this range.</div>".to_string(),
            None,
        );
    }
    let now_secs = local_naive_secs(now);
    let times: Vec<i64> = events
        .iter()
        .filter_map(|e| e.time)
        .map(local_naive_secs)
        .collect();
    let start = match range.window() {
        Some(w) => local_naive_secs(now - w),
        None => times.iter().copied().min().unwrap_or(now_secs),
    };
    let count = |step: i64| (now_secs.div_euclid(step) - start.div_euclid(step)) + 1;
    let step = match range {
        Range::Hour => 120,
        Range::Day => 1800,
        Range::Week => 21_600,
        Range::All => STEPS
            .into_iter()
            .find(|s| count(*s) <= MAX_BUCKETS)
            .unwrap_or(STEPS[STEPS.len() - 1]),
    };
    let n = count(step).clamp(1, MAX_BUCKETS);
    let first = now_secs.div_euclid(step) * step - (n - 1) * step;
    let mut buckets = vec![[0u32; 4]; n as usize];
    for e in events {
        let Some(t) = e.time else { continue };
        let i = (local_naive_secs(t) - first).div_euclid(step);
        if (0..n).contains(&i) {
            let slot = match e.kind {
                Kind::Ok => 0,
                Kind::Error => 1,
                Kind::Rejected => 2,
                Kind::Other => 3,
            };
            buckets[i as usize][slot] += 1;
        }
    }
    let ymax = nice_max(
        buckets
            .iter()
            .map(|b| b.iter().sum::<u32>())
            .max()
            .unwrap_or(1)
            .max(1),
    );

    let (w, h) = (640.0, 200.0);
    let (left, right, top, bottom) = (34.0, 6.0, 8.0, 22.0);
    let (pw, ph) = (w - left - right, h - top - bottom);
    let slot = pw / n as f64;
    let y_of = |v: f64| top + ph - v / ymax as f64 * ph;
    let label_time = |secs: i64| {
        chrono::DateTime::from_timestamp(secs, 0)
            .map(|t| t.naive_utc())
            .unwrap_or_default()
    };
    let label_format = if step >= 86400 {
        "%b %-d"
    } else if n * step <= 2 * 86400 {
        "%H:%M"
    } else {
        "%b %-d %H:%M"
    };

    let mut svg = String::new();
    let _ = write!(
        svg,
        "<svg viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Events over time\" xmlns=\"http://www.w3.org/2000/svg\">"
    );
    for (value, label) in [
        (0, true),
        (ymax / 2, ymax.is_multiple_of(2) && ymax > 2),
        (ymax, true),
    ] {
        let y = y_of(value as f64);
        let _ = write!(
            svg,
            "<line class=\"{}\" x1=\"{left}\" x2=\"{}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/>",
            if value == 0 { "axis" } else { "grid" },
            w - right,
        );
        if label {
            let _ = write!(
                svg,
                "<text x=\"{}\" y=\"{:.1}\" text-anchor=\"end\">{value}</text>",
                left - 5.0,
                y + 3.5
            );
        }
    }
    let classes = ["b-ok", "b-error", "b-rejected", "b-other"];
    let names = ["ok", "error", "rejected", "other"];
    for (i, b) in buckets.iter().enumerate() {
        let x0 = left + i as f64 * slot;
        let begin = label_time(first + i as i64 * step);
        let end = label_time(first + (i as i64 + 1) * step);
        let mut tip = format!(
            "{} – {}",
            begin.format("%b %-d %H:%M"),
            end.format(
                if begin.date() == end.date() || end.time() == chrono::NaiveTime::MIN {
                    "%H:%M"
                } else {
                    "%b %-d %H:%M"
                }
            )
        );
        let total: u32 = b.iter().sum();
        if total == 0 {
            tip.push_str(": no events");
        } else {
            let parts: Vec<String> = (0..4)
                .filter(|k| b[*k] > 0)
                .map(|k| format!("{} {}", b[k], names[k]))
                .collect();
            let _ = write!(tip, ": {}", parts.join(", "));
        }
        let _ = write!(
            svg,
            "<g class=\"bk\"><title>{}</title><rect class=\"hit\" x=\"{x0:.2}\" y=\"{top}\" width=\"{slot:.2}\" height=\"{ph}\"/>",
            esc(&tip)
        );
        let mut acc = 0u32;
        for k in 0..4 {
            if b[k] == 0 {
                continue;
            }
            let y_top = y_of((acc + b[k]) as f64);
            let y_bottom = y_of(acc as f64);
            let _ = write!(
                svg,
                "<rect class=\"{}\" x=\"{:.2}\" y=\"{y_top:.2}\" width=\"{:.2}\" height=\"{:.2}\"/>",
                classes[k],
                x0 + slot * 0.1,
                slot * 0.8,
                y_bottom - y_top
            );
            acc += b[k];
        }
        svg.push_str("</g>");
    }
    let every = (n as usize).div_ceil(6).max(1);
    for i in (0..n as usize).step_by(every) {
        let t = label_time(first + i as i64 * step);
        let _ = write!(
            svg,
            "<text x=\"{:.1}\" y=\"{}\" text-anchor=\"middle\">{}</text>",
            left + (i as f64 + 0.5) * slot,
            h - 6.0,
            esc(&t.format(label_format).to_string())
        );
    }
    svg.push_str("</svg>");
    (svg, Some(step_text(step)))
}

struct CallerRow {
    detail: String,
    transports: BTreeSet<String>,
    calls: u32,
    errors: u32,
    rejected: u32,
    last: Option<DateTime<Local>>,
}

fn activity(
    out: &mut String,
    events: &[&Ev],
    more: bool,
    loaded: usize,
    params: &Params,
    now: DateTime<Local>,
) {
    out.push_str("<section id=\"activity\" aria-labelledby=\"h-activity\"><div class=\"sec-head\"><h2 id=\"h-activity\">Activity</h2><nav class=\"seg\" aria-label=\"Time range\">");
    for range in Range::ALL {
        let current = range == params.range;
        let _ = write!(
            out,
            "<a href=\"{}\"{}>{}</a>",
            esc(&params.href(|p| p.range = range, "#activity")),
            if current {
                " aria-current=\"page\""
            } else {
                ""
            },
            range.label()
        );
    }
    out.push_str("</nav></div>");
    if more {
        let _ = write!(
            out,
            "<div class=\"note\">Only the newest {loaded} events are loaded; older events are in the audit log.</div>"
        );
    }

    let calls = events.iter().filter(|e| e.event == "tool_call").count();
    let errors = events.iter().filter(|e| e.kind == Kind::Error).count();
    let rejected = events.iter().filter(|e| e.kind == Kind::Rejected).count();
    let sent = events
        .iter()
        .filter(|e| e.tool.as_deref() == Some("send_message") && e.kind == Kind::Ok)
        .count();
    let callers: BTreeSet<&str> = events.iter().map(|e| e.label.as_str()).collect();
    out.push_str("<div class=\"kpis\">");
    for (class, value, label) in [
        ("k-accent", calls, "Tool calls"),
        ("k-error", errors, "Errors"),
        ("k-rejected", rejected, "Rejected requests"),
        ("k-send", sent, "Messages sent"),
        ("k-ok", callers.len(), "Distinct callers"),
    ] {
        let _ = write!(
            out,
            "<div class=\"kpi {class}\"><div class=\"v\">{value}</div><div class=\"l\">{label}</div></div>"
        );
    }
    out.push_str("</div>");

    let (svg, step) = chart(events, params.range, now);
    let bucket_note = step.map_or(String::new(), |s| {
        format!("<span class=\"muted small\">{} buckets</span>", esc(&s))
    });
    let _ = write!(
        out,
        "<div class=\"panel\"><div class=\"panel-head\"><h3>Events over time</h3><div class=\"legend\">\
         <span class=\"lg\"><i class=\"sw sw-ok\"></i>ok</span>\
         <span class=\"lg\"><i class=\"sw sw-error\"></i>error</span>\
         <span class=\"lg\"><i class=\"sw sw-rejected\"></i>rejected</span>\
         <span class=\"lg\"><i class=\"sw sw-other\"></i>other</span>{}</div></div>\
         <div class=\"chart\">{}</div></div>",
        bucket_note, svg
    );

    // By tool.
    let mut tools: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    for e in events.iter().filter(|e| e.event == "tool_call") {
        if let Some(tool) = e.tool.as_deref() {
            let entry = tools.entry(tool).or_default();
            entry.0 += 1;
            if e.kind == Kind::Error {
                entry.1 += 1;
            }
        }
    }
    let mut tools: Vec<_> = tools.into_iter().collect();
    tools.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
    let top_calls = tools.first().map_or(1, |t| t.1.0).max(1);
    out.push_str("<div class=\"two\"><div class=\"panel\"><div class=\"panel-head\"><h3>By tool</h3><span class=\"muted small\">Select a tool to filter the log</span></div>");
    if tools.is_empty() {
        out.push_str("<div class=\"empty\">No tool calls in this range.</div>");
    } else {
        out.push_str("<div class=\"tablewrap\"><table class=\"tbl\"><thead><tr><th>Tool</th><th class=\"num\">Calls</th><th class=\"num\">Errors</th><th class=\"barcol\">Share</th></tr></thead><tbody>");
        for (tool, (n, errs)) in &tools {
            let ok_w = f64::from(n - errs) / f64::from(top_calls) * 100.0;
            let err_w = f64::from(*errs) / f64::from(top_calls) * 100.0;
            let _ = write!(
                out,
                "<tr{}><td class=\"mono\"><a href=\"{}\">{}</a></td><td class=\"num\">{n}</td><td class=\"num\">{errs}</td>\
                 <td class=\"barcol\"><svg class=\"hbar\" viewBox=\"0 0 100 8\" preserveAspectRatio=\"none\" aria-hidden=\"true\">\
                 <rect class=\"seg-ok\" x=\"0\" y=\"0\" width=\"{ok_w:.2}\" height=\"8\"/>\
                 <rect class=\"seg-error\" x=\"{ok_w:.2}\" y=\"0\" width=\"{err_w:.2}\" height=\"8\"/></svg></td></tr>",
                if params.tool == *tool {
                    " class=\"active\""
                } else {
                    ""
                },
                esc(&params.href(|p| p.tool = (*tool).to_string(), "#events")),
                esc(tool),
            );
        }
        out.push_str("</tbody></table></div>");
    }
    out.push_str("</div>");

    // Callers.
    let mut rows: BTreeMap<&str, CallerRow> = BTreeMap::new();
    for e in events {
        let row = rows.entry(e.label.as_str()).or_insert_with(|| CallerRow {
            detail: String::new(),
            transports: BTreeSet::new(),
            calls: 0,
            errors: 0,
            rejected: 0,
            last: None,
        });
        if row.detail.is_empty() {
            row.detail.clone_from(&e.detail);
        }
        if !e.transport.is_empty() {
            row.transports.insert(e.transport.clone());
        }
        if e.event == "tool_call" {
            row.calls += 1;
        }
        match e.kind {
            Kind::Error => row.errors += 1,
            Kind::Rejected => row.rejected += 1,
            _ => {}
        }
        if let Some(t) = e.time
            && row.last.is_none_or(|l| t > l)
        {
            row.last = Some(t);
        }
    }
    let mut rows: Vec<_> = rows.into_iter().collect();
    rows.sort_by(|a, b| {
        (b.1.calls + b.1.rejected)
            .cmp(&(a.1.calls + a.1.rejected))
            .then(a.0.cmp(b.0))
    });
    out.push_str("<div class=\"panel\"><div class=\"panel-head\"><h3>Callers</h3><span class=\"muted small\">Select a caller to filter the log</span></div>");
    if rows.is_empty() {
        out.push_str("<div class=\"empty\">No callers in this range.</div>");
    } else {
        out.push_str("<div class=\"tablewrap\"><table class=\"tbl\"><thead><tr><th>Caller</th><th>Transport</th><th class=\"num\">Calls</th><th class=\"num\">Errors</th><th class=\"num\">Rejected</th><th>Last seen</th></tr></thead><tbody>");
        for (label, row) in &rows {
            let last = row
                .last
                .map_or("—".to_string(), |t| ago((now - t).num_seconds()));
            let _ = write!(
                out,
                "<tr{}><td><a class=\"who\" href=\"{}\">{}</a>{}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"nowrap\">{}</td></tr>",
                if params.caller == *label {
                    " class=\"active\""
                } else {
                    ""
                },
                esc(&params.href(|p| p.caller = (*label).to_string(), "#events")),
                esc(label),
                if row.detail.is_empty() {
                    String::new()
                } else {
                    format!("<div class=\"who-detail\">{}</div>", esc(&row.detail))
                },
                esc(&row
                    .transports
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")),
                row.calls,
                row.errors,
                row.rejected,
                esc(&last),
            );
        }
        out.push_str("</tbody></table></div>");
    }
    out.push_str("</div></div></section>");
}

fn select(
    out: &mut String,
    name: &str,
    all_label: &str,
    current: &str,
    options: &BTreeSet<String>,
) {
    let _ = write!(
        out,
        "<select name=\"{name}\" aria-label=\"{}\"><option value=\"\">{}</option>",
        esc(all_label),
        esc(all_label)
    );
    let mut options = options.clone();
    if !current.is_empty() {
        options.insert(current.to_string());
    }
    for option in &options {
        let _ = write!(
            out,
            "<option value=\"{0}\"{1}>{0}</option>",
            esc(option),
            if option == current { " selected" } else { "" }
        );
    }
    out.push_str("</select>");
}

fn event_row(out: &mut String, e: &Ev, now: DateTime<Local>) {
    let send = e.tool.as_deref() == Some("send_message");
    let raw_time = str_of(e.raw, "time").unwrap_or("");
    let time = match e.time {
        Some(t) if t.date_naive() == now.date_naive() => t.format("%H:%M:%S").to_string(),
        Some(t) => t.format("%b %-d %H:%M:%S").to_string(),
        None if raw_time.is_empty() => "—".to_string(),
        None => raw_time.to_string(),
    };
    let name = match (&e.tool, e.event.as_str()) {
        (Some(tool), _) => tool.clone(),
        (None, event) => event.to_string(),
    };
    // A refusal the caller asked for, to check the gateway, went as expected, so it is green.
    // Any caller can claim one, so it still counts as a rejection in the totals and filters.
    let probe =
        e.kind == Kind::Rejected && e.raw.get("probe").and_then(Value::as_bool) == Some(true);
    let mut badge = format!(
        "<span class=\"badge {}\"{}>{}</span>",
        if probe { "ok" } else { e.kind.key() },
        if probe {
            " title=\"The caller marked this request as a check that the gateway refuses it. \
             Any caller can send that mark.\""
        } else {
            ""
        },
        match e.kind {
            Kind::Rejected => {
                let label = if probe { "self-test" } else { "rejected" };
                match e.raw.get("status").filter(|s| !s.is_null()) {
                    Some(status) => format!("{label} {}", esc(&plain(status))),
                    None => label.to_string(),
                }
            }
            Kind::Other if e.event == "tool_call" =>
                esc(str_of(e.raw, "outcome").unwrap_or("other")),
            Kind::Other => "other".to_string(),
            kind => kind.key().to_string(),
        }
    );
    if let Some(reason) = str_of(e.raw, "reason") {
        let _ = write!(badge, "<span class=\"reason\">{}</span>", esc(reason));
    }
    if let Some(error) = e.raw.get("error").filter(|v| !v.is_null()) {
        let _ = write!(
            badge,
            "<span class=\"errtext\">{}</span>",
            esc(&plain(error))
        );
    }
    let duration = e
        .raw
        .get("duration_ms")
        .and_then(Value::as_u64)
        .map(duration_text)
        .unwrap_or_default();
    let _ = write!(
        out,
        "<details class=\"ev {}{}\"><summary class=\"evrow\">\
         <span class=\"t\" title=\"{}\">{}</span><span class=\"cwho\">{}</span><span class=\"evn\">{}</span>\
         <span class=\"out\">{}</span><span class=\"dur\">{}</span><span class=\"args\">{}</span></summary>\
         <pre>{}</pre></details>",
        e.kind.key(),
        if send { " send" } else { "" },
        esc(raw_time),
        esc(&time),
        esc(&e.label),
        esc(&name),
        badge,
        esc(&duration),
        esc(&args_summary(e.raw)),
        esc(&serde_json::to_string_pretty(e.raw).unwrap_or_default()),
    );
}

fn log(out: &mut String, in_range: &[&Ev], params: &Params, now: DateTime<Local>) {
    let needle = params.q.to_lowercase();
    let matching: Vec<&&Ev> = in_range
        .iter()
        .filter(|e| params.caller.is_empty() || e.label == params.caller)
        .filter(|e| params.tool.is_empty() || e.tool.as_deref() == Some(params.tool.as_str()))
        .filter(|e| params.outcome.is_empty() || e.kind.key() == params.outcome)
        .filter(|e| needle.is_empty() || e.text.contains(&needle))
        .collect();
    let shown = matching.len().min(params.rows);

    let callers: BTreeSet<String> = in_range.iter().map(|e| e.label.clone()).collect();
    let tools: BTreeSet<String> = in_range.iter().filter_map(|e| e.tool.clone()).collect();
    let outcomes: BTreeSet<String> = OUTCOMES.iter().map(|o| (*o).to_string()).collect();

    out.push_str("<section id=\"events\" aria-labelledby=\"h-log\"><div class=\"sec-head\"><h2 id=\"h-log\">Event log</h2>");
    let _ = write!(
        out,
        "<span class=\"muted small\">{} of {} events{}</span></div>",
        matching.len(),
        in_range.len(),
        if shown < matching.len() {
            format!(", showing the newest {shown}")
        } else {
            String::new()
        }
    );
    out.push_str("<form class=\"filters\" method=\"get\" action=\"/#events\">");
    if params.range != Range::Day {
        let _ = write!(
            out,
            "<input type=\"hidden\" name=\"range\" value=\"{}\">",
            params.range.key()
        );
    }
    if params.rows != DEFAULT_ROWS {
        let _ = write!(
            out,
            "<input type=\"hidden\" name=\"rows\" value=\"{}\">",
            params.rows
        );
    }
    if !params.refresh {
        out.push_str("<input type=\"hidden\" name=\"refresh\" value=\"0\">");
    }
    if params.demo {
        out.push_str("<input type=\"hidden\" name=\"demo\" value=\"1\">");
    }
    let _ = write!(
        out,
        "<input type=\"search\" name=\"q\" value=\"{}\" placeholder=\"Search events\" aria-label=\"Search events\" autocomplete=\"off\" spellcheck=\"false\">",
        esc(&params.q)
    );
    select(out, "outcome", "All outcomes", &params.outcome, &outcomes);
    select(out, "caller", "All callers", &params.caller, &callers);
    select(out, "tool", "All tools", &params.tool, &tools);
    let _ = write!(
        out,
        "<button class=\"btn\" type=\"submit\">Filter</button><a class=\"btn\" href=\"{}\">Clear filters</a></form>",
        esc(&params.href(
            |p| {
                p.caller.clear();
                p.tool.clear();
                p.outcome.clear();
                p.q.clear();
            },
            "#events"
        ))
    );

    if matching.is_empty() {
        out.push_str(if in_range.is_empty() {
            "<div class=\"empty\">No events in this range.</div>"
        } else {
            "<div class=\"empty\">No events match these filters.</div>"
        });
    } else {
        out.push_str("<div class=\"logwrap\"><div class=\"loghead\"><span>Time</span><span>Caller</span><span>Event</span><span>Outcome</span><span class=\"dur\">Duration</span><span class=\"args\">Arguments</span></div>");
        for e in matching.iter().take(shown) {
            event_row(out, e, now);
        }
        out.push_str("</div>");
    }
    if shown < matching.len() && params.rows < MAX_ROWS {
        let _ = write!(
            out,
            "<div class=\"more\"><a class=\"btn\" href=\"{}\">Show more</a></div>",
            esc(&params.href(|p| p.rows = (p.rows + MORE_STEP).min(MAX_ROWS), "#events"))
        );
    }
    out.push_str("</section>");
}

pub fn render(
    status: &Value,
    events: &[Value],
    more: bool,
    query: &[(String, String)],
    now: DateTime<Local>,
) -> String {
    let params = Params::from_query(query);
    let parsed: Vec<Ev> = events.iter().map(parse_event).collect();
    let cutoff = params.range.window().map(|w| now - w);
    let in_range: Vec<&Ev> = parsed
        .iter()
        .filter(|e| match (cutoff, e.time) {
            (None, _) => true,
            (Some(cutoff), Some(t)) => t >= cutoff,
            (Some(_), None) => false,
        })
        .collect();

    let mut out = String::with_capacity(64 * 1024);
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta name=\"color-scheme\" content=\"light dark\">");
    if params.refresh {
        let _ = write!(
            out,
            "<meta http-equiv=\"refresh\" content=\"{REFRESH_SECONDS}\">"
        );
    }
    out.push_str(if params.demo {
        "<title>msgmcp (demo data)</title>"
    } else {
        "<title>msgmcp</title>"
    });
    out.push_str("<link rel=\"stylesheet\" href=\"/app.css\">");
    out.push_str("<link rel=\"icon\" href=\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Crect width='16' height='16' rx='4' fill='%231f6f8b'/%3E%3Cpath d='M4 11V6l4 3 4-3v5' fill='none' stroke='white' stroke-width='1.6'/%3E%3C/svg%3E\">");
    out.push_str("</head><body>");
    header(&mut out, status, &params, now);
    out.push_str("<main>");
    configuration(&mut out, status, now);
    activity(&mut out, &in_range, more, events.len(), &params, now);
    log(&mut out, &in_range, &params, now);
    out.push_str("</main></body></html>");
    out
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::SecondsFormat;
    use serde_json::json;

    const XSS: &str = "<script>alert(1)</script>\"><img src=x onerror=alert(1)>";

    fn status() -> Value {
        json!({"service":"gateway","version":"0.1.0","pid":12345,
            "started_at":"2026-10-19T12:00:00.123+00:00",
            "endpoint":"http://127.0.0.1:8765/mcp","allowed_hosts":["my-mac.example.ts.net"],
            "send_enabled":true,
            "tools":["read_conversation","search_messages","send_message"],
            "stores":[{"source":"imessage","names":["iMessage","SMS"],"path":"/Users/x/Library/Messages/chat.db","readable":true,"error":null}],
            "contacts":{"path":"/Users/x/AddressBook","databases":2},
            "audit_log":"/Users/x/Library/Logs/msgmcp/audit.log"})
    }

    fn at(now: DateTime<Local>, minutes_ago: i64) -> String {
        (now - Duration::minutes(minutes_ago)).to_rfc3339_opts(SecondsFormat::Millis, false)
    }

    fn call(
        now: DateTime<Local>,
        minutes_ago: i64,
        tool: &str,
        device: &str,
        outcome: &str,
    ) -> Value {
        json!({"time": at(now, minutes_ago), "transport":"gateway","event":"tool_call","tool":tool,
            "caller":{"ip":"100.101.102.103","device":device,"tags":["tag:agent"]},
            "arguments":{"text":"dinner","limit":5},"outcome":outcome,
            "error": if outcome == "error" { json!("Unknown conversation") } else { Value::Null },
            "duration_ms":42})
    }

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn page(events: &[Value], more: bool, query: &[(&str, &str)]) -> String {
        render(&status(), events, more, &q(query), Local::now())
    }

    fn sample(now: DateTime<Local>) -> Vec<Value> {
        vec![
            call(now, 1, "search_messages", "grok-agent", "ok"),
            call(now, 5, "read_conversation", "grok-agent", "error"),
            json!({"time": at(now, 7), "transport":"gateway","event":"rejected","reason":"wrong token","status":401,
                "caller":{"ip":"100.64.0.9","device":"laptop","user":"someone@example.com"}}),
            json!({"time": at(now, 9), "transport":"socket","event":"tool_call","tool":"send_message",
                "caller":{"pid":42,"program":"/Users/x/Applications/msgmcp.app/Contents/MacOS/msgmcp",
                          "parent_program":"/Applications/Claude.app/Contents/MacOS/Claude"},
                "arguments":{"recipient":"+15551234567","text":"On my way"},"outcome":"ok","error":null,"duration_ms":900}),
            json!({"time": at(now, 3 * 60), "transport":"gateway","event":"mystery","caller":{"ip":"127.0.0.1"}}),
            call(now, 3 * 24 * 60, "search_messages", "old-device", "ok"),
        ]
    }

    /// Every tag has balanced quotes, so no value broke out of its attribute.
    fn assert_well_formed(html: &str) {
        for part in html.split('<').skip(1) {
            let tag = part.split('>').next().unwrap();
            assert_eq!(
                tag.matches('"').count() % 2,
                0,
                "unbalanced quotes in <{tag}>"
            );
        }
    }

    #[test]
    fn escapes_everything_from_events() {
        let now = Local::now();
        let events = vec![
            json!({"time": at(now, 1), "transport": XSS, "event": "tool_call", "tool": XSS,
            "caller": {"device": XSS, "ip": XSS, "tags": [XSS], "user": XSS},
            "arguments": {XSS: XSS}, "outcome": "error", "error": XSS, "duration_ms": 1}),
            json!({"time": at(now, 2), "event": "rejected", "reason": XSS, "status": XSS,
                "caller": {"device": XSS}}),
            json!({"time": XSS, "event": XSS, "caller": {"parent_program": XSS}}),
        ];
        for query in [
            vec![],
            vec![("range", "all")],
            vec![("q", XSS), ("caller", XSS), ("tool", XSS), ("outcome", XSS)],
            vec![("caller", XSS)],
            vec![
                ("tool", XSS),
                ("range", XSS),
                ("rows", XSS),
                ("refresh", XSS),
            ],
        ] {
            let html = render(&status(), &events, true, &q(&query), now);
            assert!(!html.contains("<script"), "{query:?}");
            assert!(!html.contains("<img"), "{query:?}");
            assert_well_formed(&html);
        }
        let html = render(&status(), &events, false, &q(&[("range", "all")]), now);
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;&quot;&gt;&lt;img"));
    }

    #[test]
    fn escapes_everything_from_status() {
        let mut s = status();
        s["service"] = json!(XSS);
        s["version"] = json!(XSS);
        s["endpoint"] = json!(XSS);
        s["allowed_hosts"] = json!([XSS]);
        s["tools"] = json!([XSS]);
        s["audit_log"] = json!(XSS);
        s["stores"] =
            json!([{"source": XSS, "names": [XSS], "path": XSS, "readable": false, "error": XSS}]);
        s["contacts"] = json!({"path": XSS, "databases": 0});
        let html = render(&s, &[], false, &[], Local::now());
        assert!(!html.contains("<script") && !html.contains("<img"));
        assert_well_formed(&html);
    }

    #[test]
    fn page_has_no_scripts_or_inline_styles() {
        let now = Local::now();
        let html = render(&status(), &sample(now), true, &[], now);
        assert!(!html.contains("<script"));
        assert!(!html.contains("<style"));
        assert!(!html.contains(" style="));
        assert!(!html.contains(" onclick"));
        assert!(html.contains("http-equiv=\"refresh\" content=\"30\""));
        assert_well_formed(&html);
    }

    #[test]
    fn query_values_are_escaped_in_form_and_links() {
        let html = page(&[], false, &[("q", "\"><b>x</b>&y='")]);
        assert!(html.contains("value=\"&quot;&gt;&lt;b&gt;x&lt;/b&gt;&amp;y=&#39;\""));
        assert!(html.contains("q=%22%3E%3Cb%3Ex%3C%2Fb%3E%26y%3D%27"));
        assert!(!html.contains("<b>"));
        assert_well_formed(&html);
    }

    #[test]
    fn range_scopes_events_and_kpis() {
        let now = Local::now();
        let events = sample(now);
        let day = render(&status(), &events, false, &[], now);
        assert!(day.contains("search_messages"));
        assert!(!day.contains("old-device"));
        assert!(day.contains("5 of 5 events"));
        let hour = render(&status(), &events, false, &q(&[("range", "1h")]), now);
        assert!(hour.contains("4 of 4 events"));
        assert!(!hour.contains("mystery"));
        let week = render(&status(), &events, false, &q(&[("range", "7d")]), now);
        assert!(week.contains("old-device"));
        let all = render(&status(), &events, false, &q(&[("range", "all")]), now);
        assert!(all.contains("6 of 6 events"));
        assert!(all.contains("aria-current=\"page\">All loaded"));
        assert!(day.contains("aria-current=\"page\">24 h"));
        // An invalid range falls back to 24 h.
        let bad = render(
            &status(),
            &events,
            false,
            &q(&[("range", "yesterday")]),
            now,
        );
        assert!(bad.contains("aria-current=\"page\">24 h"));
    }

    #[test]
    fn kpis_count_calls_errors_rejections_and_sends() {
        let now = Local::now();
        let html = render(&status(), &sample(now), false, &q(&[("range", "1h")]), now);
        let kpi = |label: &str| {
            let at = html
                .find(&format!("<div class=\"l\">{label}</div>"))
                .unwrap();
            let head = &html[..at];
            let v = head.rfind("<div class=\"v\">").unwrap() + "<div class=\"v\">".len();
            head[v..].split('<').next().unwrap().to_string()
        };
        assert_eq!(kpi("Tool calls"), "3");
        assert_eq!(kpi("Errors"), "1");
        assert_eq!(kpi("Rejected requests"), "1");
        assert_eq!(kpi("Messages sent"), "1");
        assert_eq!(kpi("Distinct callers"), "3");
    }

    #[test]
    fn filters_apply_to_the_list_only() {
        let now = Local::now();
        let events = sample(now);
        let by = |pairs: &[(&str, &str)]| render(&status(), &events, false, &q(pairs), now);
        let caller = by(&[("caller", "Claude")]);
        assert!(caller.contains("1 of 5 events"));
        assert!(caller.contains("On my way"));
        let tool = by(&[("tool", "read_conversation")]);
        assert!(tool.contains("1 of 5 events"));
        assert!(tool.contains("<option value=\"read_conversation\" selected>"));
        let outcome = by(&[("outcome", "rejected")]);
        assert!(outcome.contains("1 of 5 events"));
        assert!(outcome.contains("rejected 401"));
        let other = by(&[("outcome", "other")]);
        assert!(other.contains("1 of 5 events"));
        assert!(other.contains("mystery"));
        let text = by(&[("q", "ON MY WAY")]);
        assert!(text.contains("1 of 5 events"));
        // Filters leave the KPIs alone.
        assert!(text.contains("<div class=\"v\">3</div><div class=\"l\">Tool calls"));
        // An unknown outcome is ignored.
        assert!(by(&[("outcome", "bogus")]).contains("5 of 5 events"));
        assert!(by(&[("q", "no such text")]).contains("No events match these filters."));
    }

    #[test]
    fn caller_labels() {
        let (label, detail) =
            caller_of(&json!({"caller":{"ip":"1.2.3.4","device":"dev","tags":["tag:a","tag:b"]}}));
        assert_eq!(
            (label.as_str(), detail.as_str()),
            ("dev", "tag:a, tag:b · 1.2.3.4")
        );
        let (label, detail) = caller_of(&json!({"caller":{"ip":"1.2.3.4","user":"u@example.com"}}));
        assert_eq!(
            (label.as_str(), detail.as_str()),
            ("1.2.3.4", "u@example.com")
        );
        let (label, _) = caller_of(
            &json!({"caller":{"pid":1,"program":"/bin/tool","parent_program":"/Applications/Claude.app/Contents/MacOS/Claude"}}),
        );
        assert_eq!(label, "Claude");
        let (label, _) = caller_of(&json!({"caller":{"program":"/usr/local/bin/mytool"}}));
        assert_eq!(label, "mytool");
        assert_eq!(caller_of(&json!({})).0, "unknown");
    }

    #[test]
    fn links_preserve_other_parameters() {
        let now = Local::now();
        let html = render(
            &status(),
            &sample(now),
            false,
            &q(&[
                ("range", "7d"),
                ("q", "dinner"),
                ("refresh", "0"),
                ("rows", "50"),
            ]),
            now,
        );
        assert!(html.contains("href=\"/?range=7d&amp;q=dinner&amp;rows=50\">Resume auto-refresh"));
        assert!(!html.contains("http-equiv=\"refresh\""));
        assert!(html.contains("href=\"/?q=dinner&amp;rows=50&amp;refresh=0#activity\">24 h"));
        assert!(html.contains("href=\"/?range=7d&amp;tool=search_messages&amp;q=dinner&amp;rows=50&amp;refresh=0#events\">search_messages"));
        assert!(html.contains("name=\"refresh\" value=\"0\""));
        assert!(html.contains("name=\"rows\" value=\"50\""));
        assert!(
            html.contains("href=\"/?range=7d&amp;rows=50&amp;refresh=0#events\">Clear filters")
        );
        // Defaults are omitted.
        let plain_page = render(&status(), &sample(now), false, &[], now);
        assert!(plain_page.contains("href=\"/?refresh=0\">Pause auto-refresh"));
        assert!(plain_page.contains("href=\"/#activity\" aria-current=\"page\">24 h"));
        assert!(plain_page.contains("href=\"/?range=1h#activity\">1 h"));
        assert!(plain_page.contains("Pause to keep expanded rows open"));
    }

    #[test]
    fn rows_limit_and_show_more() {
        let now = Local::now();
        let events: Vec<Value> = (0..30)
            .map(|i| call(now, i, "search_messages", "d", "ok"))
            .collect();
        let html = render(&status(), &events, false, &q(&[("rows", "10")]), now);
        assert_eq!(html.matches("<details").count(), 10);
        assert!(html.contains("30 of 30 events, showing the newest 10"));
        assert!(html.contains("href=\"/?rows=510#events\">Show more"));
        let all = render(&status(), &events, false, &q(&[("rows", "100")]), now);
        assert_eq!(all.matches("<details").count(), 30);
        assert!(!all.contains("Show more"));
        // rows is clamped and invalid values fall back.
        assert_eq!(Params::from_query(&q(&[("rows", "999999")])).rows, MAX_ROWS);
        assert_eq!(Params::from_query(&q(&[("rows", "0")])).rows, DEFAULT_ROWS);
        assert_eq!(Params::from_query(&q(&[("rows", "x")])).rows, DEFAULT_ROWS);
    }

    #[test]
    fn more_shows_the_truncation_note() {
        let now = Local::now();
        let events = sample(now);
        let with = render(&status(), &events, true, &[], now);
        assert!(with.contains("Only the newest 6 events are loaded"));
        let without = render(&status(), &events, false, &[], now);
        assert!(!without.contains("are loaded"));
    }

    #[test]
    fn probes_are_labeled_and_still_counted_as_rejections() {
        let now = Local::now();
        let rejected = |minutes, probe: Option<bool>| {
            let mut e = json!({"time": at(now, minutes), "transport":"gateway","event":"rejected",
                "reason":"missing token","status":401,"caller":{"ip":"100.64.0.10"}});
            if let Some(probe) = probe {
                e["probe"] = json!(probe);
            }
            e
        };
        let events = [
            rejected(1, Some(true)),
            rejected(2, None),
            rejected(3, Some(false)),
        ];
        let html = render(&status(), &events, false, &[], now);
        assert_eq!(html.matches(">self-test 401</span>").count(), 1);
        assert_eq!(html.matches(">rejected 401</span>").count(), 2);
        assert!(html.contains(
            "<span class=\"badge ok\" title=\"The caller marked this request as a check"
        ));
        let html = render(
            &status(),
            &events,
            false,
            &q(&[("outcome", "rejected")]),
            now,
        );
        assert!(html.contains("3 of 3 events"));
    }

    #[test]
    fn rows_show_details_and_highlight_sends() {
        let now = Local::now();
        let html = render(&status(), &sample(now), false, &[], now);
        assert!(html.contains("class=\"ev ok send\""));
        assert!(html.contains("recipient=+15551234567 text=On my way"));
        assert!(html.contains("<span class=\"errtext\">Unknown conversation</span>"));
        assert!(html.contains("<span class=\"reason\">wrong token</span>"));
        assert!(html.contains("<pre>{"));
        assert!(html.contains("title=\"20"));
        assert!(html.contains("<span class=\"chip side\">send_message</span>"));
        assert!(html.contains("Sending is enabled"));
        assert!(html.contains("<svg viewBox=\"0 0 640 200\""));
    }

    #[test]
    fn argument_summary_is_one_short_line() {
        let long = "x".repeat(500);
        let summary = args_summary(&json!({"arguments": {"a": "line1\nline2", "b": long}}));
        assert!(summary.starts_with("a=line1 line2 b=xxx"), "{summary}");
        assert_eq!(summary.chars().count(), ARGS_CHARS + 1);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn empty_page_renders() {
        for range in ["1h", "24h", "7d", "all"] {
            let html = page(&[], false, &[("range", range)]);
            assert!(html.contains("No events in this range."));
            assert!(html.contains("<div class=\"l\">Tool calls</div>"));
            assert!(!html.contains("<details"));
            assert_well_formed(&html);
        }
        assert!(page(&[], false, &[]).contains("0 of 0 events"));
    }

    #[test]
    fn unreadable_store_and_missing_contacts_are_flagged() {
        let mut s = status();
        s["stores"] = json!([{"source":"signal","names":["Signal"],"path":"/x/Signal","readable":false,"error":"Cannot read <it>"}]);
        s["contacts"] = json!({"path":"/x/AddressBook","databases":0});
        s["send_enabled"] = json!(false);
        s["service"] = json!("socket");
        s["allowed_hosts"] = json!([]);
        let html = render(&s, &[], false, &[], Local::now());
        assert!(html.contains("✗ Not readable"));
        assert!(html.contains("Cannot read &lt;it&gt;"));
        assert!(html.contains("card bad"));
        assert!(html.contains("card warn"));
        assert!(html.contains("⚠ No databases"));
        assert!(html.contains("Sending is disabled"));
        assert!(html.contains("local only"));
    }

    #[test]
    fn chart_has_at_most_48_buckets_with_titles() {
        let now = Local::now();
        let events = vec![
            call(now, 1, "search_messages", "d", "ok"),
            call(now, 40 * 24 * 60, "search_messages", "d", "error"),
        ];
        for range in ["1h", "24h", "7d", "all"] {
            let html = render(&status(), &events, false, &q(&[("range", range)]), now);
            let svg = &html[html.find("<svg viewBox=\"0 0 640").unwrap()..];
            let svg = &svg[..svg.find("</svg>").unwrap()];
            let buckets = svg.matches("<g class=\"bk\">").count();
            assert!((1..=48).contains(&buckets), "{range}: {buckets}");
            assert_eq!(svg.matches("<title>").count(), buckets);
        }
    }

    #[test]
    fn escape_helper() {
        assert_eq!(esc("&<>\"'"), "&amp;&lt;&gt;&quot;&#39;");
    }
}
