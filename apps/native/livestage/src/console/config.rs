//! The appliance's own settings, `/data/system/setup.conf`: shell variables
//! that the boot scripts source and this tool reads back. The first setup
//! writes it; until then every value is its default, which is a working
//! machine (DHCP on every wired port, the web UI open to the network).
//!
//! Hand-made overrides belong in `/data/livestage/livestage.conf`, which the
//! service reads after this file; this tool never touches that one.

use std::net::Ipv4Addr;

pub const DEFAULT_NAME: &str = "livestage";
pub const DEFAULT_TIMEZONE: &str = "UTC";
pub const DEFAULT_PORT: u16 = 8730;

/// Sample rates offered; `0` is the interface's own rate.
pub const RATES: [u32; 5] = [0, 44_100, 48_000, 88_200, 96_000];
/// Frames per callback offered.
pub const BUFFERS: [u32; 5] = [64, 128, 256, 512, 1024];
pub const DEFAULT_BUFFER: u32 = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupConfig {
    /// The host name, and `<name>.local` on the network.
    pub name: String,
    /// An IANA zone ("Asia/Bangkok").
    pub timezone: String,
    /// One network port; `None` is every wired port, by DHCP.
    pub interface: Option<String>,
    /// A fixed address on `interface`; `None` is DHCP.
    pub fixed: Option<FixedAddress>,
    /// The network to join when `interface` is a Wi-Fi port.
    pub wifi: Option<Wifi>,
    /// The web UI answers on every network, or on this machine only.
    pub web_open: bool,
    pub web_port: u16,
    /// The interface LiveStage starts with when there is no session yet.
    /// Once there is one, the session holds it (the web UI changes it there)
    /// and the setup edits the session instead.
    pub audio: AudioChoice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedAddress {
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wifi {
    pub ssid: String,
    /// The WPA key made from the password (64 hex digits, what
    /// `wpa_passphrase` prints); `None` is an open network. The password
    /// itself is never kept.
    pub psk: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioChoice {
    /// ALSA device names (`hw:CARD=USB,DEV=0`); `None` is the default device.
    pub output: Option<String>,
    pub input: Option<String>,
    /// `0` is the interface's own rate.
    pub rate: u32,
    pub buffer: u32,
}

impl Default for AudioChoice {
    fn default() -> Self {
        Self {
            output: None,
            input: None,
            rate: 0,
            buffer: DEFAULT_BUFFER,
        }
    }
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            name: DEFAULT_NAME.to_string(),
            timezone: DEFAULT_TIMEZONE.to_string(),
            interface: None,
            fixed: None,
            wifi: None,
            web_open: true,
            web_port: DEFAULT_PORT,
            audio: AudioChoice::default(),
        }
    }
}

impl SetupConfig {
    /// Reads the file's variables. Anything missing or not valid keeps its
    /// default, so a damaged file still boots to a reachable machine.
    pub fn parse(text: &str) -> Self {
        let mut config = Self::default();
        let mut mode_static = false;
        let (mut address, mut gateway, mut dns) = (None, None, Vec::new());
        let (mut ssid, mut psk) = (None, None);
        for (key, value) in variables(text) {
            match key.as_str() {
                "DEVICE_NAME" => {
                    if let Ok(name) = valid_name(&value) {
                        config.name = name;
                    }
                }
                "TIMEZONE" => {
                    if valid_zone(&value) {
                        config.timezone = value;
                    }
                }
                "NET_INTERFACE" => {
                    config.interface = valid_interface(&value).then_some(value);
                }
                "NET_MODE" => mode_static = value == "static",
                "NET_ADDRESS" => address = parse_cidr(&value).ok(),
                "NET_GATEWAY" => gateway = value.parse().ok(),
                "NET_DNS" => dns = parse_address_list(&value).unwrap_or_default(),
                "WIFI_SSID" => ssid = valid_ssid(&value).then_some(value),
                "WIFI_PSK" => psk = valid_psk(&value).then(|| value.to_ascii_lowercase()),
                "LIVESTAGE_HTTP" => {
                    if let Some((open, port)) = parse_http(&value) {
                        config.web_open = open;
                        config.web_port = port;
                    }
                }
                "AUDIO_OUTPUT" => config.audio.output = valid_device(&value).then_some(value),
                "AUDIO_INPUT" => config.audio.input = valid_device(&value).then_some(value),
                "AUDIO_RATE" => {
                    config.audio.rate = value
                        .parse()
                        .ok()
                        .filter(|r| RATES.contains(r))
                        .unwrap_or(0)
                }
                "AUDIO_BUFFER" => {
                    config.audio.buffer = value
                        .parse()
                        .ok()
                        .filter(|b| BUFFERS.contains(b))
                        .unwrap_or(DEFAULT_BUFFER)
                }
                _ => {}
            }
        }
        // Wi-Fi and a fixed address both need one port to put them on.
        if config.interface.is_some() {
            config.wifi = ssid.map(|ssid| Wifi { ssid, psk });
        }
        if mode_static && config.interface.is_some() {
            if let Some((address, prefix)) = address {
                config.fixed = Some(FixedAddress {
                    address,
                    prefix,
                    gateway,
                    dns,
                });
            }
        }
        config
    }

    pub fn to_file(&self) -> String {
        let fixed = self.fixed.as_ref();
        let mut out = String::from(
            "# LiveStage appliance settings, written by livestage-setup (the setup on\n\
             # the console, tty1). Read at boot by /etc/init.d/livestage-config and\n\
             # /etc/init.d/livestage. Hand-made LiveStage settings go in\n\
             # /data/livestage/livestage.conf instead, which wins over these.\n\n",
        );
        let mut put = |key: &str, value: &str| {
            out.push_str(key);
            out.push('=');
            out.push_str(&quote(value));
            out.push('\n');
        };
        put("DEVICE_NAME", &self.name);
        put("TIMEZONE", &self.timezone);
        put("NET_INTERFACE", self.interface.as_deref().unwrap_or(""));
        put("NET_MODE", if fixed.is_some() { "static" } else { "dhcp" });
        put(
            "NET_ADDRESS",
            &fixed
                .map(|f| format!("{}/{}", f.address, f.prefix))
                .unwrap_or_default(),
        );
        put(
            "NET_GATEWAY",
            &fixed
                .and_then(|f| f.gateway)
                .map(|g| g.to_string())
                .unwrap_or_default(),
        );
        put(
            "NET_DNS",
            &fixed.map(|f| join_addresses(&f.dns)).unwrap_or_default(),
        );
        let wifi = self.wifi.as_ref();
        put("WIFI_SSID", wifi.map(|w| w.ssid.as_str()).unwrap_or(""));
        put(
            "WIFI_PSK",
            wifi.and_then(|w| w.psk.as_deref()).unwrap_or(""),
        );
        put("LIVESTAGE_HTTP", &self.http());
        put("AUDIO_OUTPUT", self.audio.output.as_deref().unwrap_or(""));
        put("AUDIO_INPUT", self.audio.input.as_deref().unwrap_or(""));
        put("AUDIO_RATE", &self.audio.rate.to_string());
        put("AUDIO_BUFFER", &self.audio.buffer.to_string());
        out
    }

    /// The server's `--http` address.
    pub fn http(&self) -> String {
        let host = if self.web_open {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        };
        format!("{host}:{}", self.web_port)
    }
}

/// `KEY=value` and `KEY="value"` lines; comments and anything else skipped.
pub fn variables(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return None;
            }
            Some((key.to_string(), unquote(value)))
        })
        .collect()
}

