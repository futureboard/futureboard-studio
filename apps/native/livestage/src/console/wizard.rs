//! The setup's pages: the machine's name, the network (Wi-Fi too), the
//! audio interface, where recordings go, the web UI, the time zone and the
//! console password, then a review. The console runs it on the machine
//! itself (`ui.rs` applies the answers); the installer runs it for the disk
//! it installs onto ([`Wizard::for_install`]), and writes the answers there.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::config::{self, AudioChoice, FixedAddress, SetupConfig, Wifi};
use super::drives::{drive_header, drive_rows, fallback_reason, recordings_summary, volume_name};
use super::storage_api::{self as api, State as StorageState};
use super::system::{self, NetPort, PasswordState, SoundCard, System, WifiNetwork};
use super::widgets::{
    ACCENT, BAD, DIM, FOCUS, LABEL_WIDTH, TextInput, button_row, cable_span, centered,
    wrapped_height,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Welcome,
    Name,
    Network,
    Audio,
    Storage,
    Web,
    Time,
    Password,
    Review,
}

const PAGES: [Page; 9] = [
    Page::Welcome,
    Page::Name,
    Page::Network,
    Page::Audio,
    Page::Storage,
    Page::Web,
    Page::Time,
    Page::Password,
    Page::Review,
];

/// The pages for a disk being installed: no welcome (the installer has its
/// own) and no drives (recordings start on the internal storage; the drives
/// are this computer's, chosen on the machine itself later).
const INSTALL_PAGES: [Page; 7] = [
    Page::Name,
    Page::Network,
    Page::Audio,
    Page::Web,
    Page::Time,
    Page::Password,
    Page::Review,
];

