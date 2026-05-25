use serde_json::Value;
use std::net::Ipv4Addr;
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS},
    NetworkManagement::{
        IpHelper::{
            GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
            GAA_FLAG_SKIP_MULTICAST, IF_TYPE_IEEE80211, IF_TYPE_TUNNEL, IP_ADAPTER_ADDRESSES_LH,
            MIB_IF_TYPE_ETHERNET, MIB_IF_TYPE_LOOPBACK, MIB_IF_TYPE_PPP, MIB_IF_TYPE_SLIP,
        },
        Ndis::IfOperStatusUp,
    },
    Networking::WinSock::{NldsPreferred, AF_INET, SOCKADDR_IN, SOCKET_ADDRESS},
};

pub(super) fn number_field(value: &Value, field: &str, fallback: f64) -> f64 {
    number_value(value, field).unwrap_or(fallback)
}

pub(super) fn number_value(value: &Value, field: &str) -> Option<f64> {
    value.get(field).and_then(Value::as_f64)
}

pub(super) fn bool_field(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_bool).unwrap_or(false)
}

pub(super) fn clamp(value: f64, min: f64, max: f64) -> f64 {
    if value.is_finite() {
        value.min(max).max(min)
    } else {
        min
    }
}

pub(super) fn query_param(query: &str, name: &str) -> Option<String> {
    for part in query.split('&') {
        let mut pair = part.splitn(2, '=');
        let key = pair.next().unwrap_or("");
        let value = pair.next().unwrap_or("");
        if key == name {
            return Some(percent_decode_query_value(value));
        }
    }
    None
}

pub(super) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        diff |= (left_byte ^ right_byte) as usize;
    }
    diff == 0
}

fn percent_decode_query_value(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte == b'+' {
            bytes.push(b' ');
            continue;
        }
        if byte == b'%' {
            let Some(high) = chars.next() else {
                bytes.push(byte);
                break;
            };
            let Some(low) = chars.next() else {
                bytes.push(byte);
                bytes.push(high);
                break;
            };
            if let (Some(high), Some(low)) = (hex_value(high), hex_value(low)) {
                bytes.push((high << 4) | low);
            } else {
                bytes.push(byte);
                bytes.push(high);
                bytes.push(low);
            }
            continue;
        }
        bytes.push(byte);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(super) fn remote_url(host: &str, port: u16, token: &str) -> String {
    let url_host = if host == "0.0.0.0" {
        lan_ip_address().unwrap_or_else(|| "127.0.0.1".to_string())
    } else {
        host.to_string()
    };
    format!("http://{}:{}/?token={}", url_host, port, token)
}

pub(super) fn remote_relay_url(
    relay_url: &str,
    token: &str,
    cloud_user_id: Option<&str>,
) -> Result<String, String> {
    let mut url = relay_url_with_path(relay_url, "/")?;
    let scheme = match url.scheme() {
        "http" => "http",
        "https" => "https",
        "ws" => "http",
        "wss" => "https",
        other => return Err(format!("unsupported remote relay URL scheme: {}", other)),
    };
    url.set_scheme(scheme)
        .map_err(|_| format!("failed to set remote relay scheme: {}", scheme))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("auth", "cloud")
            .append_pair("token", token);
        if let Some(user_id) = cloud_user_id.filter(|user_id| !user_id.trim().is_empty()) {
            query.append_pair("cloudUser", user_id.trim());
        }
    }
    Ok(url.to_string())
}

pub(super) fn relay_host_ws_url(
    relay_url: &str,
    token: &str,
    cloud: bool,
) -> Result<String, String> {
    let mut url = relay_url_with_path(relay_url, "/ws/host")?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        "ws" => "ws",
        "wss" => "wss",
        other => return Err(format!("unsupported remote relay URL scheme: {}", other)),
    };
    url.set_scheme(scheme)
        .map_err(|_| format!("failed to set remote relay scheme: {}", scheme))?;
    {
        let mut query = url.query_pairs_mut();
        if cloud {
            query.append_pair("auth", "cloud");
        }
        query.append_pair("token", token);
    }
    Ok(url.to_string())
}

fn relay_url_with_path(relay_url: &str, pathname: &str) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(relay_url).map_err(|e| e.to_string())?;
    let base_path = url.path().trim_end_matches('/');
    url.set_path(&format!("{}{}", base_path, pathname));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn lan_ip_address() -> Option<String> {
    preferred_lan_ip_from_windows()
        .or_else(preferred_lan_ip_from_interfaces)
        .or_else(preferred_lan_ip_from_socket)
        .or_else(|| Some("127.0.0.1".to_string()))
}

#[cfg(windows)]
fn preferred_lan_ip_from_windows() -> Option<String> {
    let mut buffer_len = 15_000u32;
    for _ in 0..3 {
        let mut buffer = vec![0u8; buffer_len as usize];
        let status = unsafe {
            GetAdaptersAddresses(
                AF_INET as u32,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                std::ptr::null(),
                buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut buffer_len,
            )
        };

        if status == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if status != ERROR_SUCCESS {
            return None;
        }

        let first_adapter = buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        return preferred_lan_ip_from_windows_adapters(first_adapter).map(|ip| ip.to_string());
    }

    None
}