/// Double quotes, with the four characters the shell still reads inside them
/// escaped. (The values are validated as well; this keeps the file right
/// whatever ends up in it.)
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        if c != '\n' {
            out.push(c);
        }
    }
    out.push('"');
    out
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return inner.to_string();
    }
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                if !matches!(next, '"' | '\\' | '$' | '`') {
                    out.push('\\');
                }
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A host name: one DNS label, lower case.
pub fn valid_name(value: &str) -> Result<String, &'static str> {
    let name = value.trim().to_ascii_lowercase();
    if name.is_empty() {
        return Err("The name cannot be empty.");
    }
    if name.len() > 63 {
        return Err("The name can be at most 63 characters.");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("Use only letters, digits and '-'.");
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("The name cannot start or end with '-'.");
    }
    Ok(name)
}

/// A zone name as a relative path under /usr/share/zoneinfo.
pub fn valid_zone(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
}

pub fn valid_interface(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 15
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A Wi-Fi network name: 1 to 32 bytes, no control characters.
pub fn valid_ssid(value: &str) -> bool {
    (1..=32).contains(&value.len()) && !value.chars().any(char::is_control)
}

/// A WPA password: 8 to 63 printable ASCII characters.
pub fn valid_passphrase(value: &str) -> bool {
    (8..=63).contains(&value.len()) && value.chars().all(|c| (' '..='~').contains(&c))
}

fn valid_psk(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// The WPA key of a password on a network: PBKDF2-HMAC-SHA1, 4096 rounds,
/// salted with the network name (IEEE 802.11i; what `wpa_passphrase` does).
pub fn wifi_psk(ssid: &str, passphrase: &str) -> String {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(passphrase.as_bytes(), ssid.as_bytes(), 4096, &mut key);
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// wpa_supplicant's settings for joining `wifi`. The name is written in
/// hex, so nothing in it needs escaping; `country` (two letters) sets the
/// channels the radio may use.
pub fn wpa_supplicant_file(wifi: &Wifi, country: Option<&str>) -> String {
    let mut out = String::from(
        "# Made by livestage-setup from /data/system/setup.conf.\n\
         ctrl_interface=/run/wpa_supplicant\nupdate_config=0\n",
    );
    if let Some(country) = country {
        out.push_str(&format!("country={country}\n"));
    }
    out.push_str(&format!(
        "\nnetwork={{\n\tssid={}\n\tscan_ssid=1\n",
        hex(wifi.ssid.as_bytes())
    ));
    match &wifi.psk {
        Some(psk) => out.push_str(&format!("\tpsk={psk}\n")),
        None => out.push_str("\tkey_mgmt=NONE\n"),
    }
    out.push_str("}\n");
    out
}

/// An ALSA PCM name as the sound cards offer them (`hw:CARD=USB,DEV=0`).
pub fn valid_device(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '=' | ',' | '_' | '-' | '.'))
}

/// `192.168.1.50/24`; a bare address is a /24.
pub fn parse_cidr(value: &str) -> Result<(Ipv4Addr, u8), &'static str> {
    let value = value.trim();
    let (address, prefix) = match value.split_once('/') {
        Some((address, prefix)) => (
            address,
            prefix
                .parse::<u8>()
                .ok()
                .filter(|p| (1..=32).contains(p))
                .ok_or("The prefix after '/' is 1 to 32 (24 is 255.255.255.0).")?,
        ),
        None => (value, 24),
    };
    let address: Ipv4Addr = address
        .parse()
        .map_err(|_| "Type the address as four numbers, like 192.168.1.50/24.")?;
    if address.is_unspecified() || address.is_broadcast() || address.is_multicast() {
        return Err("That address cannot belong to a machine.");
    }
    Ok((address, prefix))
}

