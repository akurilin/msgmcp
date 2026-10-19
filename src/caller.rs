//! Who made a request, for the audit log: the tailnet device behind the gateway, or the
//! process on the other end of the Unix socket. Identifying a caller never fails a request.

use serde::Serialize;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Caller {
    /// Gateway: the caller's address; for requests through `tailscale serve`, the tailnet
    /// address it reports in X-Forwarded-For.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
    /// Socket: the connecting process (normally the msgmcp stdio adapter) and the program
    /// that started it, such as the MCP client app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_program: Option<String>,
}

/// A tailnet device, from `tailscale whois`.
#[derive(Clone, Debug, Serialize)]
pub struct Device {
    pub device: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The owner's login, for devices that aren't tagged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

const TAILSCALE: [&str; 3] = [
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];
const CACHE_FOR: Duration = Duration::from_secs(600);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(1);

/// Lookups by address, with when they were made; `None` means no tailnet device answered.
type DeviceCache = HashMap<IpAddr, (Instant, Option<Device>)>;
static DEVICES: LazyLock<Mutex<DeviceCache>> = LazyLock::new(Default::default);

fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        // 100.64.0.0/10 and fd7a:115c:a1e0::/48.
        IpAddr::V4(v4) => v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64,
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

async fn whois(ip: IpAddr) -> Option<Device> {
    let program = TAILSCALE
        .iter()
        .find(|p| std::path::Path::new(p).exists())?;
    let output = tokio::process::Command::new(program)
        .args(["whois", "--json", &ip.to_string()])
        .kill_on_drop(true)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let node = &value["Node"];
    let tags: Vec<String> = node["Tags"]
        .as_array()
        .map(|t| {
            t.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Some(Device {
        device: node["Name"].as_str()?.trim_end_matches('.').to_owned(),
        user: value["UserProfile"]["LoginName"]
            .as_str()
            .filter(|_| tags.is_empty())
            .map(str::to_owned),
        tags,
    })
}

/// The tailnet device at `ip`, cached for ten minutes. A lookup that takes longer than a
/// second is left running to fill the cache, and this request is logged with the IP alone.
pub async fn device(ip: IpAddr) -> Option<Device> {
    if !is_tailnet(ip) {
        return None;
    }
    if let Some((at, device)) = DEVICES.lock().ok()?.get(&ip)
        && at.elapsed() < CACHE_FOR
    {
        return device.clone();
    }
    let lookup = tokio::spawn(async move {
        let device = whois(ip).await;
        if let Ok(mut cache) = DEVICES.lock() {
            cache.insert(ip, (Instant::now(), device.clone()));
        }
        device
    });
    tokio::time::timeout(LOOKUP_TIMEOUT, lookup)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
}

fn program_path(pid: i32) -> Option<String> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    (length > 0).then(|| String::from_utf8_lossy(&buffer[..length as usize]).into_owned())
}

fn parent_pid(pid: i32) -> Option<i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size).then_some(info.pbi_ppid as i32)
}

/// A socket client, from the peer's process ID.
pub fn process(pid: Option<i32>) -> Caller {
    Caller {
        pid,
        program: pid.and_then(program_path),
        parent_program: pid.and_then(parent_pid).and_then(program_path),
        ..Caller::default()
    }
}
