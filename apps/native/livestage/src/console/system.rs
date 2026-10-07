//! What the console reads from and does to the machine: network ports, sound
//! cards, the services, the files made at boot on /run.
//!
//! Every path goes through [`System::path`], so `--root DIR` points the tool
//! at a copy of the tree; there it runs no commands (it says what it would
//! have run) and changes nothing outside that directory.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::config::{self, AudioChoice, SetupConfig};

pub const SETUP_CONF: &str = "/data/system/setup.conf";
const LIVESTAGE_CONF: &str = "/data/livestage/livestage.conf";
const DEFAULT_SESSION: &str = "/data/livestage/show.json";
pub const LOG: &str = "/var/log/livestage.log";
const ZONEINFO: &str = "/usr/share/zoneinfo";

pub struct System {
    root: PathBuf,
    live: bool,
}

/// A network port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetPort {
    pub name: String,
    pub mac: String,
    /// Has a cable with something at the other end; `None` while it is down.
    pub carrier: Option<bool>,
    /// IPv4 addresses with their prefix (`192.168.1.23/24`).
    pub addresses: Vec<String>,
    /// A Wi-Fi radio.
    pub wireless: bool,
}

/// A Wi-Fi network a scan found.
#[derive(Debug, Clone, PartialEq)]
pub struct WifiNetwork {
    pub ssid: String,
    /// dBm; the strongest of the network's access points.
    pub signal: f32,
    /// Needs a password (WPA/WEP); `false` is open.
    pub secure: bool,
    /// MHz, of the strongest access point.
    pub frequency: u32,
}

/// An ALSA card, from /proc/asound (readable while LiveStage holds it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundCard {
    pub id: String,
    pub name: String,
    /// The first playback / capture PCM device numbers.
    pub playback: Option<u32>,
    pub capture: Option<u32>,
}

impl SoundCard {
    pub fn output_device(&self) -> Option<String> {
        self.playback
            .map(|dev| format!("hw:CARD={},DEV={dev}", self.id))
    }