/// Addresses separated by spaces or commas.
pub fn parse_address_list(value: &str) -> Result<Vec<Ipv4Addr>, &'static str> {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse()
                .map_err(|_| "Type DNS servers as addresses, like 1.1.1.1 8.8.8.8.")
        })
        .collect()
}

pub fn join_addresses(addresses: &[Ipv4Addr]) -> String {
    addresses
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `0.0.0.0:8730` is open to the network; a loopback address is not.
fn parse_http(value: &str) -> Option<(bool, u16)> {
    let addr: std::net::SocketAddr = value.parse().ok()?;
    Some((!addr.ip().is_loopback(), addr.port()))
}

/// The netmask of a prefix length, for showing next to it.
pub fn netmask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    };
    Ipv4Addr::from(bits)
}

// ── The files made from the settings at boot (on /run) ──────────────────────

/// `/etc/network/interfaces` for ifupdown-ng. `wired` is every wired port
/// found, for the default of DHCP on all of them; `wireless` says the chosen
/// port is Wi-Fi, which wpa_supplicant joins to its network first.
pub fn interfaces_file(config: &SetupConfig, wired: &[String], wireless: bool) -> String {
    let mut out = String::from(
        "# Made at boot by livestage-setup from /data/system/setup.conf.\n\
         # Change it with the setup on the console (tty1).\n\n\
         auto lo\niface lo inet loopback\n",
    );
    let Some(interface) = &config.interface else {
        let fallback = ["eth0".to_string()];
        let ports = if wired.is_empty() {
            &fallback[..]
        } else {
            wired
        };
        for port in ports {
            out.push_str(&format!("\nauto {port}\niface {port} inet dhcp\n"));
        }
        return out;
    };
    match &config.fixed {
        Some(fixed) => {
            out.push_str(&format!(
                "\nauto {interface}\niface {interface} inet static\n\taddress {}/{}\n",
                fixed.address, fixed.prefix
            ));
            if let Some(gateway) = fixed.gateway {
                out.push_str(&format!("\tgateway {gateway}\n"));
            }
        }
        None => out.push_str(&format!(
            "\nauto {interface}\niface {interface} inet dhcp\n"
        )),
    }
    if wireless && config.wifi.is_some() {
        let run = format!("/run/wpa_supplicant/{interface}");
        out.push_str(&format!(
            "\tpre-up rfkill unblock wlan 2>/dev/null || true\n\
             \tpre-up wpa_supplicant -B -q -i {interface} -c {run}.conf -P {run}.pid\n\
             \tpost-down kill \"$(cat {run}.pid 2>/dev/null)\" 2>/dev/null || true\n"
        ));
    }
    out
}