#[cfg(not(windows))]
fn preferred_lan_ip_from_windows() -> Option<String> {
    None
}

#[cfg(windows)]
#[derive(Clone, Copy)]
struct WindowsLanCandidate {
    ip: Ipv4Addr,
    score: i32,
    metric: u32,
}

#[cfg(windows)]
fn preferred_lan_ip_from_windows_adapters(
    first_adapter: *mut IP_ADAPTER_ADDRESSES_LH,
) -> Option<Ipv4Addr> {
    let mut best: Option<WindowsLanCandidate> = None;
    let mut adapter_ptr = first_adapter;

    while !adapter_ptr.is_null() {
        let adapter = unsafe { &*adapter_ptr };
        let label = format!(
            "{} {}",
            unsafe { utf16_ptr_to_string(adapter.FriendlyName) },
            unsafe { utf16_ptr_to_string(adapter.Description) }
        );
        let score = windows_adapter_lan_score(&label, adapter.IfType, adapter.OperStatus);

        let mut unicast_ptr = adapter.FirstUnicastAddress;
        while !unicast_ptr.is_null() {
            let unicast = unsafe { &*unicast_ptr };
            if unicast.DadState == NldsPreferred {
                if let Some(ip) = socket_address_ipv4(&unicast.Address) {
                    if is_useful_lan_ipv4(ip) && is_rfc1918_ipv4(ip) && score >= 1500 {
                        let candidate = WindowsLanCandidate {
                            ip,
                            score,
                            metric: adapter.Ipv4Metric,
                        };
                        match best {
                            Some(current)
                                if current.score > candidate.score
                                    || (current.score == candidate.score
                                        && current.metric <= candidate.metric) => {}
                            _ => best = Some(candidate),
                        }
                    }
                }
            }

            unicast_ptr = unicast.Next;
        }

        adapter_ptr = adapter.Next;
    }

    best.map(|candidate| candidate.ip)
}

#[cfg(windows)]
fn windows_adapter_lan_score(label: &str, if_type: u32, oper_status: i32) -> i32 {
    let normalized = normalize_interface_name(label);
    let mut score = 0;

    if oper_status == IfOperStatusUp {
        score += 1000;
    }
    if is_windows_lan_if_type(if_type) {
        score += 500;
    }
    if is_preferred_lan_interface_name(&normalized) {
        score += 200;
    }
    if is_windows_virtual_if_type(if_type) || is_virtual_interface_name(&normalized) {
        score -= 800;
    }

    score
}

#[cfg(windows)]
fn is_windows_lan_if_type(if_type: u32) -> bool {
    if_type == MIB_IF_TYPE_ETHERNET || if_type == IF_TYPE_IEEE80211
}

#[cfg(windows)]
fn is_windows_virtual_if_type(if_type: u32) -> bool {
    if_type == MIB_IF_TYPE_LOOPBACK
        || if_type == IF_TYPE_TUNNEL
        || if_type == MIB_IF_TYPE_PPP
        || if_type == MIB_IF_TYPE_SLIP
}

#[cfg(windows)]
fn socket_address_ipv4(address: &SOCKET_ADDRESS) -> Option<Ipv4Addr> {
    if address.lpSockaddr.is_null()
        || address.iSockaddrLength < std::mem::size_of::<SOCKADDR_IN>() as i32
    {
        return None;
    }

    let sockaddr = unsafe { &*address.lpSockaddr };
    if sockaddr.sa_family != AF_INET {
        return None;
    }

    let sockaddr_in = unsafe { &*(address.lpSockaddr as *const SOCKADDR_IN) };
    let octets = unsafe { sockaddr_in.sin_addr.S_un.S_un_b };
    Some(Ipv4Addr::new(
        octets.s_b1,
        octets.s_b2,
        octets.s_b3,
        octets.s_b4,
    ))
}

#[cfg(windows)]
unsafe fn utf16_ptr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }

    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }

    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}

fn preferred_lan_ip_from_interfaces() -> Option<String> {
    let mut best: Option<(i32, String)> = None;
    let interfaces = get_if_addrs::get_if_addrs().ok()?;
    for interface in interfaces {
        let ip = match interface.addr {
            get_if_addrs::IfAddr::V4(addr) => addr.ip,
            get_if_addrs::IfAddr::V6(_) => continue,
        };
        if !is_useful_lan_ipv4(ip) {
            continue;
        }
        let score = interface_lan_score(&interface.name, ip);
        let candidate = ip.to_string();
        match &mut best {
            Some((best_score, best_ip)) if score > *best_score => {
                *best_score = score;
                *best_ip = candidate;
            }
            None => best = Some((score, candidate)),
            _ => {}
        }
    }
    best.map(|(_, ip)| ip)
}

fn preferred_lan_ip_from_socket() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    let ipv4 = match ip {
        std::net::IpAddr::V4(ipv4) => ipv4,
        std::net::IpAddr::V6(_) => return None,
    };
    if is_rfc1918_ipv4(ipv4) {
        Some(ipv4.to_string())
    } else {
        None
    }
}