    pub fn input_device(&self) -> Option<String> {
        self.capture
            .map(|dev| format!("hw:CARD={},DEV={dev}", self.id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Running,
    Stopped,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordState {
    /// Anyone at the keyboard logs in as root.
    None,
    Set,
    Locked,
    Unknown,
}

impl System {
    /// The machine itself (`None`), or a copy of its tree for trying the
    /// console out.
    pub fn new(root: Option<PathBuf>) -> Self {
        match root {
            Some(root) => Self { root, live: false },
            None => Self {
                root: PathBuf::from("/"),
                live: true,
            },
        }
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }

    fn read(&self, absolute: &str) -> Option<String> {
        std::fs::read_to_string(self.path(absolute)).ok()
    }

    fn write(&self, absolute: &str, text: &str) -> Result<(), String> {
        let path = self.path(absolute);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Runs a command and returns what it printed. With `--root`, only says
    /// what it would have run.
    pub fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.run_with_input(program, args, None)
    }

    fn run_with_input(
        &self,
        program: &str,
        args: &[&str],
        input: Option<&str>,
    ) -> Result<String, String> {
        if !self.live {
            return Ok(format!("(not run: {program} {})", args.join(" ")));
        }
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| format!("{program}: {e}"))?;
        if let Some(input) = input
            && let Some(mut stdin) = child.stdin.take()
        {
            use std::io::Write;
            let _ = stdin.write_all(input.as_bytes());
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("{program}: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr
                .lines()
                .chain(stdout.lines())
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("failed")
                .to_string();
            Err(format!("{program}: {message}"))
        }
    }

    // ── The settings ────────────────────────────────────────────────────────

    /// `None` until the first setup has been saved.
    pub fn load_config(&self) -> Option<SetupConfig> {
        self.read(SETUP_CONF).map(|text| SetupConfig::parse(&text))
    }

    pub fn save_config(&self, config: &SetupConfig) -> Result<(), String> {
        // Next to it first, then over it: a power cut leaves the old file or
        // the new one, never half of one.
        let temporary = format!("{SETUP_CONF}.new");
        self.write(&temporary, &config.to_file())?;
        // It holds the Wi-Fi key: readable by root only.
        self.private(&temporary)?;
        std::fs::rename(self.path(&temporary), self.path(SETUP_CONF))
            .map_err(|e| format!("{SETUP_CONF}: {e}"))?;
        if self.live {
            let _ = self.run("sync", &[]);
        }
        Ok(())
    }

    fn private(&self, absolute: &str) -> Result<(), String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = self.path(absolute);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        #[cfg(not(unix))]
        let _ = absolute;
        Ok(())
    }

    /// Writes what the settings make on /run (the system is read-only; /etc
    /// links there) and sets the host name. Run at boot before the network
    /// starts, and again by the setup.
    pub fn write_runtime(&self, config: &SetupConfig) -> Result<(), String> {
        let wired: Vec<String> = self
            .ports()
            .into_iter()
            .filter(|port| self.is_wired(&port.name))
            .map(|port| port.name)
            .collect();
        self.write("/run/hostname", &format!("{}\n", config.name))?;
        self.write("/run/hosts", &config::hosts_file(&config.name))?;
        let wireless = config
            .interface
            .as_deref()
            .is_some_and(|name| self.is_wireless(name));
        if let (Some(interface), Some(wifi), true) = (&config.interface, &config.wifi, wireless) {
            let file = format!("/run/wpa_supplicant/{interface}.conf");
            let country = self.zone_country(&config.timezone);
            // Empty first, private, then the key goes in.
            self.write(&file, "")?;
            self.private(&file)?;
            self.write(
                &file,
                &config::wpa_supplicant_file(wifi, country.as_deref()),
            )?;
        }
        self.write(
            "/run/network/interfaces",
            &config::interfaces_file(config, &wired, wireless),
        )?;
        match config.fixed.as_ref().and_then(config::resolv_file) {
            Some(resolv) => self.write("/run/resolv.conf", &resolv)?,
            // Back to DHCP: drop ours, udhcpc writes its own.
            None => {
                if self
                    .read("/run/resolv.conf")
                    .is_some_and(|text| text.starts_with("# Made by livestage-setup"))
                {
                    let _ = std::fs::remove_file(self.path("/run/resolv.conf"));
                }
            }
        }
        self.link_timezone(&config.timezone)?;
        if self.live {
            std::fs::write("/proc/sys/kernel/hostname", &config.name)
                .map_err(|e| format!("host name: {e}"))?;
        }
        Ok(())
    }

    fn link_timezone(&self, zone: &str) -> Result<(), String> {
        let zone_file = format!("{ZONEINFO}/{zone}");
        let target = if config::valid_zone(zone) && self.path(&zone_file).is_file() {
            zone_file
        } else {
            format!("{ZONEINFO}/UTC")
        };
        let link = self.path("/run/localtime");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link)
                .map_err(|e| format!("{}: {e}", link.display()))?;
        }
        #[cfg(not(unix))]
        {
            // Trying the console out on Windows: a note instead of a link.
            std::fs::write(&link, format!("-> {target}\n"))
                .map_err(|e| format!("{}: {e}", link.display()))?;
        }
        Ok(())
    }

    // ── Network ─────────────────────────────────────────────────────────────

    /// Every network port but loopback, with its addresses.
    pub fn ports(&self) -> Vec<NetPort> {
        let mut ports: Vec<NetPort> = std::fs::read_dir(self.path("/sys/class/net"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "lo" {
                    return None;
                }
                let dir = entry.path();
                let text = |file: &str| {
                    std::fs::read_to_string(dir.join(file))
                        .ok()
                        .map(|t| t.trim().to_string())
                };
                // Ethernet-type only (wired and Wi-Fi): not monitor or
                // tunnel devices.
                if text("type").is_some_and(|t| t != "1") {
                    return None;
                }
                Some(NetPort {
                    mac: text("address").unwrap_or_default(),
                    carrier: text("carrier").map(|c| c == "1"),
                    addresses: Vec::new(),
                    wireless: dir.join("wireless").exists() || dir.join("phy80211").exists(),
                    name,
                })
            })
            .collect();
        ports.sort_by(|a, b| a.name.cmp(&b.name));
        let addresses = self.addresses();
        for port in &mut ports {
            port.addresses = addresses
                .iter()
                .filter(|(name, _)| *name == port.name)
                .map(|(_, address)| address.clone())
                .collect();
        }
        ports
    }

    /// A physical, wired port: a device behind it, Ethernet, not Wi-Fi.
    pub fn is_wired(&self, name: &str) -> bool {
        let dir = self.path(&format!("/sys/class/net/{name}"));
        dir.join("device").exists()
            && !dir.join("wireless").exists()
            && !dir.join("phy80211").exists()
            && std::fs::read_to_string(dir.join("type")).is_ok_and(|t| t.trim() == "1")
    }

    pub fn is_wireless(&self, name: &str) -> bool {
        let dir = self.path(&format!("/sys/class/net/{name}"));
        dir.join("wireless").exists() || dir.join("phy80211").exists()
    }

    /// The Wi-Fi networks in range of `port`, strongest first. Takes a few
    /// seconds; the port is brought up for it.
    pub fn scan_wifi(&self, port: &str) -> Result<Vec<WifiNetwork>, String> {
        if !config::valid_interface(port) {
            return Err(format!("{port}: not a network port"));
        }
        if !self.live {
            return Ok(Vec::new());
        }
        let _ = self.run("rfkill", &["unblock", "wlan"]);
        self.run("ip", &["link", "set", port, "up"])?;
        // A scan already running (wpa_supplicant's own) answers "busy":
        // wait for it, then read what it found.
        let mut last = String::new();
        for _ in 0..4 {
            match self.run("iw", &["dev", port, "scan"]) {
                Ok(text) => return Ok(parse_iw_scan(&text)),
                Err(error) => last = error,
            }
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        match self.run("iw", &["dev", port, "scan", "dump"]) {
            Ok(text) if !text.trim().is_empty() => Ok(parse_iw_scan(&text)),
            _ => Err(last),
        }
    }

    /// The network `port` is joined to and its signal (dBm).
    pub fn wifi_link(&self, port: &str) -> Option<(String, Option<f32>)> {
        if !self.live || !config::valid_interface(port) {
            return None;
        }
        parse_iw_link(&self.run("iw", &["dev", port, "link"]).ok()?)
    }

    /// The country of a time zone (tzdata's first one for it), which sets the
    /// channels the Wi-Fi radio may use.
    pub fn zone_country(&self, zone: &str) -> Option<String> {
        let text = self.read(&format!("{ZONEINFO}/zone1970.tab"))?;
        text.lines()
            .filter(|line| !line.starts_with('#'))
            .find(|line| line.split('\t').nth(2) == Some(zone))
            .and_then(|line| line.split('\t').next())
            .and_then(|codes| codes.split(',').next())
            .filter(|code| code.len() == 2 && code.chars().all(|c| c.is_ascii_uppercase()))
            .map(str::to_string)
    }

    /// `(port, address/prefix)` for every global IPv4 address.
    fn addresses(&self) -> Vec<(String, String)> {
        if !self.live {
            return Vec::new();
        }
        self.run("ip", &["-4", "-o", "addr", "show", "scope", "global"])
            .map(|text| parse_ip_addr(&text))
            .unwrap_or_default()
    }

    /// Takes the network down and up again with new settings.
    pub fn restart_network(&self, config: &SetupConfig) -> Result<(), String> {
        // Down with the old settings (ifupdown-ng needs them to undo them),
        // then up with the new ones.
        let _ = self.run("rc-service", &["networking", "stop"]);
        self.write_runtime(config)?;
        self.run("rc-service", &["networking", "start"])?;
        Ok(())
    }

    // ── Audio ───────────────────────────────────────────────────────────────

    pub fn sound_cards(&self) -> Vec<SoundCard> {
        let Some(text) = self.read("/proc/asound/cards") else {
            return Vec::new();
        };
        // Every card's PCM devices and which way they go (the per-card
        // pcmNp folders need a verbose-procfs kernel; this list does not).
        let pcms = self
            .read("/proc/asound/pcm")
            .map(|text| parse_asound_pcm(&text))
            .unwrap_or_default();
        parse_asound_cards(&text)
            .into_iter()
            .map(|(index, id, name)| {
                let first = |playback: bool| {
                    pcms.iter()
                        .filter(|pcm| pcm.card == index)
                        .filter(|pcm| if playback { pcm.playback } else { pcm.capture })
                        .map(|pcm| pcm.device)
                        .min()
                };
                SoundCard {
                    id,
                    name,
                    playback: first(true),
                    capture: first(false),
                }
            })
            .collect()
    }

    /// The session LiveStage loads, as the service would pick it.
    pub fn session_path(&self) -> String {
        self.read(LIVESTAGE_CONF)
            .and_then(|text| {
                config::variables(&text)
                    .into_iter()
                    .rev()
                    .find(|(key, _)| key == "LIVESTAGE_SESSION")
                    .map(|(_, value)| value)
            })
            .filter(|path| path.starts_with('/'))
            .unwrap_or_else(|| DEFAULT_SESSION.to_string())
    }

    /// The interface the session holds; `None` when there is no session yet.
    pub fn session_audio(&self) -> Option<AudioChoice> {
        let text = std::fs::read_to_string(self.path(&self.session_path())).ok()?;
        let session: serde_json::Value = serde_json::from_str(&text).ok()?;
        let audio = session.get("audio");
        let field = |key: &str| audio.and_then(|a| a.get(key));
        let text = |key: &str| field(key).and_then(|v| v.as_str()).map(str::to_string);
        let number = |key: &str| field(key).and_then(|v| v.as_u64()).map(|n| n as u32);
        Some(AudioChoice {
            output: text("output_device"),
            input: text("input_device"),
            rate: number("sample_rate").unwrap_or(0),
            buffer: number("buffer_frames").unwrap_or(config::DEFAULT_BUFFER),
        })
    }

    /// Moves the session out of the way (next to it, `.before-setup`), so
    /// LiveStage starts afresh: one channel per input of the interface.
    pub fn set_aside_session(&self) -> Result<(), String> {
        let path = self.path(&self.session_path());
        if !path.exists() {
            return Ok(());
        }
        let aside = path.with_extension("json.before-setup");
        std::fs::rename(&path, &aside).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Puts the interface into the session. LiveStage must be stopped: it
    /// writes the session when it stops.
    pub fn set_session_audio(&self, audio: &AudioChoice) -> Result<(), String> {
        let path = self.path(&self.session_path());
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut session: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let object = session
            .as_object_mut()
            .ok_or_else(|| format!("{}: not a session", path.display()))?;
        let audio_object = object
            .entry("audio")
            .or_insert_with(|| serde_json::json!({}));
        if !audio_object.is_object() {
            *audio_object = serde_json::json!({});
        }
        let fields = audio_object.as_object_mut().expect("made an object above");
        fields.insert("output_device".into(), audio.output.clone().into());
        fields.insert("input_device".into(), audio.input.clone().into());
        fields.insert("sample_rate".into(), audio.rate.into());
        fields.insert("buffer_frames".into(), audio.buffer.into());
        let text = serde_json::to_string_pretty(&session).map_err(|e| e.to_string())?;
        // Written in place, so the file stays the service account's.
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    // ── Time ────────────────────────────────────────────────────────────────

    /// IANA zones, from tzdata's own list; UTC first.
    pub fn zones(&self) -> Vec<String> {
        let mut zones: Vec<String> = self
            .read(&format!("{ZONEINFO}/zone1970.tab"))
            .map(|text| parse_zone_tab(&text))
            .unwrap_or_default();
        zones.sort();
        zones.dedup();
        zones.insert(0, "UTC".to_string());
        zones
    }

    pub fn clock(&self) -> String {
        if !self.live {
            return "--:--".to_string();
        }
        self.run("date", &["+%a %e %b  %H:%M"])
            .map(|t| t.trim().to_string())
            .unwrap_or_default()
    }

    // ── Status ──────────────────────────────────────────────────────────────

    pub fn hostname(&self) -> String {
        self.read("/proc/sys/kernel/hostname")
            .or_else(|| self.read("/run/hostname"))
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| config::DEFAULT_NAME.to_string())
    }

    pub fn service(&self, name: &str) -> ServiceState {
        if !self.live {
            return ServiceState::Unknown;
        }
        match Command::new("rc-service")
            .args([name, "status"])
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stdout);
                if text.contains("crashed") {
                    ServiceState::Failed
                } else if output.status.success() {
                    ServiceState::Running
                } else {
                    ServiceState::Stopped
                }
            }
            Err(_) => ServiceState::Unknown,
        }
    }