impl Page {
    pub fn title(self) -> &'static str {
        match self {
            Page::Welcome => "Welcome",
            Page::Name => "Name",
            Page::Network => "Network",
            Page::Audio => "Audio interface",
            Page::Storage => "Storage",
            Page::Web => "Web UI",
            Page::Time => "Time zone",
            Page::Password => "Console password",
            Page::Review => "Review",
        }
    }

    fn short(self) -> &'static str {
        match self {
            Page::Welcome => "Welcome",
            Page::Name => "Name",
            Page::Network => "Network",
            Page::Audio => "Audio",
            Page::Storage => "Storage",
            Page::Web => "Web UI",
            Page::Time => "Time",
            Page::Password => "Password",
            Page::Review => "Review",
        }
    }

    fn intro(self) -> &'static str {
        match self {
            Page::Welcome => {
                "This machine runs LiveStage: a mixer you control from a browser on any \
                 phone, tablet or computer on the same network.\n\n\
                 A few questions set it up: its name, the network, the audio interface, \
                 where recordings go, who can open the web UI, the time zone and a \
                 console password. Every \
                 one has a sensible default. You can come back here any time from this \
                 screen."
            }
            Page::Name => {
                "The machine's name on the network. Browsers reach it as \
                 http://NAME.local:PORT/ (most phones and computers understand .local)."
            }
            Page::Network => {
                "Leave it on DHCP when the network gives out addresses (a router does). \
                 On a desk with no router, or to always find the machine at the same \
                 address, give it a fixed one."
            }
            Page::Audio => {
                "The interface LiveStage plays and records with. It can be changed later \
                 from the web UI's Setup page too."
            }
            Page::Storage => {
                "Recordings go to the internal storage, or to a USB drive plugged into \
                 this machine. What is done on the drives page happens straight away, \
                 not at Apply."
            }
            Page::Web => {
                "The web UI has no login: anyone who can reach it controls the mixer. \
                 Keep the machine on the show's own network, or close the web UI to this \
                 machine only."
            }
            Page::Time => "Recordings are named and dated with this machine's clock.",
            Page::Password => {
                "The text console (Alt+F2) logs in as root. Without a password, anyone at \
                 the keyboard can change this machine. Leave both empty to keep it as it is."
            }
            Page::Review => "Nothing has changed yet. Apply to save and use these settings.",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldId {
    Start,
    Defaults,
    Name,
    Interface,
    Mode,
    Address,
    Gateway,
    Dns,
    Ssid,
    Scan,
    WifiPassword,
    Output,
    Storage,
    Input,
    Rate,
    Buffer,
    Access,
    WebPort,
    Zone,
    Password,
    Again,
    Back,
    Next,
    Apply,
}

#[derive(PartialEq, Eq)]
enum Kind {
    Text,
    Choice,
    Button,
    /// A button among the fields (rather than in the row at the bottom).
    Action,
}

impl FieldId {
    fn kind(self) -> Kind {
        match self {
            Self::Name
            | Self::Address
            | Self::Gateway
            | Self::Dns
            | Self::Ssid
            | Self::WifiPassword
            | Self::WebPort
            | Self::Password
            | Self::Again => Kind::Text,
            Self::Scan | Self::Storage => Kind::Action,
            Self::Interface
            | Self::Mode
            | Self::Output
            | Self::Input
            | Self::Rate
            | Self::Buffer
            | Self::Access
            | Self::Zone => Kind::Choice,
            Self::Start | Self::Defaults | Self::Back | Self::Next | Self::Apply => Kind::Button,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Start => "Set up",
            Self::Defaults => "Use the defaults",
            Self::Name => "Name",
            Self::Interface => "Port",
            Self::Mode => "Address",
            Self::Address => "IP address",
            Self::Gateway => "Gateway",
            Self::Dns => "DNS servers",
            Self::Ssid => "Wi-Fi",
            Self::Scan => "",
            Self::WifiPassword => "Wi-Fi key",
            Self::Output => "Output",
            Self::Storage => "Recordings",
            Self::Input => "Input",
            Self::Rate => "Sample rate",
            Self::Buffer => "Buffer",
            Self::Access => "Open to",
            Self::WebPort => "Port",
            Self::Zone => "Time zone",
            Self::Password => "Password",
            Self::Again => "Again",
            Self::Back => "Back",
            Self::Next => "Next",
            Self::Apply => "Apply",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Address => "like 192.168.1.50/24",
            Self::Gateway => "the router; empty for none",
            Self::Dns => "empty: ask the gateway",
            Self::Ssid => "type it, or scan",
            Self::WebPort => "1024 to 65535",
            Self::Password => "at least 6 characters",
            _ => "",
        }
    }
}

/// One entry of a choice: what it shows, and its value as text ("" for
/// none / automatic).
struct Entry {
    label: String,
    key: String,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Scan {
    Idle,
    /// Asked for: runs at the next tick, after "Scanning..." is drawn.
    Pending,
    Found(usize),
    Failed(String),
}

pub enum WizardOutcome {
    Stay,
    Leave,
    Pick(Picker),
    /// The review's Apply (or the welcome's Use the defaults): the answers
    /// are [`Wizard::answers`].
    Apply {
        defaults: bool,
    },
    /// The drives page, over the setup.
    Storage,
}

pub struct Wizard {
    /// The machine's first setup: it starts at the welcome.
    pub first: bool,
    /// For a disk being installed ([`Wizard::for_install`]), not for this
    /// machine.
    pub install: bool,
    page: usize,
    focus: usize,
    error: Option<String>,
    pub original: SetupConfig,
    pub original_audio: AudioChoice,
    pub session_exists: bool,
    password_state: PasswordState,
    pub ports: Vec<NetPort>,
    cards: Vec<SoundCard>,
    zones: Vec<String>,
    /// The drives, as last read.
    pub storage: StorageState,

    name: TextInput,
    interface: Option<String>,
    fixed: bool,
    address: TextInput,
    gateway: TextInput,
    dns: TextInput,
    ssid: TextInput,
    wifi_password: TextInput,
    networks: Vec<WifiNetwork>,
    pub scan: Scan,
    output: Option<String>,
    input: Option<String>,
    rate: u32,
    buffer: u32,
    web_open: bool,
    web_port: TextInput,
    zone: String,
    password: TextInput,
    again: TextInput,
}

impl Wizard {
    pub fn new(system: &System, first: bool, storage: StorageState) -> Self {
        let original = system.load_config().unwrap_or_default();
        let session_audio = system.session_audio();
        let session_exists = session_audio.is_some();
        let original_audio = session_audio.unwrap_or_else(|| original.audio.clone());
        let fixed = original.fixed.clone();
        let ports = system.ports();
        // A fresh machine with one port in use: offer its address as the
        // start of a fixed one.
        let current = ports
            .iter()
            .flat_map(|p| p.addresses.first())
            .next()
            .cloned();
        Self {
            first,
            install: false,
            page: if first { 0 } else { 1 },
            focus: 0,
            error: None,
            session_exists,
            password_state: system.password(),
            cards: system.sound_cards(),
            zones: system.zones(),
            storage,
            name: TextInput::new(original.name.clone()),
            interface: original.interface.clone(),
            fixed: fixed.is_some(),
            address: TextInput::new(
                fixed
                    .as_ref()
                    .map(|f| format!("{}/{}", f.address, f.prefix))
                    .or(current)
                    .unwrap_or_default(),
            ),
            gateway: TextInput::new(
                fixed
                    .as_ref()
                    .and_then(|f| f.gateway)
                    .map(|g| g.to_string())
                    .unwrap_or_default(),
            ),
            dns: TextInput::new(
                fixed
                    .as_ref()
                    .map(|f| config::join_addresses(&f.dns))
                    .unwrap_or_default(),
            ),
            ssid: TextInput::new(
                original
                    .wifi
                    .as_ref()
                    .map(|w| w.ssid.clone())
                    .unwrap_or_default(),
            ),
            wifi_password: TextInput::secret(),
            networks: Vec::new(),
            scan: Scan::Idle,
            output: original_audio.output.clone(),
            input: original_audio.input.clone(),
            rate: original_audio.rate,
            buffer: original_audio.buffer,
            web_open: original.web_open,
            web_port: TextInput::new(original.web_port.to_string()),
            zone: original.timezone.clone(),
            password: TextInput::secret(),
            again: TextInput::secret(),
            ports,
            original_audio,
            original,
        }
    }

    /// The setup for the disk the installer puts LiveStage on: every answer
    /// starts at its default (nothing of the installer's own system carries
    /// over), the ports and sound cards are this computer's, and nothing is
    /// applied here; the installer writes [`Wizard::answers`] onto the disk.
    #[allow(
        dead_code,
        reason = "livestage-installer's; the console never installs"
    )]
    pub fn for_install(system: &System) -> Self {
        let mut wizard = Self::new(system, true, StorageState::default());
        let original = SetupConfig::default();
        wizard.install = true;
        wizard.page = 0;
        wizard.session_exists = false;
        wizard.password_state = PasswordState::None;
        wizard.name = TextInput::new(original.name.clone());
        wizard.interface = None;
        wizard.fixed = false;
        wizard.address = TextInput::default();
        wizard.gateway = TextInput::default();
        wizard.dns = TextInput::default();
        wizard.ssid = TextInput::default();
        wizard.output = None;
        wizard.input = None;
        wizard.rate = original.audio.rate;
        wizard.buffer = original.audio.buffer;
        wizard.web_open = original.web_open;
        wizard.web_port = TextInput::new(original.web_port.to_string());
        wizard.zone = original.timezone.clone();
        wizard.original_audio = original.audio.clone();
        wizard.original = original;
        wizard
    }

    fn pages(&self) -> &'static [Page] {
        if self.install { &INSTALL_PAGES } else { &PAGES }
    }

    /// The page Esc leaves the setup from.
    fn first_page(&self) -> usize {
        if self.install || self.first { 0 } else { 1 }
    }

    pub fn page(&self) -> Page {
        self.pages()[self.page]
    }

    /// What the page says first.
    fn intro(&self) -> &'static str {
        match self.page() {
            Page::Audio if self.install => {
                "The interface LiveStage plays and records with, from the sound cards the \
                 installer finds on this computer. Leave it on Automatic to choose it later \
                 from the web UI's Setup page."
            }
            Page::Password if self.install => {
                "The text console (Alt+F2) logs in as root. Without a password, anyone at \
                 the keyboard can change this machine. Leave both empty for none (it can be \
                 set later from the console's setup)."
            }
            Page::Review if self.install => {
                "Nothing is written yet. These settings go onto the disk with LiveStage; \
                 its first boot uses them instead of asking."
            }
            page => page.intro(),
        }
    }

    /// A button's text.
    fn button_label(&self, id: FieldId) -> &'static str {
        match id {
            FieldId::Apply if self.install => "Use these",
            _ => id.label(),
        }
    }

    /// The settings and the new console password (if one was typed), as
    /// the review's Apply leaves them; with `defaults`, the settings as they
    /// were and no password.
    pub fn answers(&self, defaults: bool) -> (SetupConfig, Option<String>) {
        if defaults {
            return (self.original.clone(), None);
        }
        let password = (!self.password.value.is_empty()).then(|| self.password.value.clone());
        (self.config(), password)
    }

    fn fields(&self) -> Vec<FieldId> {
        use FieldId::*;
        match self.page() {
            Page::Welcome => vec![Start, Defaults],
            Page::Name => vec![Name, Back, Next],
            Page::Network => {
                let mut fields = vec![Interface];
                if self.wireless() {
                    fields.extend([Ssid, Scan, WifiPassword]);
                }
                fields.push(Mode);
                if self.fixed {
                    fields.extend([Address, Gateway, Dns]);
                }
                fields.extend([Back, Next]);
                fields
            }
            Page::Audio => vec![Output, Input, Rate, Buffer, Back, Next],
            Page::Storage => vec![Storage, Back, Next],
            Page::Web => vec![Access, WebPort, Back, Next],
            Page::Time => vec![Zone, Back, Next],
            Page::Password => vec![Password, Again, Back, Next],
            Page::Review => vec![Back, Apply],
        }
    }

    fn focused(&self) -> FieldId {
        let fields = self.fields();
        fields[self.focus.min(fields.len() - 1)]
    }

    fn focus_field(&mut self, id: FieldId) {
        if let Some(index) = self.fields().iter().position(|f| *f == id) {
            self.focus = index;
        }
    }

    fn text(&mut self, id: FieldId) -> Option<&mut TextInput> {
        Some(match id {
            FieldId::Name => &mut self.name,
            FieldId::Address => &mut self.address,
            FieldId::Gateway => &mut self.gateway,
            FieldId::Dns => &mut self.dns,
            FieldId::Ssid => &mut self.ssid,
            FieldId::WifiPassword => &mut self.wifi_password,
            FieldId::WebPort => &mut self.web_port,
            FieldId::Password => &mut self.password,
            FieldId::Again => &mut self.again,
            _ => return None,
        })
    }

    fn text_ref(&self, id: FieldId) -> Option<&TextInput> {
        Some(match id {
            FieldId::Name => &self.name,
            FieldId::Address => &self.address,
            FieldId::Gateway => &self.gateway,
            FieldId::Dns => &self.dns,
            FieldId::Ssid => &self.ssid,
            FieldId::WifiPassword => &self.wifi_password,
            FieldId::WebPort => &self.web_port,
            FieldId::Password => &self.password,
            FieldId::Again => &self.again,
            _ => return None,
        })
    }

    // ── Choices ─────────────────────────────────────────────────────────────

    fn entries(&self, id: FieldId) -> Vec<Entry> {
        let entry = |label: String, key: String| Entry { label, key };
        let mut entries = Vec::new();
        match id {
            FieldId::Interface => {
                entries.push(entry("Every wired port".into(), String::new()));
                for port in &self.ports {
                    let cable = match port.carrier {
                        _ if port.wireless => "Wi-Fi",
                        Some(true) => "cable in",
                        Some(false) => "no cable",
                        None => "down",
                    };
                    entries.push(entry(
                        format!("{}  {}  {}", port.name, port.mac, cable),
                        port.name.clone(),
                    ));
                }
                if let Some(current) = &self.interface
                    && !self.ports.iter().any(|p| p.name == *current)
                {
                    entries.push(entry(format!("{current}  (not found)"), current.clone()));
                }
            }
            FieldId::Mode => {
                entries.push(entry("Automatic (DHCP)".into(), "dhcp".into()));
                entries.push(entry("Fixed".into(), "fixed".into()));
            }
            FieldId::Output | FieldId::Input => {
                let output = id == FieldId::Output;
                entries.push(entry(
                    "Automatic (the system's default)".into(),
                    String::new(),
                ));
                for card in &self.cards {
                    let device = if output {
                        card.output_device()
                    } else {
                        card.input_device()
                    };
                    if let Some(device) = device {
                        entries.push(entry(format!("{}  ({device})", card.name), device));
                    }
                }
                let current = if output { &self.output } else { &self.input };
                if let Some(current) = current
                    && !entries.iter().any(|e| e.key == *current)
                {
                    entries.push(entry(
                        format!("{current}  (not connected)"),
                        current.clone(),
                    ));
                }
            }
            FieldId::Rate => {
                for rate in config::RATES {
                    let label = if rate == 0 {
                        "The interface's own rate".to_string()
                    } else {
                        format!("{} kHz", rate as f32 / 1000.0)
                    };
                    entries.push(entry(label, rate.to_string()));
                }
            }
            FieldId::Buffer => {
                for frames in config::BUFFERS {
                    entries.push(entry(
                        format!("{frames} frames ({:.1} ms at 48 kHz)", frames as f32 / 48.0),
                        frames.to_string(),
                    ));
                }
            }
            FieldId::Access => {
                entries.push(entry("Every network".into(), "open".into()));
                entries.push(entry("This machine only".into(), "local".into()));
            }
            FieldId::Ssid => {
                for network in &self.networks {
                    let band = if network.frequency >= 5000 {
                        "5 GHz"
                    } else {
                        "2.4 GHz"
                    };
                    entries.push(entry(
                        format!(
                            "{:<32}  {:>4.0} dBm  {:<7}  {}",
                            network.ssid,
                            network.signal,
                            band,
                            if network.secure { "password" } else { "open" }
                        ),
                        network.ssid.clone(),
                    ));
                }
            }
            FieldId::Zone => {
                for zone in &self.zones {
                    entries.push(entry(zone.replace('_', " "), zone.clone()));
                }
                if !self.zones.contains(&self.zone) {
                    entries.push(entry(self.zone.clone(), self.zone.clone()));
                }
            }
            _ => {}
        }
        entries
    }

    fn choice_key(&self, id: FieldId) -> String {
        match id {
            FieldId::Interface => self.interface.clone().unwrap_or_default(),
            FieldId::Mode => if self.fixed { "fixed" } else { "dhcp" }.into(),
            FieldId::Output => self.output.clone().unwrap_or_default(),
            FieldId::Input => self.input.clone().unwrap_or_default(),
            FieldId::Rate => self.rate.to_string(),
            FieldId::Buffer => self.buffer.to_string(),
            FieldId::Access => if self.web_open { "open" } else { "local" }.into(),
            FieldId::Zone => self.zone.clone(),
            _ => String::new(),
        }
    }

    pub fn set_choice(&mut self, id: FieldId, key: &str) {
        let some = |key: &str| (!key.is_empty()).then(|| key.to_string());
        match id {
            FieldId::Interface => self.interface = some(key),
            FieldId::Mode => self.fixed = key == "fixed",
            FieldId::Output => {
                // The same card both ways is the usual case: follow along
                // while the input has not been picked apart from it.
                if self.input == self.output {
                    let card = self
                        .cards
                        .iter()
                        .find(|c| c.output_device().as_deref() == Some(key));
                    self.input = match card {
                        Some(card) => card.input_device(),
                        None if key.is_empty() => None,
                        None => self.input.clone(),
                    };
                }
                self.output = some(key);
            }
            FieldId::Input => self.input = some(key),
            FieldId::Rate => self.rate = key.parse().unwrap_or(0),
            FieldId::Buffer => self.buffer = key.parse().unwrap_or(config::DEFAULT_BUFFER),
            FieldId::Access => self.web_open = key == "open",
            FieldId::Zone => self.zone = key.to_string(),
            FieldId::Ssid => {
                // Picked from a scan: on to its password.
                if self.ssid.value != key {
                    self.wifi_password = TextInput::secret();
                }
                self.ssid = TextInput::new(key);
                self.focus_field(FieldId::WifiPassword);
            }
            _ => {}
        }
        self.error = None;
    }

    fn step_choice(&mut self, id: FieldId, delta: isize) {
        let entries = self.entries(id);
        if entries.is_empty() {
            return;
        }
        let current = self.choice_key(id);
        let index = entries.iter().position(|e| e.key == current).unwrap_or(0) as isize;
        let next = (index + delta).rem_euclid(entries.len() as isize) as usize;
        let key = entries[next].key.clone();
        self.set_choice(id, &key);
    }

    fn choice_label(&self, id: FieldId) -> String {
        let current = self.choice_key(id);
        self.entries(id)
            .into_iter()
            .find(|e| e.key == current)
            .map(|e| e.label)
            .unwrap_or(current)
    }

    // ── Keys ────────────────────────────────────────────────────────────────

    pub fn key(&mut self, key: KeyEvent) -> WizardOutcome {
        let fields = self.fields();
        let id = self.focused();
        let kind = id.kind();

        // Typing goes to a text field first.
        if kind == Kind::Text
            && !matches!(
                key.code,
                KeyCode::Enter
                    | KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::Esc
                    | KeyCode::Up
                    | KeyCode::Down
            )
        {
            if let Some(text) = self.text(id)
                && text.key(key)
            {
                self.error = None;
            }
            return WizardOutcome::Stay;
        }

        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.focus = (self.focus + fields.len() - 1) % fields.len();
            }
            KeyCode::Down | KeyCode::Tab => self.focus = (self.focus + 1) % fields.len(),
            KeyCode::Left if kind == Kind::Choice => self.step_choice(id, -1),
            KeyCode::Right if kind == Kind::Choice => self.step_choice(id, 1),
            KeyCode::Left => self.focus = self.focus.saturating_sub(1),
            KeyCode::Right => self.focus = (self.focus + 1).min(fields.len() - 1),
            KeyCode::Char(' ') if kind == Kind::Choice => self.step_choice(id, 1),
            KeyCode::Esc => return self.back(),
            KeyCode::Enter => match kind {
                Kind::Text if id == FieldId::Ssid && !self.networks.is_empty() => {
                    let entries = self.entries(id);
                    return WizardOutcome::Pick(Picker::new(
                        id,
                        "Wi-Fi networks",
                        entries,
                        &self.ssid.value,
                    ));
                }
                Kind::Text => {
                    // On to the next field; from the last one, to Next.
                    self.focus = (self.focus + 1).min(fields.len() - 1);
                }
                Kind::Choice => {
                    let entries = self.entries(id);
                    let current = self.choice_key(id);
                    return WizardOutcome::Pick(Picker::new(id, id.label(), entries, &current));
                }
                Kind::Button => return self.press(id),
                Kind::Action if id == FieldId::Storage => return WizardOutcome::Storage,
                Kind::Action => {
                    if id == FieldId::Scan {
                        self.scan = Scan::Pending;
                        self.error = None;
                    }
                }
            },
            _ => {}
        }
        WizardOutcome::Stay
    }

    fn press(&mut self, id: FieldId) -> WizardOutcome {
        match id {
            FieldId::Start => self.go(1),
            FieldId::Defaults => {
                // Everything as it is: the defaults, saved, so this screen
                // does not come back at the next boot.
                return WizardOutcome::Apply { defaults: true };
            }
            FieldId::Back => return self.back(),
            FieldId::Next => {
                if let Err((message, field)) = self.validate(self.page()) {
                    self.error = Some(message);
                    if let Some(field) = field {
                        self.focus_field(field);
                    }
                } else {
                    self.go(self.page + 1);
                }
            }
            FieldId::Apply => return WizardOutcome::Apply { defaults: false },
            _ => {}
        }
        WizardOutcome::Stay
    }

    fn back(&mut self) -> WizardOutcome {
        if self.page <= self.first_page() {
            return WizardOutcome::Leave;
        }
        self.go(self.page - 1);
        WizardOutcome::Stay
    }

    fn go(&mut self, page: usize) {
        self.page = page.min(self.pages().len() - 1);
        self.error = None;
        // Land on the first field, or on Apply in the review.
        self.focus = if self.page() == Page::Review { 1 } else { 0 };
    }

    fn validate(&self, page: Page) -> Result<(), (String, Option<FieldId>)> {
        let fail = |message: &str, field: FieldId| Err((message.to_string(), Some(field)));
        match page {
            Page::Name => {
                if let Err(message) = config::valid_name(&self.name.value) {
                    return fail(message, FieldId::Name);
                }
            }
            Page::Network if self.wireless() && !config::valid_ssid(&self.ssid.value) => {
                return fail(
                    "Type the Wi-Fi network's name, or scan for it.",
                    FieldId::Ssid,
                );
            }
            Page::Network
                if self.wireless()
                    && !self.wifi_password.value.is_empty()
                    && !config::valid_passphrase(&self.wifi_password.value) =>
            {
                return fail(
                    "A Wi-Fi password is 8 to 63 characters.",
                    FieldId::WifiPassword,
                );
            }
            Page::Network
                if self.wireless()
                    && self.wifi_password.value.is_empty()
                    && self.saved_key().is_none()
                    && self
                        .networks
                        .iter()
                        .any(|n| n.ssid == self.ssid.value && n.secure) =>
            {
                return fail("This network needs its password.", FieldId::WifiPassword);
            }
            Page::Network if self.fixed => {
                if self.interface.is_none() {
                    return fail(
                        "Pick the one port that gets the fixed address.",
                        FieldId::Interface,
                    );
                }
                let (address, prefix) = match config::parse_cidr(&self.address.value) {
                    Ok(parsed) => parsed,
                    Err(message) => return fail(message, FieldId::Address),
                };
                if !self.gateway.value.trim().is_empty() {
                    let Ok(gateway) = self.gateway.value.trim().parse() else {
                        return fail(
                            "Type the gateway as an address, like 192.168.1.1.",
                            FieldId::Gateway,
                        );
                    };
                    if !system::same_subnet(address, gateway, prefix) {
                        return fail(
                            "The gateway is not on the same network as the address (check the /prefix).",
                            FieldId::Gateway,
                        );
                    }
                }
                if let Err(message) = config::parse_address_list(&self.dns.value) {
                    return fail(message, FieldId::Dns);
                }
            }
            Page::Web => match self.web_port.value.trim().parse::<u16>() {
                Ok(port) if port >= 1024 => {}
                _ => return fail("The port is a number from 1024 to 65535.", FieldId::WebPort),
            },
            Page::Password => {
                if self.password.value != self.again.value {
                    return fail("The two passwords are not the same.", FieldId::Again);
                }
                let length = self.password.value.chars().count();
                if length > 0 && length < 6 {
                    return fail("Use at least 6 characters.", FieldId::Password);
                }
                if self.password.value.contains(':') {
                    return fail("The password cannot contain ':'.", FieldId::Password);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The chosen port is a Wi-Fi radio.
    fn wireless(&self) -> bool {
        self.interface
            .as_ref()
            .is_some_and(|name| self.ports.iter().any(|p| p.name == *name && p.wireless))
    }

    /// The key saved for the network now typed, kept when no password is.
    fn saved_key(&self) -> Option<&String> {
        self.original
            .wifi
            .as_ref()
            .filter(|w| w.ssid == self.ssid.value)
            .and_then(|w| w.psk.as_ref())
    }

    /// Runs the scan the Scan button asked for; the list to pick from when
    /// it found anything.
    pub fn run_scan(&mut self, system: &System) -> Option<Picker> {
        let port = self.interface.clone().unwrap_or_default();
        match system.scan_wifi(&port) {
            Ok(networks) => {
                self.scan = Scan::Found(networks.len());
                self.networks = networks;
                if self.networks.is_empty() {
                    return None;
                }
                let entries = self.entries(FieldId::Ssid);
                Some(Picker::new(
                    FieldId::Ssid,
                    "Wi-Fi networks",
                    entries,
                    &self.ssid.value,
                ))
            }
            Err(error) => {
                self.scan = Scan::Failed(error);
                None
            }
        }
    }

    /// The settings as drafted (validated page by page before the review).
    /// Making the Wi-Fi key takes a moment: `with_key` false leaves a
    /// placeholder, for showing.
    pub fn config(&self) -> SetupConfig {
        self.config_with(true)
    }

    fn config_with(&self, with_key: bool) -> SetupConfig {
        let wifi = self.wireless().then(|| Wifi {
            ssid: self.ssid.value.clone(),
            psk: if !self.wifi_password.value.is_empty() {
                Some(if with_key {
                    config::wifi_psk(&self.ssid.value, &self.wifi_password.value)
                } else {
                    String::new()
                })
            } else {
                self.saved_key().cloned()
            },
        });
        let fixed = if self.fixed {
            config::parse_cidr(&self.address.value)
                .ok()
                .map(|(address, prefix)| FixedAddress {
                    address,
                    prefix,
                    gateway: self.gateway.value.trim().parse().ok(),
                    dns: config::parse_address_list(&self.dns.value).unwrap_or_default(),
                })
        } else {
            None
        };
        SetupConfig {
            name: config::valid_name(&self.name.value)
                .unwrap_or_else(|_| self.original.name.clone()),
            timezone: self.zone.clone(),
            interface: self.interface.clone(),
            fixed,
            wifi,
            web_open: self.web_open,
            web_port: self
                .web_port
                .value
                .trim()
                .parse()
                .unwrap_or(config::DEFAULT_PORT),
            audio: self.audio(),
            // The drives page saves its choice itself, straight away.
            record_storage: self.original.record_storage.clone(),
            record_storage_label: self.original.record_storage_label.clone(),
        }
    }

    fn audio(&self) -> AudioChoice {
        AudioChoice {
            output: self.output.clone(),
            input: self.input.clone(),
            rate: self.rate,
            buffer: self.buffer,
        }
    }

    // ── Drawing ─────────────────────────────────────────────────────────────

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered().border_style(DIM);
        let outer = area.inner(Margin::new(1, 0));
        let inner = block.inner(outer).inner(Margin::new(1, 0));
        frame.render_widget(block, outer);

        // The steps, the current one lit; closer together when they would
        // not fit.
        let pages = self.pages();
        let skip = self.first_page();
        let wide = pages[skip..].iter().map(|p| p.short().len()).sum::<usize>()
            + 3 * (pages.len() - skip - 1);
        let separator = if wide <= inner.width as usize {
            " > "
        } else {
            "  "
        };
        let mut steps = Vec::new();
        for (index, page) in pages.iter().enumerate() {
            if index < skip {
                continue;
            }
            if !steps.is_empty() {
                steps.push(Span::styled(separator, Style::new().fg(DIM)));
            }
            let style = if index == self.page {
                Style::new().fg(ACCENT).bold()
            } else if index < self.page {
                Style::new()
            } else {
                Style::new().fg(DIM)
            };
            steps.push(Span::styled(page.short(), style));
        }

        let intro = self.intro();
        let intro_height = wrapped_height(intro, inner.width);
        let fields = self.fields();
        let rows: Vec<FieldId> = fields
            .iter()
            .copied()
            .filter(|f| f.kind() != Kind::Button)
            .collect();
        let [
            steps_area,
            _,
            intro_area,
            _,
            fields_area,
            extra_area,
            error_area,
            buttons_area,
        ] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(intro_height),
            Constraint::Length(1),
            Constraint::Length(rows.len() as u16),
            Constraint::Min(0),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(inner);
        frame.render_widget(Paragraph::new(Line::from(steps)), steps_area);
        frame.render_widget(Paragraph::new(intro).wrap(Wrap { trim: true }), intro_area);

        let focused = self.focused();
        for (row, id) in rows.iter().enumerate() {
            let line_area = Rect {
                y: fields_area.y + row as u16,
                height: 1,
                ..fields_area
            };
            self.draw_field(frame, line_area, *id, *id == focused);
        }

        self.draw_extra(frame, extra_area);

        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .fg(BAD)
                    .wrap(Wrap { trim: true }),
                error_area,
            );
        }

        let buttons: Vec<FieldId> = fields
            .iter()
            .copied()
            .filter(|f| f.kind() == Kind::Button)
            .collect();
        let labels: Vec<&str> = buttons.iter().map(|b| self.button_label(*b)).collect();
        let focus = buttons.iter().position(|b| *b == focused);
        frame.render_widget(button_row(&labels, focus), buttons_area);
    }

    fn draw_field(&self, frame: &mut Frame, area: Rect, id: FieldId, focused: bool) {
        let label_style = if focused {
            Style::new().fg(FOCUS).bold()
        } else {
            Style::new().fg(ACCENT)
        };
        let mut spans = vec![Span::styled(
            format!("{:<width$}", id.label(), width = LABEL_WIDTH as usize),
            label_style,
        )];
        let value_x = area.x + LABEL_WIDTH;
        match id.kind() {
            Kind::Text => {
                let text = self.text_ref(id).expect("a text field");
                let shown = text.shown();
                let width = 32usize.max(shown.chars().count() + 1);
                let style = if focused {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new().add_modifier(Modifier::UNDERLINED)
                };
                spans.push(Span::raw("  "));
                spans.push(Span::styled(format!("{shown:<width$}"), style));
                let hint = match id {
                    FieldId::WifiPassword if self.saved_key().is_some() => "empty: keep saved",
                    FieldId::WifiPassword => "empty: open network",
                    _ => id.hint(),
                };
                if !hint.is_empty() {
                    spans.push(Span::styled(format!("  {hint}"), Style::new().fg(DIM)));
                }
                if focused {
                    let x = value_x + 2 + text.cursor as u16;
                    if x < area.right() {
                        frame.set_cursor_position((x, area.y));
                    }
                }
            }
            Kind::Choice => {
                let label = self.choice_label(id);
                if focused {
                    spans.push(Span::styled("< ", Style::new().fg(FOCUS)));
                    spans.push(Span::styled(
                        label,
                        Style::new().add_modifier(Modifier::REVERSED),
                    ));
                    spans.push(Span::styled(" >", Style::new().fg(FOCUS)));
                } else {
                    spans.push(Span::raw("  "));
                    spans.push(Span::raw(label));
                }
            }
            Kind::Action if id == FieldId::Storage => {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    "[ Drives... ]",
                    if focused {
                        Style::new().fg(Color::Black).bg(FOCUS).bold()
                    } else {
                        Style::new()
                    },
                ));
                spans.push(Span::raw("  "));
                spans.extend(recordings_summary(&self.storage));
            }
            Kind::Action => {
                let label = match &self.scan {
                    Scan::Pending => "[ Scanning... ]",
                    _ => "[ Scan for networks ]",
                };
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    label,
                    if focused {
                        Style::new().fg(Color::Black).bg(FOCUS).bold()
                    } else {
                        Style::new()
                    },
                ));
                match &self.scan {
                    Scan::Found(0) => {
                        spans.push(Span::styled("  none in range", Style::new().fg(FOCUS)))
                    }
                    Scan::Found(count) => spans.push(Span::styled(
                        format!("  {count} found (Enter on Wi-Fi lists them)"),
                        Style::new().fg(DIM),
                    )),
                    Scan::Failed(error) => {
                        spans.push(Span::styled(format!("  {error}"), Style::new().fg(BAD)))
                    }
                    Scan::Idle | Scan::Pending => {}
                }
            }
            Kind::Button => {}
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// What helps decide on this page: the ports and their state, the cards
    /// found, the addresses the web UI will have, the review.
    fn draw_extra(&self, frame: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = vec![Line::raw("")];
        let dim = Style::new().fg(DIM);
        match self.page() {
            Page::Welcome => {
                let addresses: Vec<String> = self
                    .ports
                    .iter()
                    .flat_map(|p| p.addresses.iter())
                    .map(|a| {
                        format!(
                            "http://{}:{}/",
                            system::bare_address(a),
                            self.original.web_port
                        )
                    })
                    .collect();
                if !addresses.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("The web UI already answers at ", dim),
                        Span::raw(addresses.join("  ")),
                    ]));
                }
            }
            Page::Name => {
                if let Ok(name) = config::valid_name(&self.name.value) {
                    lines.push(Line::from(vec![
                        Span::styled("The web UI will be at ", dim),
                        Span::raw(format!(
                            "http://{name}.local:{}/",
                            self.web_port.value.trim()
                        )),
                    ]));
                }
            }
            Page::Network => {
                lines.push(Line::styled("Ports on this machine", dim));
                if self.ports.is_empty() {
                    lines.push(Line::styled(
                        "  None found: is the network adapter supported?",
                        Style::new().fg(BAD),
                    ));
                }
                for port in &self.ports {
                    lines.push(Line::from(vec![
                        Span::raw(format!("  {:<8} {}  ", port.name, port.mac)),
                        if port.wireless {
                            Span::raw("Wi-Fi    ")
                        } else {
                            cable_span(port.carrier)
                        },
                        Span::raw(port.addresses.join(" ")),
                    ]));
                }
                if !self.wireless() && self.ports.iter().any(|p| p.wireless) {
                    lines.push(Line::styled("  For Wi-Fi, choose its port above.", dim));
                }
                if self.fixed
                    && let Ok((_, prefix)) = config::parse_cidr(&self.address.value)
                {
                    lines.push(Line::styled(
                        format!("  /{prefix} is netmask {}", config::netmask(prefix)),
                        dim,
                    ));
                }
            }
            Page::Audio => {
                if self.cards.is_empty() {
                    lines.push(Line::styled(
                        "No sound card found. Plug the interface in, then open the setup again.",
                        Style::new().fg(FOCUS),
                    ));
                } else {
                    lines.push(Line::styled("Sound cards", dim));
                    for card in &self.cards {
                        let ways = match (card.playback.is_some(), card.capture.is_some()) {
                            (true, true) => "plays and records",
                            (true, false) => "plays",
                            (false, true) => "records",
                            (false, false) => "no audio devices",
                        };
                        lines.push(Line::raw(format!(
                            "  {:<12} {}  ({ways})",
                            card.id, card.name
                        )));
                    }
                }
                if !self.session_exists {
                    lines.push(Line::styled(
                        "LiveStage makes one channel per input the first time it starts.",
                        dim,
                    ));
                }
            }
            Page::Storage => {
                lines.push(drive_header());
                for row in drive_rows(&self.storage) {
                    lines.push(row.line(&self.storage, false));
                }
                lines.push(Line::styled(
                    "Enter on Drives... to record onto one, eject it or format it.",
                    dim,
                ));
            }
            Page::Web => {
                let port = self.web_port.value.trim();
                if self.web_open {
                    let name = config::valid_name(&self.name.value).unwrap_or_default();
                    lines.push(Line::from(vec![
                        Span::styled("Open http://", dim),
                        Span::raw(format!("{name}.local:{port}/")),
                        Span::styled(" from a browser on the same network.", dim),
                    ]));
                } else {
                    lines.push(Line::styled(
                        format!("Only a browser on this machine (http://127.0.0.1:{port}/) reaches the mixer."),
                        dim,
                    ));
                }
            }
            Page::Time => {
                lines.push(Line::styled(
                    "Enter opens the list; type part of a city to find it.",
                    dim,
                ));
            }
            Page::Password => {
                let now = match self.password_state {
                    PasswordState::None => "There is no password now.",
                    PasswordState::Set => "A password is set now; a new one replaces it.",
                    PasswordState::Locked => "Root login is locked now; a password opens it.",
                    PasswordState::Unknown => "",
                };
                lines.push(Line::styled(now, dim));
            }
            Page::Review => lines.extend(self.review()),
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn review(&self) -> Vec<Line<'static>> {
        let config = self.config_with(false);
        let original = &self.original;
        let row = |label: &str, value: String, changed: bool| {
            Line::from(vec![
                Span::styled(
                    format!("{label:<width$}", width = LABEL_WIDTH as usize),
                    Style::new().fg(ACCENT),
                ),
                Span::styled(
                    value,
                    if changed {
                        Style::new().bold()
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(
                    if changed { "  (changed)" } else { "" },
                    Style::new().fg(FOCUS),
                ),
            ])
        };
        let port_name = |port: &str| match &config.wifi {
            Some(wifi) => format!("{port} (Wi-Fi \"{}\")", wifi.ssid),
            None => port.to_string(),
        };
        let network = match (&config.interface, &config.fixed) {
            (Some(port), Some(fixed)) => {
                let port = port_name(port);
                let mut text = format!("{port}: {}/{}", fixed.address, fixed.prefix);
                if let Some(gateway) = fixed.gateway {
                    text.push_str(&format!(", gateway {gateway}"));
                }
                if !fixed.dns.is_empty() {
                    text.push_str(&format!(", DNS {}", config::join_addresses(&fixed.dns)));
                }
                text
            }
            (Some(port), None) => format!("{}: DHCP", port_name(port)),
            (None, _) => "every wired port: DHCP".to_string(),
        };
        let audio = self.audio();
        let rate = if audio.rate == 0 {
            "its own rate".to_string()
        } else {
            format!("{} kHz", audio.rate as f32 / 1000.0)
        };
        let fresh = |changed: bool| self.first || changed;
        vec![
            row(
                "Name",
                format!(
                    "{}  (http://{}.local:{}/)",
                    config.name, config.name, config.web_port
                ),
                fresh(config.name != original.name),
            ),
            row(
                "Network",
                network,
                fresh(
                    config.interface != original.interface
                        || config.fixed != original.fixed
                        || config.wifi.as_ref().map(|w| &w.ssid)
                            != original.wifi.as_ref().map(|w| &w.ssid)
                        || !self.wifi_password.value.is_empty(),
                ),
            ),
            row(
                "Output",
                self.choice_label(FieldId::Output),
                audio.output != self.original_audio.output,
            ),
            row(
                "Input",
                self.choice_label(FieldId::Input),
                audio.input != self.original_audio.input,
            ),
            row(
                "",
                format!("{rate}, {} frames", audio.buffer),
                audio.rate != self.original_audio.rate
                    || audio.buffer != self.original_audio.buffer,
            ),
            row(
                "Web UI",
                format!(
                    "{}, port {}",
                    if config.web_open {
                        "every network"
                    } else {
                        "this machine only"
                    },
                    config.web_port
                ),
                config.web_open != original.web_open || config.web_port != original.web_port,
            ),
            row(
                "Recordings",
                match self.storage.volume(&self.storage.target.id) {
                    Some(volume) if volume.id != api::INTERNAL => volume_name(volume),
                    _ if self.storage.falling_back() => {
                        format!("Internal for now ({})", fallback_reason(&self.storage))
                    }
                    _ => api::INTERNAL_LABEL.to_string(),
                },
                false,
            ),
            row(
                "Time zone",
                config.timezone.clone(),
                fresh(config.timezone != original.timezone),
            ),
            row(
                "Password",
                if self.password.value.is_empty() {
                    "unchanged".into()
                } else {
                    "a new one".into()
                },
                !self.password.value.is_empty(),
            ),
        ]
    }
}