pub fn hosts_file(name: &str) -> String {
    format!("127.0.0.1\t{name} localhost\n::1\t\t{name} localhost\n")
}

/// `resolv.conf` for a fixed address; DHCP writes its own.
pub fn resolv_file(fixed: &FixedAddress) -> Option<String> {
    let servers = if fixed.dns.is_empty() {
        // No servers given: the gateway usually answers.
        fixed.gateway.into_iter().collect()
    } else {
        fixed.dns.clone()
    };
    if servers.is_empty() {
        return None;
    }
    let mut out = String::from("# Made by livestage-setup (fixed address).\n");
    for server in servers {
        out.push_str(&format!("nameserver {server}\n"));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_survive_a_round_trip() {
        let config = SetupConfig::default();
        assert_eq!(SetupConfig::parse(&config.to_file()), config);
        assert_eq!(SetupConfig::parse(""), config);
    }

    #[test]
    fn a_fixed_address_survives_a_round_trip() {
        let config = SetupConfig {
            name: "foh-rack".into(),
            timezone: "Asia/Bangkok".into(),
            interface: Some("eth1".into()),
            wifi: Some(Wifi {
                ssid: "Stage \"A\" $net".into(),
                psk: Some("ab".repeat(32)),
            }),
            fixed: Some(FixedAddress {
                address: Ipv4Addr::new(10, 0, 0, 20),
                prefix: 16,
                gateway: Some(Ipv4Addr::new(10, 0, 0, 1)),
                dns: vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)],
            }),
            web_open: false,
            web_port: 9000,
            audio: AudioChoice {
                output: Some("hw:CARD=USB,DEV=0".into()),
                input: Some("hw:CARD=USB,DEV=0".into()),
                rate: 48_000,
                buffer: 128,
            },
        };
        let file = config.to_file();
        assert!(file.contains("NET_ADDRESS=\"10.0.0.20/16\""));
        assert!(file.contains("LIVESTAGE_HTTP=\"127.0.0.1:9000\""));
        assert_eq!(SetupConfig::parse(&file), config);
    }

    #[test]
    fn bad_values_fall_back_to_the_defaults() {
        let config = SetupConfig::parse(
            "DEVICE_NAME=\"-bad\"\nTIMEZONE=\"../../etc/passwd\"\nNET_INTERFACE=\"eth0; reboot\"\n\
             NET_MODE=static\nNET_ADDRESS=300.1.1.1\nAUDIO_OUTPUT=\"$(reboot)\"\nAUDIO_RATE=12345\n",
        );
        assert_eq!(config, SetupConfig::default());
    }

    #[test]
    fn a_fixed_address_without_a_port_is_dhcp() {
        let config = SetupConfig::parse("NET_MODE=static\nNET_ADDRESS=10.0.0.5/24\n");
        assert_eq!(config.fixed, None);
    }

    #[test]
    fn quoting_round_trips_what_the_shell_would_expand() {
        assert_eq!(quote("a\"b$c`d\\e"), "\"a\\\"b\\$c\\`d\\\\e\"");
        assert_eq!(unquote(&quote("a\"b$c`d\\e")), "a\"b$c`d\\e");
        assert_eq!(unquote("'x y'"), "x y");
        assert_eq!(unquote("plain"), "plain");
    }

    #[test]
    fn the_wpa_key_matches_ieee_802_11i() {
        // The standard's test vectors (IEEE 802.11i, annex H.4).
        assert_eq!(
            wifi_psk("IEEE", "password"),
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"
        );
        assert_eq!(
            wifi_psk("ThisIsASSID", "ThisIsAPassword"),
            "0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af"
        );
        assert!(valid_passphrase("12345678"));
        assert!(!valid_passphrase("short"));
        assert!(!valid_passphrase(&"x".repeat(64)));
    }

    #[test]
    fn a_wifi_port_starts_wpa_supplicant_first() {
        let config = SetupConfig {
            interface: Some("wlan0".into()),
            wifi: Some(Wifi {
                ssid: "Front of house".into(),
                psk: Some(wifi_psk("Front of house", "loud and clear")),
            }),
            ..SetupConfig::default()
        };
        let file = interfaces_file(&config, &["eth0".into()], true);
        assert!(
            file.contains("auto wlan0\niface wlan0 inet dhcp\n"),
            "{file}"
        );
        assert!(file.contains(
            "\tpre-up wpa_supplicant -B -q -i wlan0 -c /run/wpa_supplicant/wlan0.conf -P /run/wpa_supplicant/wlan0.pid\n"
        ));
        assert!(!file.contains("eth0"));
        let wpa = wpa_supplicant_file(config.wifi.as_ref().unwrap(), Some("TH"));
        assert!(wpa.contains("country=TH\n"));
        assert!(wpa.contains("\tssid=46726f6e74206f6620686f757365\n"));
        assert!(wpa.contains(&format!(
            "\tpsk={}\n",
            config.wifi.as_ref().unwrap().psk.as_ref().unwrap()
        )));
        // An open network.
        let open = wpa_supplicant_file(
            &Wifi {
                ssid: "x".into(),
                psk: None,
            },
            None,
        );
        assert!(open.contains("key_mgmt=NONE") && !open.contains("country"));
        // The round trip through the file.
        assert_eq!(SetupConfig::parse(&config.to_file()), config);
    }

    #[test]
    fn names_are_one_lower_case_label() {
        assert_eq!(valid_name(" LiveStage-2 "), Ok("livestage-2".into()));
        assert!(valid_name("").is_err());
        assert!(valid_name("a.b").is_err());
        assert!(valid_name("end-").is_err());
        assert!(valid_name(&"x".repeat(64)).is_err());
    }

    #[test]
    fn addresses_parse_with_or_without_a_prefix() {
        assert_eq!(
            parse_cidr("192.168.1.50"),
            Ok((Ipv4Addr::new(192, 168, 1, 50), 24))
        );
        assert_eq!(
            parse_cidr("10.1.2.3/8"),
            Ok((Ipv4Addr::new(10, 1, 2, 3), 8))
        );
        assert!(parse_cidr("10.1.2.3/33").is_err());
        assert!(parse_cidr("0.0.0.0/24").is_err());
        assert!(parse_cidr("10.1.2").is_err());
        assert_eq!(
            parse_address_list("1.1.1.1, 8.8.8.8"),
            Ok(vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)])
        );
        assert!(parse_address_list("dns.example").is_err());
        assert_eq!(netmask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(netmask(20), Ipv4Addr::new(255, 255, 240, 0));
    }

    #[test]
    fn dhcp_on_every_wired_port_by_default() {
        let file = interfaces_file(
            &SetupConfig::default(),
            &["eth0".into(), "eth1".into()],
            false,
        );
        assert!(file.contains("auto eth0\niface eth0 inet dhcp\n"));
        assert!(file.contains("auto eth1\niface eth1 inet dhcp\n"));
        // No port seen yet (a slow driver): eth0 all the same.
        let file = interfaces_file(&SetupConfig::default(), &[], false);
        assert!(file.contains("iface eth0 inet dhcp"));
    }

    #[test]
    fn a_fixed_address_goes_on_its_port_only() {
        let config = SetupConfig {
            interface: Some("eth1".into()),
            fixed: Some(FixedAddress {
                address: Ipv4Addr::new(192, 168, 0, 9),
                prefix: 24,
                gateway: Some(Ipv4Addr::new(192, 168, 0, 1)),
                dns: vec![],
            }),
            ..SetupConfig::default()
        };
        let file = interfaces_file(&config, &["eth0".into(), "eth1".into()], false);
        assert!(!file.contains("eth0"));
        assert!(file.contains(
            "auto eth1\niface eth1 inet static\n\taddress 192.168.0.9/24\n\tgateway 192.168.0.1\n"
        ));
        // No DNS given: ask the gateway.
        assert_eq!(
            resolv_file(config.fixed.as_ref().unwrap()).unwrap(),
            "# Made by livestage-setup (fixed address).\nnameserver 192.168.0.1\n"
        );
    }
}