    /// The server's line about the interface it opened, the latest one.
    pub fn mixer_summary(&self) -> Option<String> {
        let log = self.read(LOG)?;
        log.lines()
            .rev()
            .find_map(|line| {
                let line = line.strip_prefix("[livestage] ")?;
                (line.contains(" Hz, ") || line.starts_with("no device")).then_some(line)
            })
            .map(str::to_string)
    }

    pub fn log_tail(&self, lines: usize) -> Vec<String> {
        let Some(log) = self.read(LOG) else {
            return vec![format!("({LOG} is empty or missing)")];
        };
        let all: Vec<&str> = log.lines().collect();
        all[all.len().saturating_sub(lines)..]
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    /// Free and total bytes on /data.
    pub fn data_space(&self) -> Option<(u64, u64)> {
        #[cfg(target_os = "linux")]
        {
            let path =
                std::ffi::CString::new(self.path("/data").to_string_lossy().as_bytes()).ok()?;
            let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
            // SAFETY: a NUL-terminated path and a zeroed out-struct.
            if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
                return None;
            }
            let block = stats.f_frsize as u64;
            Some((stats.f_bavail as u64 * block, stats.f_blocks as u64 * block))
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    pub fn password(&self) -> PasswordState {
        let Some(shadow) = self.read("/etc/shadow") else {
            return PasswordState::Unknown;
        };
        match shadow
            .lines()
            .find_map(|line| line.strip_prefix("root:"))
            .map(|rest| rest.split(':').next().unwrap_or(""))
        {
            Some("") => PasswordState::None,
            Some(hash) if hash.starts_with('!') || hash.starts_with('*') => PasswordState::Locked,
            Some(_) => PasswordState::Set,
            None => PasswordState::Unknown,
        }
    }

    /// The console's root password. The system is read-only, so it is
    /// opened for writing just for this.
    pub fn set_root_password(&self, password: &str) -> Result<(), String> {
        self.run("mount", &["-o", "remount,rw", "/"])?;
        let result = self.run_with_input(
            "chpasswd",
            &["-c", "sha512"],
            Some(&format!("root:{password}\n")),
        );
        let _ = self.run("sync", &[]);
        let remount = self.run("mount", &["-o", "remount,ro", "/"]);
        result?;
        remount.map(|_| ())
    }
}

/// `ip -4 -o addr` lines: `2: eth0    inet 10.0.2.15/24 brd … scope global eth0`.
fn parse_ip_addr(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let _index = words.next()?;
            let port = words.next()?.trim_end_matches(':');
            (words.next()? == "inet").then_some(())?;
            let address = words.next()?;
            Some((port.to_string(), address.to_string()))
        })
        .collect()
}

