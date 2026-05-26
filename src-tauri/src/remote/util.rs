use serde_json::Value;
use std::net::Ipv4Addr;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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
    let mut candidates = interface_lan_ip_candidates();
    if let Some(ip) = routed_lan_ip_candidate() {
        candidates.push(("route".to_string(), ip));
    }
    candidates
        .into_iter()
        .filter_map(|(name, ip)| lan_ip_candidate_score(&name, ip).map(|score| (score, ip)))
        .max_by_key(|(score, _)| *score)
        .map(|(_, ip)| ip.to_string())
}

fn routed_lan_ip_candidate() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        std::net::IpAddr::V6(_) => None,
    }
}

fn interface_lan_ip_candidates() -> Vec<(String, Ipv4Addr)> {
    if cfg!(windows) {
        windows_ipconfig_lan_ip_candidates()
    } else {
        unix_ifconfig_lan_ip_candidates()
    }
}

fn windows_ipconfig_lan_ip_candidates() -> Vec<(String, Ipv4Addr)> {
    let Ok(output) = Command::new("ipconfig").output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut adapter = String::new();
    let mut candidates = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !line.starts_with(' ') && !line.starts_with('\t') && trimmed.ends_with(':') {
            adapter = trimmed.trim_end_matches(':').to_string();
            continue;
        }
        if !trimmed.contains("IPv4") {
            continue;
        }
        let Some((_, value)) = trimmed.split_once(':') else {
            continue;
        };
        if let Some(ip) = parse_ipv4_token(value) {
            candidates.push((adapter.clone(), ip));
        }
    }
    candidates
}

fn unix_ifconfig_lan_ip_candidates() -> Vec<(String, Ipv4Addr)> {
    let Ok(output) = Command::new("ifconfig").output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut iface = String::new();
    let mut candidates = Vec::new();
    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') {
            iface = line.split(':').next().unwrap_or("").trim().to_string();
        }
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("inet ") else {
            continue;
        };
        if let Some(ip) = rest.split_whitespace().next().and_then(parse_ipv4_token) {
            candidates.push((iface.clone(), ip));
        }
    }
    candidates
}

fn parse_ipv4_token(value: &str) -> Option<Ipv4Addr> {
    let token = value.split_whitespace().next().unwrap_or("").trim();
    let end = token
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(token.len());
    token.get(..end)?.parse().ok()
}

fn lan_ip_candidate_score(interface_name: &str, ip: Ipv4Addr) -> Option<i32> {
    let octets = ip.octets();
    if ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || octets[0] >= 224
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
    {
        return None;
    }

    let mut score = if octets[0] == 192 && octets[1] == 168 {
        130
    } else if octets[0] == 172 && (16..=31).contains(&octets[1]) {
        120
    } else if octets[0] == 10 {
        110
    } else {
        10
    };

    let name = interface_name.to_ascii_lowercase();
    if name.contains("wi-fi")
        || name.contains("wifi")
        || name.contains("wireless")
        || name.starts_with("en")
        || name.starts_with("wl")
        || name.starts_with("eth")
    {
        score += 20;
    }
    if name.contains("virtual")
        || name.contains("vethernet")
        || name.contains("vmware")
        || name.contains("virtualbox")
        || name.contains("hyper-v")
        || name.contains("wsl")
        || name.contains("docker")
        || name.contains("bridge")
        || name.contains("tailscale")
        || name.contains("zerotier")
        || name.contains("clash")
        || name.contains("vpn")
        || name.contains("tun")
        || name.contains("tap")
        || name.starts_with("utun")
        || name.starts_with("lo")
    {
        score -= 80;
    }
    Some(score)
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
    fn constant_time_comparison_checks_value_and_length() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
        assert!(!constant_time_eq(b"secret", b"secret-extra"));
    }

    #[test]
    fn lan_ip_candidate_score_prefers_physical_lan_over_tun_and_virtual() {
        let tun = lan_ip_candidate_score("Clash TUN", Ipv4Addr::new(198, 18, 0, 1))
            .expect("tun candidate");
        let wifi = lan_ip_candidate_score("Wi-Fi", Ipv4Addr::new(192, 168, 0, 23))
            .expect("wifi candidate");
        let virtual_switch = lan_ip_candidate_score("vEthernet", Ipv4Addr::new(192, 168, 137, 1))
            .expect("virtual candidate");
        assert!(wifi > tun);
        assert!(wifi > virtual_switch);
    }

    #[test]
    fn parse_ipv4_token_ignores_windows_preferred_suffix() {
        assert_eq!(
            parse_ipv4_token("192.168.0.23(Preferred)"),
            Some(Ipv4Addr::new(192, 168, 0, 23))
        );
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