fn is_useful_lan_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_loopback()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && !ip.is_unspecified()
        && !ip.is_link_local()
        && !is_benchmark_ipv4(ip)
}

fn interface_lan_score(name: &str, ip: Ipv4Addr) -> i32 {
    let normalized = normalize_interface_name(name);
    let mut score = 0;

    if is_rfc1918_ipv4(ip) {
        score += 100;
    }
    if is_preferred_lan_interface_name(&normalized) {
        score += 40;
    }
    if is_virtual_interface_name(&normalized) {
        score -= 40;
    }

    score
}

fn normalize_interface_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn is_preferred_lan_interface_name(name: &str) -> bool {
    name.contains("wlan")
        || name.contains("wi-fi")
        || name.contains("wifi")
        || name.contains("ethernet")
        || name.contains("以太网")
        || name.contains("无线")
        || name.contains("lan")
}

fn is_virtual_interface_name(name: &str) -> bool {
    name.contains("mihomo")
        || name.contains("wintun")
        || name.contains("tun")
        || name.contains("tap")
        || name.contains("vmware")
        || name.contains("virtual")
        || name.contains("virtualbox")
        || name.contains("hyper-v")
        || name.contains("tailscale")
        || name.contains("wireguard")
        || name.contains("zerotier")
        || name.contains("loopback")
        || name.starts_with("本地连接*")
        || name.contains('*')
}

fn is_rfc1918_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
}

fn is_benchmark_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 198 && (18..=19).contains(&b)
}

pub(super) fn make_token() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect::<String>()
}

pub(super) fn make_relay_connection_id() -> String {
    let token = make_token();
    format!("relay-host-{}", &token[..32])
}

pub(super) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub(super) fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;

    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b'\t' | b' ' => continue,
            _ => return None,
        } as u32;

        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xff) as u8);
        }
    }

    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_decodes_values() {
        assert_eq!(
            query_param("token=secret&name=office+mac%2Fone", "name").as_deref(),
            Some("office mac/one")
        );
    }

    #[test]
    fn token_generation_uses_256_bit_hex_tokens() {
        let first = make_token();
        let second = make_token();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn lan_ipv4_classification_filters_tun_benchmark_addresses() {
        assert!(is_rfc1918_ipv4(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(is_rfc1918_ipv4(Ipv4Addr::new(10, 0, 0, 8)));
        assert!(!is_rfc1918_ipv4(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(is_benchmark_ipv4(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(is_useful_lan_ipv4(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!is_useful_lan_ipv4(Ipv4Addr::new(198, 18, 0, 1)));
    }

    #[test]
    fn lan_interface_scoring_prefers_physical_lan_over_virtual_adapters() {
        let wlan = interface_lan_score("WLAN", Ipv4Addr::new(192, 168, 0, 8));
        let local = interface_lan_score("本地连接* 11", Ipv4Addr::new(192, 168, 137, 1));
        let tun = interface_lan_score("Mihomo", Ipv4Addr::new(198, 18, 0, 1));
        assert!(wlan > local);
        assert!(local > tun);
    }

    #[cfg(windows)]
    #[test]
    fn windows_adapter_scoring_prefers_physical_lan_over_virtual_adapters() {
        let wlan = windows_adapter_lan_score(
            "WLAN Intel(R) Wireless-AC 9462",
            IF_TYPE_IEEE80211,
            IfOperStatusUp,
        );
        let local = windows_adapter_lan_score(
            "本地连接* 11 Microsoft Wi-Fi Direct Virtual Adapter",
            IF_TYPE_IEEE80211,
            IfOperStatusUp,
        );
        let tun = windows_adapter_lan_score("Mihomo Wintun", IF_TYPE_TUNNEL, IfOperStatusUp);

        assert!(wlan >= 1500);
        assert!(local < 1500);
        assert!(tun < 1500);
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_lan_ip_selector_runs() {
        let _ = preferred_lan_ip_from_windows();
    }

    #[test]
    fn constant_time_comparison_checks_value_and_length() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
        assert!(!constant_time_eq(b"secret", b"secret-extra"));
    }

    #[test]
    fn cloud_relay_urls_mark_cloud_auth_without_embedding_access_tokens() {
        let public = remote_relay_url(
            "https://relay.example/base",
            "relay-host-abc123",
            Some("user-1"),
        )
        .expect("public url");
        assert_eq!(
            public,
            "https://relay.example/base/?auth=cloud&token=relay-host-abc123&cloudUser=user-1"
        );

        let host = relay_host_ws_url("https://relay.example/base", "session-token", true)
            .expect("host url");
        assert_eq!(
            host,
            "wss://relay.example/base/ws/host?auth=cloud&token=session-token"
        );
    }

    #[test]
    fn relay_connection_id_is_public_prefixed_token() {
        let first = make_relay_connection_id();
        let second = make_relay_connection_id();
        assert!(first.starts_with("relay-host-"));
        assert_eq!(first.len(), "relay-host-".len() + 32);
        assert_ne!(first, second);
    }
}