/// `/proc/asound/cards`: ` 0 [PCH            ]: HDA-Intel - HDA Intel PCH`.
fn parse_asound_cards(text: &str) -> Vec<(u32, String, String)> {
    text.lines()
        .filter_map(|line| {
            let (index, rest) = line.trim_start().split_once(' ')?;
            let index: u32 = index.parse().ok()?;
            let rest = rest.trim_start().strip_prefix('[')?;
            let (id, rest) = rest.split_once(']')?;
            let name = rest
                .split_once(" - ")
                .map(|(_, name)| name)
                .unwrap_or(rest)
                .trim();
            Some((index, id.trim().to_string(), name.to_string()))
        })
        .collect()
}

struct Pcm {
    card: u32,
    device: u32,
    playback: bool,
    capture: bool,
}

/// `/proc/asound/pcm`: `00-00: ALC662 Analog : ALC662 Analog : playback 1 : capture 1`.
fn parse_asound_pcm(text: &str) -> Vec<Pcm> {
    text.lines()
        .filter_map(|line| {
            let (address, rest) = line.split_once(':')?;
            let (card, device) = address.trim().split_once('-')?;
            Some(Pcm {
                card: card.parse().ok()?,
                device: device.parse().ok()?,
                playback: rest.contains(": playback "),
                capture: rest.contains(": capture "),
            })
        })
        .collect()
}