// ── The list picker ─────────────────────────────────────────────────────────

pub struct Picker {
    pub field: FieldId,
    title: &'static str,
    entries: Vec<Entry>,
    filter: String,
    /// Into the filtered list.
    selected: usize,
}

impl Picker {
    fn new(field: FieldId, title: &'static str, entries: Vec<Entry>, current: &str) -> Self {
        let selected = entries.iter().position(|e| e.key == current).unwrap_or(0);
        Self {
            field,
            title,
            entries,
            filter: String::new(),
            selected,
        }
    }

    fn visible(&self) -> Vec<&Entry> {
        let filter = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|e| filter.is_empty() || e.label.to_lowercase().contains(&filter))
            .collect()
    }

    pub fn chosen(&self) -> Option<String> {
        self.visible().get(self.selected).map(|e| e.key.clone())
    }

    pub fn key(&mut self, key: KeyEvent) {
        let count = self.visible().len();
        let page = 10;
        match key.code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
            KeyCode::PageDown => {
                self.selected = (self.selected + page).min(count.saturating_sub(1))
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = count.saturating_sub(1),
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let visible = self.visible();
        let longest = visible
            .iter()
            .map(|e| e.label.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let width = (longest + 6).clamp(40, area.width.saturating_sub(4));
        let height = (visible.len() as u16 + 4).clamp(6, area.height.saturating_sub(2));
        let rect = centered(area, width, height);
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .title(format!(" {} ", self.title))
            .border_style(Style::new().fg(FOCUS));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let [filter_area, list_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        let filter = if self.filter.is_empty() {
            Line::styled(" type to search", Style::new().fg(DIM))
        } else {
            Line::from(vec![
                Span::styled(" search: ", Style::new().fg(DIM)),
                Span::raw(self.filter.clone()),
            ])
        };
        frame.render_widget(Paragraph::new(filter), filter_area);
        if visible.is_empty() {
            frame.render_widget(
                Paragraph::new(Text::styled(" Nothing matches.", Style::new().fg(DIM))),
                list_area,
            );
            return;
        }
        let items: Vec<ListItem> = visible
            .iter()
            .map(|e| ListItem::new(format!(" {}", e.label)))
            .collect();
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(
            List::new(items).highlight_style(Style::new().fg(Color::Black).bg(FOCUS)),
            list_area,
            &mut state,
        );
    }
}