/// `iw dev X scan`: a `BSS` line per access point, then indented fields.
fn parse_iw_scan(text: &str) -> Vec<WifiNetwork> {
    let mut found: Vec<WifiNetwork> = Vec::new();
    let mut current: Option<WifiNetwork> = None;
    let keep = |network: Option<WifiNetwork>, found: &mut Vec<WifiNetwork>| {
        let Some(network) = network else { return };
        if network.ssid.is_empty() {
            return;
        }
        match found.iter_mut().find(|n| n.ssid == network.ssid) {
            Some(known) if known.signal >= network.signal => {}
            Some(known) => *known = network,
            None => found.push(network),
        }
    };
    for line in text.lines() {
        if line.starts_with("BSS ") {
            keep(current.take(), &mut found);
            current = Some(WifiNetwork {
                ssid: String::new(),
                signal: -100.0,
                secure: false,
                frequency: 0,
            });
            continue;
        }
        let Some(network) = current.as_mut() else {
            continue;
        };
        let field = line.trim_start();
        if let Some(value) = field.strip_prefix("SSID: ") {
            network.ssid = unescape_iw(value);
        } else if field == "SSID:" {
            network.ssid.clear();
        } else if let Some(value) = field.strip_prefix("signal: ") {
            network.signal = value
                .trim_end_matches(" dBm")
                .trim()
                .parse()
                .unwrap_or(-100.0);
        } else if let Some(value) = field.strip_prefix("freq: ") {
            network.frequency = value.trim().parse::<f32>().map(|f| f as u32).unwrap_or(0);
        } else if field.starts_with("capability:") && field.contains("Privacy")
            || field.starts_with("RSN:")
            || field.starts_with("WPA:")
        {
            network.secure = true;
        }
    }
    keep(current.take(), &mut found);
    found.sort_by(|a, b| b.signal.total_cmp(&a.signal));
    found
}

/// `iw dev X link`: `Connected to …` with `SSID:` and `signal:` under it, or
/// `Not connected.`
fn parse_iw_link(text: &str) -> Option<(String, Option<f32>)> {
    if !text.starts_with("Connected") {
        return None;
    }
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_start().strip_prefix(name).map(str::trim))
    };
    let ssid = unescape_iw(field("SSID:")?);
    let signal = field("signal:").and_then(|s| s.trim_end_matches("dBm").trim().parse().ok());
    Some((ssid, signal))
}

/// iw writes bytes it will not print as `\xNN`.
fn unescape_iw(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let raw = value.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\\'
            && raw.get(i + 1) == Some(&b'x')
            && let Some(byte) = value
                .get(i + 2..i + 4)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            bytes.push(byte);
            i += 4;
            continue;
        }
        bytes.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// tzdata's `zone1970.tab`: code(s), coordinates, zone, comment.
fn parse_zone_tab(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split('\t').nth(2))
        .filter(|zone| config::valid_zone(zone))
        .map(str::to_string)
        .collect()
}

pub fn megabytes(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

/// The address of a port without its prefix.
pub fn bare_address(address: &str) -> &str {
    address.split('/').next().unwrap_or(address)
}

pub fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let mask = u32::from(config::netmask(prefix));
    u32::from(a) & mask == u32::from(b) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_addr_lines_give_port_and_address() {
        let text = "2: eth0    inet 10.0.2.15/24 brd 10.0.2.255 scope global eth0\\       valid_lft forever preferred_lft forever\n\
                    3: eth1    inet 192.168.1.9/24 scope global eth1\n";
        assert_eq!(
            parse_ip_addr(text),
            vec![
                ("eth0".to_string(), "10.0.2.15/24".to_string()),
                ("eth1".to_string(), "192.168.1.9/24".to_string()),
            ]
        );
    }

    #[test]
    fn asound_cards_give_id_and_name() {
        let text = " 0 [Intel          ]: HDA-Intel - HDA Intel\n                      HDA Intel at 0xfebf0000 irq 32\n\
                    1 [USB            ]: USB-Audio - Scarlett 2i2 USB\n                      Focusrite Scarlett 2i2 USB at usb-0000:00:14.0-1\n";
        assert_eq!(
            parse_asound_cards(text),
            vec![
                (0, "Intel".to_string(), "HDA Intel".to_string()),
                (1, "USB".to_string(), "Scarlett 2i2 USB".to_string()),
            ]
        );
    }

    #[test]
    fn asound_pcm_gives_directions() {
        let text = "00-00: Generic Analog : Generic Analog : playback 1 : capture 1\n\
                    00-03: HDMI 0 : HDMI 0 : playback 1\n\
                    01-01: USB Audio #1 : USB Audio : capture 1\n";
        let pcms = parse_asound_pcm(text);
        assert_eq!(pcms.len(), 3);
        assert!(pcms[0].playback && pcms[0].capture);
        assert!(pcms[1].playback && !pcms[1].capture && pcms[1].device == 3);
        assert!(pcms[2].card == 1 && pcms[2].device == 1 && !pcms[2].playback && pcms[2].capture);
    }

    #[test]
    fn iw_scans_give_networks_strongest_first() {
        let text = "BSS 02:00:00:00:01:00(on wlan0)\n\
                    \tlast seen: 120 ms ago\n\
                    \tfreq: 2412.0\n\
                    \tsignal: -61.00 dBm\n\
                    \tSSID: Stage\\x20Left\n\
                    \tcapability: ESS Privacy ShortSlotTime (0x0411)\n\
                    \tRSN:\t * Version: 1\n\
                    BSS 02:00:00:00:02:00(on wlan0)\n\
                    \tfreq: 5180\n\
                    \tsignal: -40.00 dBm\n\
                    \tSSID: Guest\n\
                    \tcapability: ESS ShortSlotTime (0x0401)\n\
                    BSS 02:00:00:00:03:00(on wlan0)\n\
                    \tsignal: -50.00 dBm\n\
                    \tSSID: Stage Left\n\
                    \tcapability: ESS Privacy (0x0011)\n\
                    BSS 02:00:00:00:04:00(on wlan0)\n\
                    \tsignal: -30.00 dBm\n\
                    \tSSID: \n";
        let networks = parse_iw_scan(text);
        assert_eq!(
            networks
                .iter()
                .map(|n| (n.ssid.as_str(), n.signal, n.secure))
                .collect::<Vec<_>>(),
            [("Guest", -40.0, false), ("Stage Left", -50.0, true)]
        );
        assert_eq!(networks[0].frequency, 5180);
    }

    #[test]
    fn iw_link_gives_network_and_signal() {
        let text = "Connected to 02:00:00:00:01:00 (on wlan0)\n\tSSID: FOH\n\tfreq: 2412.0\n\tsignal: -42 dBm\n";
        assert_eq!(parse_iw_link(text), Some(("FOH".to_string(), Some(-42.0))));
        assert_eq!(parse_iw_link("Not connected.\n"), None);
    }

    #[test]
    fn zone_tab_lists_zones() {
        let text = "# comment\nTH,KH,LA,VN\t+1345+10031\tAsia/Bangkok\tIndochina (most areas)\nGB,GG,IM,JE\t+513030-0000731\tEurope/London\n";
        assert_eq!(parse_zone_tab(text), vec!["Asia/Bangkok", "Europe/London"]);
    }

    #[test]
    fn a_tree_gives_ports_cards_and_the_session() {
        let root =
            std::env::temp_dir().join(format!("livestage-setup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let system = System::new(Some(root.clone()));
        let put = |path: &str, text: &str| {
            let path = system.path(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        put("/sys/class/net/eth0/address", "52:54:00:12:34:56\n");
        put("/sys/class/net/eth0/carrier", "1\n");
        put("/sys/class/net/eth0/type", "1\n");
        put("/sys/class/net/eth0/device/uevent", "");
        put("/sys/class/net/wlan0/type", "1\n");
        put("/sys/class/net/wlan0/device/uevent", "");
        put("/sys/class/net/wlan0/wireless/x", "");
        put(
            "/usr/share/zoneinfo/zone1970.tab",
            "TH,KH,LA,VN\t+1345+10031\tAsia/Bangkok\tIndochina\n",
        );
        put("/sys/class/net/lo/type", "772\n");
        put("/sys/class/net/hwsim0/type", "803\n");
        put(
            "/proc/asound/cards",
            " 0 [Intel          ]: HDA-Intel - HDA Intel\n",
        );
        put(
            "/proc/asound/pcm",
            "00-00: Generic Analog : Generic Analog : playback 1 : capture 1\n00-03: HDMI 0 : HDMI 0 : playback 1\n",
        );
        put(
            "/data/livestage/show.json",
            r#"{"name":"Show","audio":{"host":null,"output_device":"default","sample_rate":0,"buffer_frames":256},"channels":[]}"#,
        );

        let ports = system.ports();
        assert_eq!(
            ports.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["eth0", "wlan0"]
        );
        assert_eq!(ports[0].carrier, Some(true));
        assert!(system.is_wired("eth0"));
        assert!(!system.is_wired("wlan0"));

        let cards = system.sound_cards();
        assert_eq!(
            cards[0].output_device().as_deref(),
            Some("hw:CARD=Intel,DEV=0")
        );
        assert_eq!(
            cards[0].input_device().as_deref(),
            Some("hw:CARD=Intel,DEV=0")
        );

        let audio = AudioChoice {
            output: Some("hw:CARD=Intel,DEV=0".into()),
            input: None,
            rate: 48_000,
            buffer: 128,
        };
        system.set_session_audio(&audio).unwrap();
        assert_eq!(system.session_audio(), Some(audio));
        // The rest of the session is untouched.
        let text = std::fs::read_to_string(system.path("/data/livestage/show.json")).unwrap();
        assert!(text.contains("\"name\": \"Show\""));
        assert!(text.contains("\"host\": null"));

        let config = SetupConfig {
            name: "foh".into(),
            ..SetupConfig::default()
        };
        system.save_config(&config).unwrap();
        assert_eq!(system.load_config(), Some(config.clone()));
        system.write_runtime(&config).unwrap();
        let interfaces = std::fs::read_to_string(system.path("/run/network/interfaces")).unwrap();
        assert!(interfaces.contains("iface eth0 inet dhcp"));
        assert!(!interfaces.contains("wlan0"));
        assert_eq!(
            std::fs::read_to_string(system.path("/run/hostname")).unwrap(),
            "foh\n"
        );

        // Wi-Fi: wpa_supplicant joins first, with the country of the zone.
        assert_eq!(system.zone_country("Asia/Bangkok").as_deref(), Some("TH"));
        let config = SetupConfig {
            interface: Some("wlan0".into()),
            timezone: "Asia/Bangkok".into(),
            wifi: Some(config::Wifi {
                ssid: "FOH".into(),
                psk: Some(config::wifi_psk("FOH", "loud and clear")),
            }),
            ..SetupConfig::default()
        };
        system.write_runtime(&config).unwrap();
        let interfaces = std::fs::read_to_string(system.path("/run/network/interfaces")).unwrap();
        assert!(
            interfaces.contains("pre-up wpa_supplicant -B -q -i wlan0"),
            "{interfaces}"
        );
        let wpa = std::fs::read_to_string(system.path("/run/wpa_supplicant/wlan0.conf")).unwrap();
        assert!(
            wpa.contains("country=TH") && wpa.contains("ssid=464f48"),
            "{wpa}"
        );
        assert!(system.ports()[1].wireless);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_session_keys_match_the_engine() {
        let audio = livestage_engine::AudioSettings {
            host: None,
            input_device: Some("in".into()),
            output_device: Some("out".into()),
            sample_rate: 48_000,
            buffer_frames: 128,
        };
        let value = serde_json::to_value(&audio).unwrap();
        for key in [
            "input_device",
            "output_device",
            "sample_rate",
            "buffer_frames",
        ] {
            assert!(value.get(key).is_some(), "{key}");
        }
    }
}
