//! The console's screens: the status of the machine (home), the setup
//! wizard, the progress of applying it, and the drives recordings go to.
//!
//! Drawn for the Linux text console: 80×25 at the least, its sixteen colours,
//! and no glyphs beyond the box-drawing set its font has.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::config::{self, AudioChoice, FixedAddress, SetupConfig, Wifi};
use super::storage::Storage;
use super::storage_api::{self as api, Mount, State as StorageState, Volume};
use super::system::{self, NetPort, PasswordState, ServiceState, SoundCard, System, WifiNetwork};

const ACCENT: Color = Color::Cyan;
const FOCUS: Color = Color::Yellow;
const GOOD: Color = Color::Green;
const BAD: Color = Color::Red;
const DIM: Color = Color::DarkGray;
const LABEL_WIDTH: u16 = 14;
const REFRESH: Duration = Duration::from_secs(2);

pub struct App {
    system: System,
    /// The drives (the console acts on them directly, as root).
    storage: Storage,
    /// On tty1: never quits (init would only start it again).
    console: bool,
    /// Opened with `--setup` from a shell: leaving the setup ends the tool.
    quit_after_setup: bool,
    screen: Screen,
    popup: Option<Popup>,
    status: Status,
    refreshed: Instant,
    pub quit: bool,
    /// Ask the terminal for a full redraw (kernel messages can land on the
    /// console underneath).
    pub redraw: bool,
}

enum Screen {
    Home { focus: usize },
    Wizard(Box<Wizard>),
    Applying(Box<Applying>),
    Storage(Box<StorageScreen>),
}

enum Popup {
    Confirm {
        title: &'static str,
        text: String,
        action: Action,
        yes: bool,
    },
    Log {
        lines: Vec<String>,
        scroll: usize,
    },
    Picker(Picker),
    Message {
        title: &'static str,
        text: String,
    },
    /// What can be done with the drive picked on the storage screen.
    DriveMenu {
        title: String,
        items: Vec<DriveAction>,
        selected: usize,
    },
    Format(FormatDialog),
}

#[derive(Clone, Copy)]
enum Action {
    RestartMixer,
    Reboot,
    PowerOff,
    LeaveSetup,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HomeAction {
    Setup,
    Storage,
    RestartMixer,
    Log,
    Reboot,
    PowerOff,
    Quit,
}

impl HomeAction {
    fn label(self) -> &'static str {
        match self {
            Self::Setup => "Setup",
            Self::Storage => "Storage",
            Self::RestartMixer => "Restart LiveStage",
            Self::Log => "Log",
            Self::Reboot => "Reboot",
            Self::PowerOff => "Power off",
            Self::Quit => "Quit",
        }
    }
}

/// What the home screen shows, read every couple of seconds.
struct Status {
    name: String,
    clock: String,
    ports: Vec<NetPort>,
    /// Per Wi-Fi port: the network it is joined to, and the signal (dBm).
    links: Vec<(String, Option<(String, Option<f32>)>)>,
    mixer: ServiceState,
    mixer_line: Option<String>,
    storage: StorageState,
    config: SetupConfig,
    password: PasswordState,
}

impl Status {
    fn read(system: &System, storage: &Storage) -> Self {
        let ports = system.ports();
        let links = ports
            .iter()
            .filter(|p| p.wireless)
            .map(|p| (p.name.clone(), system.wifi_link(&p.name)))
            .collect();
        Self {
            name: system.hostname(),
            clock: system.clock(),
            ports,
            links,
            mixer: system.service("livestage"),
            mixer_line: system.mixer_summary(),
            storage: storage.state(system),
            config: system.load_config().unwrap_or_default(),
            password: system.password(),
        }
    }

    fn addresses(&self) -> Vec<String> {
        self.ports
            .iter()
            .flat_map(|p| p.addresses.iter())
            .map(|a| system::bare_address(a).to_string())
            .collect()
    }
}

impl App {
    pub fn new(system: System, console: bool, open_setup: bool) -> Self {
        let storage = Storage::new();
        let status = Status::read(&system, &storage);
        let first = !system.is_set_up();
        let mut app = Self {
            system,
            storage,
            console,
            quit_after_setup: open_setup && !console,
            screen: Screen::Home { focus: 0 },
            popup: None,
            status,
            refreshed: Instant::now(),
            quit: false,
            redraw: false,
        };
        if first || open_setup {
            app.screen = Screen::Wizard(Box::new(Wizard::new(
                &app.system,
                first,
                app.status.storage.clone(),
            )));
        }
        app
    }

    fn home_actions(&self) -> Vec<HomeAction> {
        let mut actions = vec![
            HomeAction::Setup,
            HomeAction::Storage,
            HomeAction::RestartMixer,
            HomeAction::Log,
            HomeAction::Reboot,
            HomeAction::PowerOff,
        ];
        if !self.console {
            actions.push(HomeAction::Quit);
        }
        actions
    }

    /// Work between key presses: one step of applying the setup at a time
    /// (drawn in between), and the home screen's refresh.
    pub fn tick(&mut self) {
        if let Screen::Applying(applying) = &mut self.screen
            && !applying.finished()
        {
            applying.run_next(&self.system);
            return;
        }
        if let Screen::Wizard(wizard) = &mut self.screen
            && wizard.scan == Scan::Pending
        {
            if let Some(picker) = wizard.run_scan(&self.system) {
                self.popup = Some(Popup::Picker(picker));
            }
            return;
        }
        if let Screen::Storage(screen) = &mut self.screen
            && let Some(action) = screen.pending.take()
        {
            let outcome = action.run(&self.system, &self.storage);
            self.refresh();
            self.popup = Some(match outcome {
                Ok(Some(text)) => Popup::Message {
                    title: "Done",
                    text,
                },
                Ok(None) => return,
                Err(text) => Popup::Message {
                    title: "That did not work",
                    text,
                },
            });
            return;
        }
        if self.refreshed.elapsed() >= REFRESH {
            self.refresh();
        }
    }

    /// Whether [`App::tick`] has more to do straight away.
    pub fn busy(&self) -> bool {
        match &self.screen {
            Screen::Applying(applying) => !applying.finished(),
            Screen::Wizard(wizard) => wizard.scan == Scan::Pending,
            Screen::Storage(screen) => screen.pending.is_some(),
            Screen::Home { .. } => false,
        }
    }

    fn refresh(&mut self) {
        self.status = Status::read(&self.system, &self.storage);
        self.refreshed = Instant::now();
        // Ports come and go (a USB Wi-Fi stick, a cable): the setup follows.
        if let Screen::Wizard(wizard) = &mut self.screen {
            wizard.ports = self.status.ports.clone();
            wizard.storage = self.status.storage.clone();
        }
        if let Screen::Storage(screen) = &mut self.screen
            && let Some(wizard) = &mut screen.back
        {
            wizard.storage = self.status.storage.clone();
        }
    }

    /// The storage screen, over the setup (`back`) or from the home screen.
    fn open_storage(&mut self, back: Option<Box<Wizard>>) {
        self.refresh();
        self.screen = Screen::Storage(Box::new(StorageScreen {
            back,
            selected: 0,
            pending: None,
        }));
    }

    fn leave_storage(&mut self) {
        let screen = std::mem::replace(&mut self.screen, Screen::Home { focus: 0 });
        self.screen = match screen {
            Screen::Storage(screen) => match screen.back {
                Some(wizard) => Screen::Wizard(wizard),
                None => Screen::Home {
                    focus: self
                        .home_actions()
                        .iter()
                        .position(|a| *a == HomeAction::Storage)
                        .unwrap_or(0),
                },
            },
            other => other,
        };
        self.refresh();
    }

    fn go_home(&mut self) {
        if self.quit_after_setup {
            self.quit = true;
            return;
        }
        self.screen = Screen::Home { focus: 0 };
        self.refresh();
    }

    // ── Keys ────────────────────────────────────────────────────────────────

    pub fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('l') {
            self.redraw = true;
            return;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            if !self.console {
                self.quit = true;
            }
            return;
        }
        if let Some(popup) = self.popup.take() {
            self.popup_key(popup, key);
            return;
        }
        let outcome = match &mut self.screen {
            Screen::Home { .. } => return self.home_key(key),
            Screen::Storage(_) => return self.storage_key(key),
            Screen::Wizard(wizard) => match wizard.key(key) {
                // Nothing to go back to from the first setup: it is the way in.
                WizardOutcome::Leave if wizard.first => WizardOutcome::Stay,
                outcome => outcome,
            },
            Screen::Applying(applying) => {
                if applying.finished() && matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    self.go_home();
                }
                return;
            }
        };
        match outcome {
            WizardOutcome::Stay => {}
            WizardOutcome::Leave => {
                self.popup = Some(Popup::Confirm {
                    title: "Leave the setup",
                    text: "Leave without changing anything?".into(),
                    action: Action::LeaveSetup,
                    yes: false,
                })
            }
            WizardOutcome::Pick(picker) => self.popup = Some(Popup::Picker(picker)),
            WizardOutcome::Apply(applying) => self.screen = Screen::Applying(applying),
            WizardOutcome::Storage => {
                if let Screen::Wizard(wizard) =
                    std::mem::replace(&mut self.screen, Screen::Home { focus: 0 })
                {
                    self.open_storage(Some(wizard));
                }
            }
        }
    }

    fn storage_key(&mut self, key: KeyEvent) {
        let rows = drive_rows(&self.status.storage);
        let Screen::Storage(screen) = &mut self.screen else {
            return;
        };
        let last = rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::BackTab => screen.selected = screen.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => screen.selected = (screen.selected + 1).min(last),
            KeyCode::Home => screen.selected = 0,
            KeyCode::End => screen.selected = last,
            KeyCode::Esc | KeyCode::Left => self.leave_storage(),
            KeyCode::Char('q') if screen.back.is_none() && !self.console => self.quit = true,
            KeyCode::Enter | KeyCode::Right => {
                let Some(row) = rows.get(screen.selected.min(last)) else {
                    return;
                };
                let state = &self.status.storage;
                let items = row.actions(state);
                self.popup = Some(if items.is_empty() {
                    Popup::Message {
                        title: "Storage",
                        text: if row.is_recording(state) {
                            "Recordings go here already.".to_string()
                        } else {
                            "Nothing can be done with this one here.".to_string()
                        },
                    }
                } else {
                    Popup::DriveMenu {
                        title: row.title(state),
                        items,
                        selected: 0,
                    }
                });
            }
            _ => {}
        }
    }

    /// An action chosen on the storage screen: done at the next tick (after
    /// "Working..." is drawn), or first the format dialog.
    fn drive_action(&mut self, action: DriveAction) {
        if let DriveAction::Format {
            disk,
            what,
            label: None,
        } = &action
        {
            self.popup = Some(Popup::Format(FormatDialog::new(disk, what)));
            return;
        }
        if let Screen::Storage(screen) = &mut self.screen {
            screen.pending = Some(action);
        }
    }

    fn home_key(&mut self, key: KeyEvent) {
        let actions = self.home_actions();
        let Screen::Home { focus } = &mut self.screen else {
            return;
        };
        match key.code {
            KeyCode::Left | KeyCode::BackTab | KeyCode::Up => {
                *focus = (*focus + actions.len() - 1) % actions.len()
            }
            KeyCode::Right | KeyCode::Tab | KeyCode::Down => *focus = (*focus + 1) % actions.len(),
            KeyCode::Char('q') if !self.console => self.quit = true,
            KeyCode::Enter => {
                let action = actions[(*focus).min(actions.len() - 1)];
                self.home_action(action);
            }
            _ => {}
        }
    }

    fn home_action(&mut self, action: HomeAction) {
        match action {
            HomeAction::Setup => {
                self.screen = Screen::Wizard(Box::new(Wizard::new(
                    &self.system,
                    false,
                    self.status.storage.clone(),
                )))
            }
            HomeAction::Storage => self.open_storage(None),
            HomeAction::RestartMixer => {
                self.popup = Some(Popup::Confirm {
                    title: "Restart LiveStage",
                    text: "The sound stops for a few seconds. The take being recorded is \
                           closed and the session saved first."
                        .into(),
                    action: Action::RestartMixer,
                    yes: false,
                })
            }
            HomeAction::Log => {
                let lines = self.system.log_tail(500);
                let scroll = lines.len();
                self.popup = Some(Popup::Log { lines, scroll })
            }
            HomeAction::Reboot => {
                self.popup = Some(Popup::Confirm {
                    title: "Reboot",
                    text: "Restart the machine? LiveStage saves the session first.".into(),
                    action: Action::Reboot,
                    yes: false,
                })
            }
            HomeAction::PowerOff => {
                self.popup = Some(Popup::Confirm {
                    title: "Power off",
                    text: "Turn the machine off? LiveStage saves the session first.".into(),
                    action: Action::PowerOff,
                    yes: false,
                })
            }
            HomeAction::Quit => self.quit = true,
        }
    }

    fn popup_key(&mut self, popup: Popup, key: KeyEvent) {
        match popup {
            Popup::Confirm {
                title,
                text,
                action,
                yes,
            } => match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    self.popup = Some(Popup::Confirm {
                        title,
                        text,
                        action,
                        yes: !yes,
                    })
                }
                KeyCode::Char('y') => self.confirmed(action),
                KeyCode::Enter if yes => self.confirmed(action),
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('n') => {}
                _ => {
                    self.popup = Some(Popup::Confirm {
                        title,
                        text,
                        action,
                        yes,
                    })
                }
            },
            Popup::Log { lines, mut scroll } => {
                let page = 10;
                match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => return,
                    KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::Down => scroll = (scroll + 1).min(lines.len()),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(page),
                    KeyCode::PageDown => scroll = (scroll + page).min(lines.len()),
                    KeyCode::Home => scroll = 0,
                    KeyCode::End => scroll = lines.len(),
                    _ => {}
                }
                self.popup = Some(Popup::Log { lines, scroll });
            }
            Popup::Picker(mut picker) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if let (Some(key), Screen::Wizard(wizard)) = (picker.chosen(), &mut self.screen)
                    {
                        wizard.set_choice(picker.field, &key);
                    }
                }
                _ => {
                    picker.key(key);
                    self.popup = Some(Popup::Picker(picker));
                }
            },
            Popup::Message { .. } => {}
            Popup::DriveMenu {
                title,
                items,
                mut selected,
            } => match key.code {
                KeyCode::Esc | KeyCode::Left => {}
                KeyCode::Enter => {
                    if let Some(action) = items.get(selected).cloned() {
                        self.drive_action(action);
                    }
                }
                code => {
                    match code {
                        KeyCode::Up | KeyCode::BackTab => selected = selected.saturating_sub(1),
                        KeyCode::Down | KeyCode::Tab => {
                            selected = (selected + 1).min(items.len().saturating_sub(1))
                        }
                        _ => {}
                    }
                    self.popup = Some(Popup::DriveMenu {
                        title,
                        items,
                        selected,
                    });
                }
            },
            Popup::Format(mut dialog) => match dialog.key(key) {
                FormatOutcome::Stay => self.popup = Some(Popup::Format(dialog)),
                FormatOutcome::Cancel => {}
                FormatOutcome::Format(label) => self.drive_action(DriveAction::Format {
                    disk: dialog.disk,
                    what: dialog.what,
                    label: Some(label),
                }),
            },
        }
    }

    fn confirmed(&mut self, action: Action) {
        let result = match action {
            Action::RestartMixer => self
                .system
                .run("rc-service", &["livestage", "restart"])
                .map(|_| ()),
            Action::Reboot => self.system.run("reboot", &[]).map(|_| ()),
            Action::PowerOff => self.system.run("poweroff", &[]).map(|_| ()),
            Action::LeaveSetup => {
                self.go_home();
                Ok(())
            }
        };
        match (action, result) {
            (_, Err(error)) => {
                self.popup = Some(Popup::Message {
                    title: "That did not work",
                    text: error,
                })
            }
            (Action::Reboot, Ok(())) => {
                self.popup = Some(Popup::Message {
                    title: "Reboot",
                    text: "Restarting...".into(),
                })
            }
            (Action::PowerOff, Ok(())) => {
                self.popup = Some(Popup::Message {
                    title: "Power off",
                    text: "Shutting down...".into(),
                })
            }
            _ => self.refresh(),
        }
    }

    // ── Drawing ─────────────────────────────────────────────────────────────

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        let title = match &self.screen {
            Screen::Home { .. } => " LiveStage".to_string(),
            Screen::Wizard(wizard) => format!(" LiveStage setup  -  {}", wizard.page().title()),
            Screen::Applying(_) => " LiveStage setup  -  Applying".to_string(),
            Screen::Storage(screen) if screen.back.is_some() => {
                " LiveStage setup  -  Storage".to_string()
            }
            Screen::Storage(_) => " LiveStage  -  Storage".to_string(),
        };
        let bar = Style::new().fg(Color::Black).bg(ACCENT);
        frame.render_widget(Paragraph::new(title).style(bar).bold(), header);
        frame.render_widget(
            Paragraph::new(format!("{}  {} ", self.status.name, self.status.clock))
                .style(bar)
                .right_aligned(),
            // Leave the title its room on a narrow screen.
            Rect {
                x: header.x + header.width / 2,
                width: header.width - header.width / 2,
                ..header
            },
        );
        let help = match (&self.popup, &self.screen) {
            (Some(Popup::Picker(_)), _) => {
                "Type to search   Up/Down move   Enter choose   Esc close"
            }
            (Some(Popup::Log { .. }), _) => "Up/Down PgUp/PgDn scroll   Esc close",
            (Some(Popup::Confirm { .. }), _) => "Left/Right choose   Enter confirm   Esc cancel",
            (Some(Popup::Message { .. }), _) => "Any key closes",
            (Some(Popup::DriveMenu { .. }), _) => "Up/Down choose   Enter do it   Esc close",
            (Some(Popup::Format(_)), _) => "Tab next field   Enter format   Esc cancel",
            (None, Screen::Storage(screen)) if screen.pending.is_some() => "Working...",
            (None, Screen::Storage(_)) => "Up/Down choose   Enter what to do   Esc back",
            (None, Screen::Home { .. }) if self.console => {
                "Left/Right choose   Enter open   Alt+F2 shell"
            }
            (None, Screen::Home { .. }) => "Left/Right choose   Enter open   q quit",
            (None, Screen::Wizard(_)) => {
                "Up/Down move   Left/Right change   Enter list/next   Esc back"
            }
            (None, Screen::Applying(a)) if a.finished() => "Enter done",
            (None, Screen::Applying(_)) => "Working...",
        };
        frame.render_widget(Paragraph::new(format!(" {help}")).fg(DIM), footer);

        match &self.screen {
            Screen::Home { focus } => self.draw_home(frame, body, *focus),
            Screen::Wizard(wizard) => wizard.draw(frame, body),
            Screen::Applying(applying) => applying.draw(frame, body),
            Screen::Storage(screen) => screen.draw(frame, body, &self.status.storage),
        }
        if let Some(popup) = &self.popup {
            draw_popup(frame, body, popup);
        }
    }

    fn draw_home(&self, frame: &mut Frame, area: Rect, focus: usize) {
        let status = &self.status;
        let config = &status.config;
        let mut lines: Vec<Line> = Vec::new();
        let row = |label: &str, value: Vec<Span<'static>>| {
            let mut spans = vec![Span::styled(
                format!("{label:<width$}", width = LABEL_WIDTH as usize),
                Style::new().fg(ACCENT),
            )];
            spans.extend(value);
            Line::from(spans)
        };

        // Where the web UI is: the first thing anyone at the screen needs.
        let port = config.web_port;
        let addresses = status.addresses();
        if !config.web_open {
            lines.push(row(
                "Web UI",
                vec![Span::raw(format!(
                    "this machine only (http://127.0.0.1:{port}/); open it in Setup"
                ))],
            ));
        } else if addresses.is_empty() {
            lines.push(row(
                "Web UI",
                vec![Span::styled(
                    "no address yet: connect a cable, or set one in Setup",
                    Style::new().fg(FOCUS),
                )],
            ));
        } else {
            for (index, address) in addresses.iter().enumerate() {
                lines.push(row(
                    if index == 0 { "Web UI" } else { "" },
                    vec![Span::styled(
                        format!("http://{address}:{port}/"),
                        Style::new().bold(),
                    )],
                ));
            }
            lines.push(row(
                "",
                vec![Span::raw(format!("http://{}.local:{port}/", status.name))],
            ));
        }
        lines.push(Line::raw(""));

        let (state, colour) = match status.mixer {
            ServiceState::Running => ("running", GOOD),
            ServiceState::Stopped => ("stopped", BAD),
            ServiceState::Failed => ("crashed", BAD),
            ServiceState::Unknown => ("unknown", DIM),
        };
        lines.push(row(
            "LiveStage",
            vec![Span::styled(state, Style::new().fg(colour).bold())],
        ));
        if let Some(line) = &status.mixer_line {
            lines.push(row("", vec![Span::raw(line.clone())]));
        }

        if status.ports.is_empty() {
            lines.push(row(
                "Network",
                vec![Span::styled("no network port found", Style::new().fg(BAD))],
            ));
        }
        for (index, port) in status.ports.iter().enumerate() {
            let mode = match (&config.interface, &config.fixed) {
                (Some(name), Some(_)) if *name == port.name => "fixed",
                (Some(name), None) if *name == port.name => "DHCP",
                (Some(_), _) => "not used",
                (None, _) => "DHCP",
            };
            let mut value = vec![Span::raw(format!("{:<8}", port.name))];
            if port.wireless {
                let link = status
                    .links
                    .iter()
                    .find(|(name, _)| *name == port.name)
                    .and_then(|(_, link)| link.as_ref());
                value.push(match link {
                    Some((ssid, signal)) => Span::styled(
                        format!(
                            "Wi-Fi {ssid}{}",
                            signal.map(|s| format!(" ({s:.0} dBm)")).unwrap_or_default()
                        ),
                        Style::new().fg(GOOD),
                    ),
                    None => Span::styled("Wi-Fi, not joined", Style::new().fg(DIM)),
                });
            } else {
                value.push(cable_span(port.carrier));
            }
            value.push(Span::raw(format!("  {mode:<8} ")));
            value.push(Span::raw(port.addresses.join(" ")));
            lines.push(row(if index == 0 { "Network" } else { "" }, value));
        }

        lines.push(row("Name", vec![Span::raw(status.name.clone())]));
        lines.push(row("Time zone", vec![Span::raw(config.timezone.clone())]));
        lines.push(row("Recordings", recordings_summary(&status.storage)));
        match status.password {
            PasswordState::None => lines.push(row(
                "Console",
                vec![Span::styled(
                    "no password (set one in Setup)",
                    Style::new().fg(FOCUS),
                )],
            )),
            PasswordState::Set => lines.push(row("Console", vec![Span::raw("password set")])),
            PasswordState::Locked => {
                lines.push(row("Console", vec![Span::raw("root login locked")]))
            }
            PasswordState::Unknown => {}
        }

        let [info, _, buttons] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area.inner(Margin::new(1, 0)));
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .title(" This machine ")
                    .border_style(DIM)
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            ),
            info,
        );
        let labels: Vec<&str> = self.home_actions().iter().map(|a| a.label()).collect();
        // Every button on one line, closer together on a narrow screen.
        let wide: usize = 1 + labels.iter().map(|l| l.chars().count() + 6).sum::<usize>();
        frame.render_widget(
            if wide <= buttons.width as usize {
                button_row(&labels, Some(focus))
            } else {
                compact_button_row(&labels, Some(focus))
            },
            buttons,
        );
    }
}

/// Where recordings go, for the home screen: the drive and its free space,
/// or the warning that its drive is missing.
fn recordings_summary(state: &StorageState) -> Vec<Span<'static>> {
    let internal = state.volume(api::INTERNAL);
    let space = |volume: Option<&Volume>| match volume.and_then(|v| v.free_bytes) {
        Some(free) => format!("{} free", system::megabytes(free)),
        None => "free space unknown".to_string(),
    };
    if state.falling_back() {
        return vec![Span::styled(
            format!(
                "{}: recording to Internal ({})",
                fallback_reason(state),
                space(internal)
            ),
            Style::new().fg(FOCUS),
        )];
    }
    let volume = state.volume(&state.target.id);
    let name = match volume {
        Some(v) if v.id != api::INTERNAL => volume_name(v),
        _ => api::INTERNAL_LABEL.to_string(),
    };
    let mut text = format!("{name}: {}", space(volume));
    if let Some(size) = volume.map(|v| v.size_bytes).filter(|s| *s > 0) {
        text.push_str(&format!(" of {}", system::megabytes(size)));
    }
    if let Some(model) = volume
        .filter(|v| v.id != api::INTERNAL)
        .and_then(|v| v.model.as_ref())
    {
        text.push_str(&format!(" ({model})"));
    }
    vec![Span::raw(text)]
}

/// Why recordings are not going to the drive chosen for them.
fn fallback_reason(state: &StorageState) -> String {
    match state.volume(&state.target.id) {
        Some(volume) if volume.ejected => format!("{} is ejected", state.target.label),
        Some(_) => format!("{} cannot be written to", state.target.label),
        None => format!("{} is not plugged in", state.target.label),
    }
}

/// A volume as people know it: its label, else its device.
fn volume_name(volume: &Volume) -> String {
    if volume.id == api::INTERNAL {
        api::INTERNAL_LABEL.to_string()
    } else if volume.label.is_empty() {
        format!("{} (no name)", volume.device)
    } else {
        volume.label.clone()
    }
}

fn cable_span(carrier: Option<bool>) -> Span<'static> {
    match carrier {
        Some(true) => Span::styled("cable in ", Style::new().fg(GOOD)),
        Some(false) => Span::styled("no cable ", Style::new().fg(FOCUS)),
        None => Span::styled("down     ", Style::new().fg(DIM)),
    }
}

fn button_row(labels: &[&str], focus: Option<usize>) -> Paragraph<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (index, label) in labels.iter().enumerate() {
        let style = if focus == Some(index) {
            Style::new().fg(Color::Black).bg(FOCUS).bold()
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!("[ {label} ]"), style));
        spans.push(Span::raw("  "));
    }
    Paragraph::new(Line::from(spans))
}

/// [`button_row`] for a narrow screen: `[Label]`, one space apart.
fn compact_button_row(labels: &[&str], focus: Option<usize>) -> Paragraph<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (index, label) in labels.iter().enumerate() {
        let style = if focus == Some(index) {
            Style::new().fg(Color::Black).bg(FOCUS).bold()
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!("[{label}]"), style));
        spans.push(Span::raw(" "));
    }
    Paragraph::new(Line::from(spans))
}

fn draw_popup(frame: &mut Frame, area: Rect, popup: &Popup) {
    match popup {
        Popup::Confirm {
            title, text, yes, ..
        } => {
            let rect = centered(area, 60, 8);
            frame.render_widget(Clear, rect);
            let block = Block::bordered()
                .title(format!(" {title} "))
                .border_style(Style::new().fg(FOCUS));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let [text_area, buttons] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
                    .areas(inner.inner(Margin::new(1, 0)));
            frame.render_widget(
                Paragraph::new(text.as_str()).wrap(Wrap { trim: true }),
                text_area,
            );
            frame.render_widget(button_row(&["No", "Yes"], Some(usize::from(*yes))), buttons);
        }
        Popup::Message { title, text } => {
            let rect = centered(area, 60, 7);
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(text.as_str())
                    .wrap(Wrap { trim: true })
                    .block(
                        Block::bordered()
                            .title(format!(" {title} "))
                            .border_style(Style::new().fg(FOCUS))
                            .padding(ratatui::widgets::Padding::horizontal(1)),
                    ),
                rect,
            );
        }
        Popup::Log { lines, scroll } => {
            let rect = area.inner(Margin::new(2, 1));
            frame.render_widget(Clear, rect);
            let block = Block::bordered()
                .title(format!(" {} ", system::LOG))
                .border_style(Style::new().fg(ACCENT));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let height = inner.height as usize;
            // `scroll` is the line at the bottom of the view.
            let end = (*scroll).clamp(height.min(lines.len()), lines.len());
            let start = end.saturating_sub(height);
            let text: Vec<Line> = lines[start..end]
                .iter()
                .map(|l| Line::raw(l.as_str()))
                .collect();
            frame.render_widget(Paragraph::new(text), inner);
        }
        Popup::Picker(picker) => picker.draw(frame, area),
        Popup::DriveMenu {
            title,
            items,
            selected,
        } => {
            let longest = items
                .iter()
                .map(|i| i.label().chars().count())
                .chain(std::iter::once(title.chars().count()))
                .max()
                .unwrap_or(0) as u16;
            let rect = centered(area, (longest + 8).max(30), items.len() as u16 + 2);
            frame.render_widget(Clear, rect);
            let block = Block::bordered()
                .title(format!(" {title} "))
                .border_style(Style::new().fg(FOCUS));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let list: Vec<ListItem> = items
                .iter()
                .map(|i| ListItem::new(format!(" {}", i.label())))
                .collect();
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(
                List::new(list).highlight_style(Style::new().fg(Color::Black).bg(FOCUS)),
                inner,
                &mut state,
            );
        }
        Popup::Format(dialog) => dialog.draw(frame, area),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

// ── Text fields ─────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct TextInput {
    value: String,
    /// In characters.
    cursor: usize,
    secret: bool,
}

impl TextInput {
    fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            cursor: value.chars().count(),
            value,
            secret: false,
        }
    }

    fn secret() -> Self {
        Self {
            secret: true,
            ..Self::default()
        }
    }

    fn byte(&self, chars: usize) -> usize {
        self.value
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    /// True when the key was for the field.
    fn key(&mut self, key: KeyEvent) -> bool {
        let length = self.value.chars().count();
        match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if length < 64 && !c.is_control() {
                    let at = self.byte(self.cursor);
                    self.value.insert(at, c);
                    self.cursor += 1;
                }
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte(self.cursor);
                self.value.remove(at);
            }
            KeyCode::Delete if self.cursor < length => {
                let at = self.byte(self.cursor);
                self.value.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(length),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = length,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    fn shown(&self) -> String {
        if self.secret {
            "*".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        }
    }
}

// ── The wizard ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
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

impl Page {
    fn title(self) -> &'static str {
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
enum FieldId {
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
enum Scan {
    Idle,
    /// Asked for: runs at the next tick, after "Scanning..." is drawn.
    Pending,
    Found(usize),
    Failed(String),
}

enum WizardOutcome {
    Stay,
    Leave,
    Pick(Picker),
    Apply(Box<Applying>),
    /// The drives page, over the setup.
    Storage,
}

struct Wizard {
    first: bool,
    page: usize,
    focus: usize,
    error: Option<String>,
    original: SetupConfig,
    original_audio: AudioChoice,
    session_exists: bool,
    password_state: PasswordState,
    ports: Vec<NetPort>,
    cards: Vec<SoundCard>,
    zones: Vec<String>,
    /// The drives, as last read.
    storage: StorageState,

    name: TextInput,
    interface: Option<String>,
    fixed: bool,
    address: TextInput,
    gateway: TextInput,
    dns: TextInput,
    ssid: TextInput,
    wifi_password: TextInput,
    networks: Vec<WifiNetwork>,
    scan: Scan,
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
    fn new(system: &System, first: bool, storage: StorageState) -> Self {
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

    fn page(&self) -> Page {
        PAGES[self.page]
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

    fn set_choice(&mut self, id: FieldId, key: &str) {
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

    fn key(&mut self, key: KeyEvent) -> WizardOutcome {
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
                return WizardOutcome::Apply(Box::new(self.applying(true)));
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
            FieldId::Apply => return WizardOutcome::Apply(Box::new(self.applying(false))),
            _ => {}
        }
        WizardOutcome::Stay
    }

    fn back(&mut self) -> WizardOutcome {
        let first_page = if self.first { 0 } else { 1 };
        if self.page <= first_page {
            return WizardOutcome::Leave;
        }
        self.go(self.page - 1);
        WizardOutcome::Stay
    }

    fn go(&mut self, page: usize) {
        self.page = page.min(PAGES.len() - 1);
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
    fn run_scan(&mut self, system: &System) -> Option<Picker> {
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
    fn config(&self) -> SetupConfig {
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

    fn applying(&self, defaults: bool) -> Applying {
        let config = if defaults {
            self.original.clone()
        } else {
            self.config()
        };
        let audio = config.audio.clone();
        let original = &self.original;
        let network_changed = self.first
            || config.interface != original.interface
            || config.fixed != original.fixed
            || config.wifi != original.wifi;
        let name_changed =
            self.first || config.name != original.name || config.timezone != original.timezone;
        let audio_changed = audio != self.original_audio;
        let password =
            (!defaults && !self.password.value.is_empty()).then(|| self.password.value.clone());
        let mixer_changed = self.first
            || audio_changed
            || config.web_open != original.web_open
            || config.web_port != original.web_port
            || config.timezone != original.timezone;
        let mut steps = vec![Step::Save];
        if network_changed {
            steps.push(Step::Network);
        }
        if name_changed {
            steps.push(Step::Name);
        }
        if audio_changed {
            steps.push(Step::Audio);
        }
        if password.is_some() {
            steps.push(Step::Password);
        }
        if mixer_changed || audio_changed {
            steps.push(Step::Mixer);
        }
        if network_changed {
            steps.push(Step::Services);
        }
        Applying {
            config,
            audio,
            // No session when the setup opened: the one LiveStage has saved
            // since was made on the system's default device, before anyone
            // chose one.
            fresh_session: !self.session_exists,
            password,
            steps: steps.into_iter().map(|s| (s, StepState::Waiting)).collect(),
            next: 0,
        }
    }

    // ── Drawing ─────────────────────────────────────────────────────────────

    fn draw(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered().border_style(DIM);
        let outer = area.inner(Margin::new(1, 0));
        let inner = block.inner(outer).inner(Margin::new(1, 0));
        frame.render_widget(block, outer);

        // The steps, the current one lit; closer together when they would
        // not fit.
        let shown = PAGES.len() - usize::from(!self.first);
        let wide = PAGES[PAGES.len() - shown..]
            .iter()
            .map(|p| p.short().len())
            .sum::<usize>()
            + 3 * (shown - 1);
        let separator = if wide <= inner.width as usize {
            " > "
        } else {
            "  "
        };
        let mut steps = Vec::new();
        for (index, page) in PAGES.iter().enumerate() {
            if index == 0 && !self.first {
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

        let intro = self.page().intro();
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
        let labels: Vec<&str> = buttons.iter().map(|b| b.label()).collect();
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

/// Lines a paragraph takes when wrapped to `width` (by words, roughly as
/// ratatui wraps).
fn wrapped_height(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    text.split('\n')
        .map(|paragraph| {
            let mut lines = 1;
            let mut used = 0;
            for word in paragraph.split_whitespace() {
                let length = word.chars().count();
                if used == 0 {
                    used = length;
                } else if used + 1 + length <= width {
                    used += 1 + length;
                } else {
                    lines += 1;
                    used = length;
                }
            }
            lines
        })
        .sum::<usize>() as u16
}

// ── The list picker ─────────────────────────────────────────────────────────

struct Picker {
    field: FieldId,
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

    fn chosen(&self) -> Option<String> {
        self.visible().get(self.selected).map(|e| e.key.clone())
    }

    fn key(&mut self, key: KeyEvent) {
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

    fn draw(&self, frame: &mut Frame, area: Rect) {
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

// ── Applying ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Save,
    Network,
    Name,
    Audio,
    Password,
    Mixer,
    /// Whatever stopping the network took down with it, started again.
    Services,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Save => "Saving the settings",
            Step::Network => "Restarting the network",
            Step::Name => "Setting the name and time zone",
            Step::Audio => "Choosing the audio interface",
            Step::Password => "Setting the console password",
            Step::Mixer => "Restarting LiveStage",
            Step::Services => "Starting the other services again",
        }
    }
}

enum StepState {
    Waiting,
    Done,
    Failed(String),
}

struct Applying {
    config: SetupConfig,
    audio: AudioChoice,
    fresh_session: bool,
    password: Option<String>,
    steps: Vec<(Step, StepState)>,
    next: usize,
}

impl Applying {
    fn finished(&self) -> bool {
        self.next >= self.steps.len()
    }

    fn run_next(&mut self, system: &System) {
        let Some((step, state)) = self.steps.get_mut(self.next) else {
            return;
        };
        let result = match step {
            Step::Save => {
                // Where recordings go is saved by the drives page as it is
                // chosen (or by the web UI meanwhile): keep the latest.
                let mut config = self.config.clone();
                if let Some(saved) = system.load_config() {
                    config.record_storage = saved.record_storage;
                    config.record_storage_label = saved.record_storage_label;
                }
                system.save_config(&config)
            }
            Step::Network => system.restart_network(&self.config),
            Step::Name => system.write_runtime(&self.config).and_then(|_| {
                // avahi answers for the new name once it starts again.
                system
                    .run("rc-service", &["avahi-daemon", "restart"])
                    .map(|_| ())
            }),
            // Stopped first: LiveStage writes its session as it stops. A
            // session from before the first setup (made on the default
            // device) is set aside, and LiveStage starts afresh on the chosen
            // interface, which the service passes on its command line. Any
            // other session gets the interface written into it.
            Step::Audio => system
                .run("rc-service", &["livestage", "stop"])
                .and_then(|_| {
                    if self.fresh_session {
                        system.set_aside_session()
                    } else if system.session_audio().is_some() {
                        system.set_session_audio(&self.audio)
                    } else {
                        Ok(())
                    }
                }),
            Step::Password => match &self.password {
                Some(password) => system.set_root_password(password),
                None => Ok(()),
            },
            Step::Mixer => system
                .run("rc-service", &["livestage", "restart"])
                .map(|_| ()),
            Step::Services => system.run("openrc", &["default"]).map(|_| ()),
        };
        *state = match result {
            Ok(()) => StepState::Done,
            Err(error) => StepState::Failed(error),
        };
        self.next += 1;
        // The password is not kept once it is set.
        if *step == Step::Password {
            self.password = None;
        }
    }

    fn draw(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::raw("")];
        for (index, (step, state)) in self.steps.iter().enumerate() {
            let (mark, style) = match state {
                StepState::Done => ("  ok  ", Style::new().fg(GOOD)),
                StepState::Failed(_) => (" fail ", Style::new().fg(BAD).bold()),
                StepState::Waiting if index == self.next => (" .... ", Style::new().fg(FOCUS)),
                StepState::Waiting => ("      ", Style::new().fg(DIM)),
            };
            lines.push(Line::from(vec![
                Span::raw(" ["),
                Span::styled(mark, style),
                Span::raw("] "),
                Span::raw(step.label()),
            ]));
            if let StepState::Failed(error) = state {
                lines.push(Line::styled(
                    format!("          {error}"),
                    Style::new().fg(BAD),
                ));
            }
        }
        if self.finished() {
            let failed = self
                .steps
                .iter()
                .any(|(_, s)| matches!(s, StepState::Failed(_)));
            lines.push(Line::raw(""));
            lines.push(if failed {
                Line::styled(
                    " Some of it did not work (above). The settings are saved; they are used in full at the next boot.",
                    Style::new().fg(BAD),
                )
            } else {
                Line::styled(" Done.", Style::new().fg(GOOD).bold())
            });
            let port = self.config.web_port;
            if self.config.web_open {
                lines.push(Line::raw(format!(
                    " Open http://{}.local:{port}/ in a browser on the same network.",
                    self.config.name
                )));
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("[ Finish ]", Style::new().fg(Color::Black).bg(FOCUS).bold()),
            ]));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().border_style(DIM)),
            area.inner(Margin::new(1, 0)),
        );
    }
}

// ── Storage ─────────────────────────────────────────────────────────────────

/// The drives: every volume (and every disk with none), where recordings
/// go, and what can be done with each.
struct StorageScreen {
    /// The setup it was opened from, to go back to.
    back: Option<Box<Wizard>>,
    selected: usize,
    /// Chosen; done at the next tick, after "Working..." is drawn.
    pending: Option<DriveAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DriveAction {
    Use {
        id: String,
        name: String,
    },
    Eject {
        id: String,
        name: String,
    },
    /// Without a label: the format dialog first.
    Format {
        disk: String,
        what: String,
        label: Option<String>,
    },
}

impl DriveAction {
    fn label(&self) -> &'static str {
        match self {
            Self::Use { .. } => "Record here",
            Self::Eject { .. } => "Eject",
            Self::Format { .. } => "Format disk...",
        }
    }

    fn working(&self) -> String {
        match self {
            Self::Use { name, .. } => format!("Switching recordings to {name}..."),
            Self::Eject { name, .. } => format!("Ejecting {name}..."),
            Self::Format { what, .. } => {
                format!("Formatting {what}... this can take a minute.")
            }
        }
    }

    /// Does it: what to tell, when there is something.
    fn run(self, system: &System, storage: &Storage) -> Result<Option<String>, String> {
        let result = self.run_now(system, storage);
        if system.is_live() {
            return result;
        }
        result.map(|text| {
            Some(format!(
                "{}(Trying out with --root: no command was run.)",
                text.map(|t| format!("{t} ")).unwrap_or_default()
            ))
        })
    }

    fn run_now(self, system: &System, storage: &Storage) -> Result<Option<String>, String> {
        match self {
            Self::Use { id, .. } => storage.use_target(system, &id).map(|_| None),
            Self::Eject { id, name } => storage
                .eject(system, &id)
                .map(|_| Some(format!("{name} can be pulled out now."))),
            Self::Format { disk, what, label } => {
                let label = label.unwrap_or_else(|| api::DEFAULT_LABEL.to_string());
                storage
                    .format(system, &disk, &label)
                    .map(|_| Some(format!("{what} is now one exFAT volume, {label}.")))
            }
        }
    }
}

/// A line of the drives list: a volume, or a disk with none on it.
#[derive(Clone, Copy)]
enum DriveRow {
    Volume(usize),
    Blank(usize),
}

fn drive_rows(state: &StorageState) -> Vec<DriveRow> {
    let mut rows: Vec<DriveRow> = (0..state.volumes.len()).map(DriveRow::Volume).collect();
    for (index, disk) in state.disks.iter().enumerate() {
        if !disk.system && !state.volumes.iter().any(|v| v.disk == disk.disk) {
            rows.push(DriveRow::Blank(index));
        }
    }
    rows
}

/// The column names over [`DriveRow::line`].
fn drive_header() -> Line<'static> {
    Line::styled(
        format!(
            "  {:<12} {:<19} {:<7}{:>8}{:>8}  {}",
            "Volume", "Drive", "Format", "Size", "Free", "State"
        ),
        Style::new().fg(DIM),
    )
}

fn fs_name(fs: Option<&str>) -> String {
    match fs {
        Some("exfat") => "exFAT".into(),
        Some("vfat") => "FAT32".into(),
        Some("ntfs") => "NTFS".into(),
        Some(other) => other.into(),
        None => "-".into(),
    }
}

/// At most `width` characters.
fn fit(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

impl DriveRow {
    fn is_recording(self, state: &StorageState) -> bool {
        match self {
            Self::Volume(index) => {
                let volume = &state.volumes[index];
                (volume.id == state.target.id && state.target.available)
                    || (volume.id == api::INTERNAL && state.falling_back())
            }
            Self::Blank(_) => false,
        }
    }

    /// `sdb, SanDisk Ultra (64 GB)`: the whole disk, for Format.
    fn disk_text(state: &StorageState, disk: &str) -> String {
        let found = state.disks.iter().find(|d| d.disk == disk);
        let mut text = disk.to_string();
        if let Some(model) = found.and_then(|d| d.model.as_ref()) {
            text.push_str(&format!(", {model}"));
        }
        if let Some(size) = found.map(|d| d.size_bytes) {
            text.push_str(&format!(" ({})", system::megabytes(size)));
        }
        text
    }

    fn title(self, state: &StorageState) -> String {
        match self {
            Self::Volume(index) => {
                let volume = &state.volumes[index];
                match &volume.model {
                    Some(model) if volume.id != api::INTERNAL => {
                        format!("{} ({model})", volume_name(volume))
                    }
                    _ => volume_name(volume),
                }
            }
            Self::Blank(index) => Self::disk_text(state, &state.disks[index].disk),
        }
    }

    fn actions(self, state: &StorageState) -> Vec<DriveAction> {
        let format = |disk: &str| DriveAction::Format {
            disk: disk.to_string(),
            what: Self::disk_text(state, disk),
            label: None,
        };
        match self {
            Self::Volume(index) => {
                let volume = &state.volumes[index];
                let mut actions = Vec::new();
                let target = volume.id == state.target.id && state.target.available;
                if volume.supported && !target {
                    actions.push(DriveAction::Use {
                        id: volume.id.clone(),
                        name: volume_name(volume),
                    });
                }
                if volume.id != api::INTERNAL {
                    if volume.mounted.is_some() {
                        actions.push(DriveAction::Eject {
                            id: volume.id.clone(),
                            name: volume_name(volume),
                        });
                    }
                    if !state
                        .disks
                        .iter()
                        .any(|d| d.disk == volume.disk && d.system)
                    {
                        actions.push(format(&volume.disk));
                    }
                }
                actions
            }
            Self::Blank(index) => vec![format(&state.disks[index].disk)],
        }
    }

    fn line(self, state: &StorageState, selected: bool) -> Line<'static> {
        let (label, drive, fs, size, free, status, colour) = match self {
            Self::Volume(index) => {
                let volume = &state.volumes[index];
                let (status, colour) = if self.is_recording(state) {
                    (
                        "Recording here",
                        if state.falling_back() { FOCUS } else { GOOD },
                    )
                } else if volume.ejected {
                    ("ejected", DIM)
                } else if !volume.supported {
                    ("unsupported", DIM)
                } else if volume.id == state.target.id {
                    // Chosen, plugged in, but not mounted to write.
                    ("not mounted", BAD)
                } else if volume.mounted.is_some() {
                    ("ready", Color::Reset)
                } else {
                    ("not mounted", FOCUS)
                };
                (
                    if volume.id != api::INTERNAL && volume.label.is_empty() {
                        "(no name)".to_string()
                    } else {
                        volume_name(volume)
                    },
                    match &volume.model {
                        Some(model) => format!("{} {model}", volume.device),
                        None => volume.device.clone(),
                    },
                    fs_name(volume.fs.as_deref()),
                    system::megabytes(volume.size_bytes),
                    volume
                        .free_bytes
                        .map(system::megabytes)
                        .unwrap_or_else(|| "-".into()),
                    status,
                    colour,
                )
            }
            Self::Blank(index) => {
                let disk = &state.disks[index];
                (
                    "(no volume)".to_string(),
                    match &disk.model {
                        Some(model) => format!("{} {model}", disk.disk),
                        None => disk.disk.clone(),
                    },
                    "-".into(),
                    system::megabytes(disk.size_bytes),
                    "-".into(),
                    "blank",
                    DIM,
                )
            }
        };
        let text = format!(
            "{}{:<12} {:<19} {:<7}{:>8}{:>8}  ",
            if selected { "> " } else { "  " },
            fit(&label, 12),
            fit(&drive, 19),
            fit(&fs, 7),
            size,
            free
        );
        if selected {
            let style = Style::new().fg(Color::Black).bg(FOCUS);
            Line::from(vec![
                Span::styled(text, style),
                Span::styled(status, style.bold()),
            ])
        } else {
            Line::from(vec![
                Span::raw(text),
                Span::styled(status, Style::new().fg(colour)),
            ])
        }
    }

    /// What to know about the selected line.
    fn details(self, state: &StorageState) -> String {
        match self {
            Self::Volume(index) => {
                let volume = &state.volumes[index];
                let place = match (&volume.mount_path, volume.mounted) {
                    (Some(path), Some(Mount::Rw)) => format!("mounted at {path}"),
                    (Some(path), _) => format!("mounted read-only at {path}"),
                    _ => "not mounted".to_string(),
                };
                let device = format!("/dev/{}, {place}", volume.device);
                if volume.ejected {
                    format!("{device}. Ejected: unplug it, or Record here to use it again.")
                } else if !volume.supported {
                    format!(
                        "{device}. LiveStage records to exFAT, FAT32, NTFS and ext4; \
                         Format disk... makes it exFAT."
                    )
                } else if volume.fs.as_deref() == Some("vfat") {
                    format!("{device}. FAT32 holds no file over 4 GB: long takes are cut.")
                } else {
                    format!("{device}.")
                }
            }
            Self::Blank(_) => "No volume on it: Format disk... makes it one exFAT volume.".into(),
        }
    }
}

impl StorageScreen {
    fn draw(&self, frame: &mut Frame, area: Rect, state: &StorageState) {
        let block = Block::bordered()
            .title(" Drives ")
            .border_style(DIM)
            .padding(ratatui::widgets::Padding::horizontal(1));
        let outer = area.inner(Margin::new(1, 0));
        let inner = block.inner(outer);
        frame.render_widget(block, outer);
        let dim = Style::new().fg(DIM);
        let mut lines = Vec::new();
        let target = &state.target;
        if state.falling_back() {
            lines.push(Line::styled(
                format!(
                    "{}: recordings go to the internal storage for now.",
                    fallback_reason(state)
                ),
                Style::new().fg(FOCUS).bold(),
            ));
        } else {
            let name = match state.volume(&target.id) {
                Some(volume) => volume_name(volume),
                None => target.label.clone(),
            };
            lines.push(Line::from(vec![
                Span::raw("Recordings go to "),
                Span::styled(name, Style::new().bold()),
            ]));
        }
        lines.push(Line::styled(format!("  {}", target.recordings_dir), dim));
        lines.push(Line::raw(""));
        lines.push(drive_header());
        let rows = drive_rows(state);
        let selected = self.selected.min(rows.len().saturating_sub(1));
        for (index, row) in rows.iter().enumerate() {
            lines.push(row.line(state, index == selected));
        }
        lines.push(Line::raw(""));
        if let Some(action) = &self.pending {
            lines.push(Line::styled(
                action.working(),
                Style::new().fg(FOCUS).bold(),
            ));
        } else if let Some(row) = rows.get(selected) {
            lines.push(Line::styled(row.details(state), dim));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

enum FormatOutcome {
    Stay,
    Cancel,
    Format(String),
}

/// Erasing a disk asks for its new name and for FORMAT typed out.
struct FormatDialog {
    disk: String,
    what: String,
    label: TextInput,
    confirm: TextInput,
    /// 0 the name, 1 the confirmation.
    focus: usize,
    error: Option<String>,
}

impl FormatDialog {
    fn new(disk: &str, what: &str) -> Self {
        Self {
            disk: disk.to_string(),
            what: what.to_string(),
            label: TextInput::new(api::DEFAULT_LABEL),
            confirm: TextInput::default(),
            focus: 1,
            error: None,
        }
    }

    fn key(&mut self, key: KeyEvent) -> FormatOutcome {
        match key.code {
            KeyCode::Esc => return FormatOutcome::Cancel,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => self.focus ^= 1,
            KeyCode::Enter if self.focus == 0 => self.focus = 1,
            KeyCode::Enter => {
                let label = self.label.value.trim().to_string();
                if !self.confirm.value.trim().eq_ignore_ascii_case("FORMAT") {
                    self.error = Some("Type FORMAT to erase the disk.".into());
                } else if let Err(message) = api::valid_label(&label) {
                    self.error = Some(message.into());
                    self.focus = 0;
                } else {
                    return FormatOutcome::Format(label);
                }
            }
            _ => {
                let field = if self.focus == 0 {
                    &mut self.label
                } else {
                    &mut self.confirm
                };
                if field.key(key) {
                    self.error = None;
                }
            }
        }
        FormatOutcome::Stay
    }

    fn draw(&self, frame: &mut Frame, area: Rect) {
        let rect = centered(area, 66, 12);
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .title(" Format disk ")
            .border_style(Style::new().fg(BAD));
        let inner = block.inner(rect).inner(Margin::new(1, 0));
        frame.render_widget(block, rect);
        let text = format!(
            "Everything on {} is erased: every partition and every file on it. \
             It becomes one exFAT volume, which Windows and Mac computers read.",
            self.what
        );
        let text_height = wrapped_height(&text, inner.width);
        let [text_area, _, name_area, confirm_area, _, error_area] = Layout::vertical([
            Constraint::Length(text_height),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), text_area);
        for (index, (label, field, hint, area)) in [
            ("Name", &self.label, "up to 11 characters", name_area),
            ("Type FORMAT", &self.confirm, "to confirm", confirm_area),
        ]
        .into_iter()
        .enumerate()
        {
            let focused = index == self.focus;
            let line = Line::from(vec![
                Span::styled(
                    format!("{label:<width$}", width = LABEL_WIDTH as usize),
                    if focused {
                        Style::new().fg(FOCUS).bold()
                    } else {
                        Style::new().fg(ACCENT)
                    },
                ),
                Span::styled(
                    format!("{:<16}", field.shown()),
                    if focused {
                        Style::new().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::new().add_modifier(Modifier::UNDERLINED)
                    },
                ),
                Span::styled(format!("  {hint}"), Style::new().fg(DIM)),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            if focused {
                let x = area.x + LABEL_WIDTH + field.cursor as u16;
                if x < area.right() {
                    frame.set_cursor_position((x, area.y));
                }
            }
        }
        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .fg(BAD)
                    .wrap(Wrap { trim: true }),
                error_area,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn tree() -> (System, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "livestage-console-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
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
        put(
            "/proc/asound/cards",
            " 0 [Intel          ]: HDA-Intel - HDA Intel\n",
        );
        put(
            "/proc/asound/pcm",
            "00-00: Generic Analog : Generic Analog : playback 1 : capture 1\n",
        );
        put(
            "/usr/share/zoneinfo/zone1970.tab",
            "TH,KH,LA,VN\t+1345+10031\tAsia/Bangkok\tIndochina\nGB\t+513030-0000731\tEurope/London\n",
        );
        put("/usr/share/zoneinfo/Asia/Bangkok", "TZif");
        put("/usr/share/zoneinfo/UTC", "TZif");
        put("/etc/shadow", "root::0:::::\n");
        (system, root)
    }

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 25)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn typing(app: &mut App, text: &str) {
        for c in text.chars() {
            app.key(press(KeyCode::Char(c)));
        }
    }

    /// Walks the whole first setup with a fixed address and prints every
    /// page (`cargo test ... -- --nocapture` to look at them).
    #[test]
    fn the_first_setup_walks_through_and_applies() {
        let (system, root) = tree();
        let mut app = App::new(system, true, false);
        let mut pages = vec![screen(&app)];
        assert!(pages[0].contains("Welcome"));

        app.key(press(KeyCode::Enter)); // Set up
        // Name: replace it.
        for _ in 0..9 {
            app.key(press(KeyCode::Backspace));
        }
        typing(&mut app, "FOH-Rack");
        pages.push(screen(&app));
        app.key(press(KeyCode::Enter)); // to Back
        app.key(press(KeyCode::Down)); // to Next
        app.key(press(KeyCode::Enter));

        // Network: eth0, fixed.
        app.key(press(KeyCode::Right)); // port: eth0
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Right)); // fixed
        app.key(press(KeyCode::Down));
        typing(&mut app, "192.168.10.20/24");
        app.key(press(KeyCode::Down));
        typing(&mut app, "192.168.20.1"); // not on the network: refused
        pages.push(screen(&app));
        app.key(press(KeyCode::Down)); // DNS
        app.key(press(KeyCode::Down)); // Back
        app.key(press(KeyCode::Down)); // Next
        app.key(press(KeyCode::Enter));
        let refused = screen(&app);
        assert!(refused.contains("not on the same network"), "{refused}");
        // Focus went to the gateway: fix it.
        for _ in 0..12 {
            app.key(press(KeyCode::Backspace));
        }
        typing(&mut app, "192.168.10.1");
        for _ in 0..3 {
            app.key(press(KeyCode::Down));
        }
        app.key(press(KeyCode::Enter));

        // Audio: the card for output; input follows.
        app.key(press(KeyCode::Right));
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Right)); // 44.1 kHz
        app.key(press(KeyCode::Right)); // 48 kHz
        pages.push(screen(&app));
        for _ in 0..3 {
            app.key(press(KeyCode::Down));
        }
        app.key(press(KeyCode::Enter));

        // Storage: as it is.
        pages.push(screen(&app));
        assert!(pages.last().unwrap().contains("Drives..."));
        app.key(press(KeyCode::BackTab));
        app.key(press(KeyCode::Enter));

        // Web UI: as it is.
        pages.push(screen(&app));
        app.key(press(KeyCode::BackTab));
        app.key(press(KeyCode::Enter));

        // Time zone, through the list.
        app.key(press(KeyCode::Enter));
        typing(&mut app, "bang");
        pages.push(screen(&app));
        app.key(press(KeyCode::Enter));
        app.key(press(KeyCode::BackTab));
        app.key(press(KeyCode::Enter));

        // Password: none.
        pages.push(screen(&app));
        app.key(press(KeyCode::BackTab));
        app.key(press(KeyCode::Enter));

        // Review, then apply.
        pages.push(screen(&app));
        app.key(press(KeyCode::Enter));
        while app.busy() {
            app.tick();
        }
        pages.push(screen(&app));
        for page in &pages {
            println!("{page}");
        }

        let config = app.system.load_config().expect("saved");
        assert_eq!(config.name, "foh-rack");
        assert_eq!(config.interface.as_deref(), Some("eth0"));
        let fixed = config.fixed.expect("a fixed address");
        assert_eq!(fixed.address.to_string(), "192.168.10.20");
        assert_eq!(
            fixed.gateway.map(|g| g.to_string()).as_deref(),
            Some("192.168.10.1")
        );
        assert_eq!(config.audio.output.as_deref(), Some("hw:CARD=Intel,DEV=0"));
        assert_eq!(config.audio.input.as_deref(), Some("hw:CARD=Intel,DEV=0"));
        assert_eq!(config.audio.rate, 48_000);
        assert_eq!(config.timezone, "Asia/Bangkok");
        let interfaces =
            std::fs::read_to_string(app.system.path("/run/network/interfaces")).unwrap();
        assert!(
            interfaces.contains("address 192.168.10.20/24"),
            "{interfaces}"
        );
        assert!(pages.last().unwrap().contains("Done."));

        // Finish: the home screen.
        app.key(press(KeyCode::Enter));
        let home = screen(&app);
        println!("{home}");
        assert!(home.contains("This machine"));
        assert!(home.contains("Asia/Bangkok"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_wifi_port_asks_for_the_network_and_keeps_only_its_key() {
        let (system, root) = tree();
        let put = |path: &str, text: &str| {
            let path = system.path(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        put(
            "/sys/class/net/wlan0/address",
            "02:00:00:00:00:00
",
        );
        put(
            "/sys/class/net/wlan0/type",
            "1
",
        );
        put("/sys/class/net/wlan0/device/uevent", "");
        put(
            "/sys/class/net/wlan0/phy80211/name",
            "phy0
",
        );
        system.save_config(&SetupConfig::default()).unwrap();
        let mut app = App::new(system, false, true);
        app.key(press(KeyCode::Up)); // Name: to Next
        app.key(press(KeyCode::Enter));

        // Network: every wired, eth0, wlan0.
        app.key(press(KeyCode::Right));
        app.key(press(KeyCode::Right));
        app.key(press(KeyCode::Down)); // Wi-Fi name
        typing(&mut app, "Front of house");
        app.key(press(KeyCode::Down)); // Scan
        app.key(press(KeyCode::Enter));
        assert!(app.busy());
        app.tick(); // the scan (none under --root)
        app.key(press(KeyCode::Down)); // Wi-Fi key
        typing(&mut app, "short");
        let page = screen(&app);
        println!("{page}");
        assert!(page.contains("Scan for networks"), "{page}");
        assert!(page.contains("none in range"), "{page}");
        for _ in 0..3 {
            app.key(press(KeyCode::Down)); // Address, Back, Next
        }
        app.key(press(KeyCode::Enter));
        assert!(screen(&app).contains("8 to 63"));
        for _ in 0..5 {
            app.key(press(KeyCode::Backspace));
        }
        typing(&mut app, "loud and clear");
        for _ in 0..3 {
            app.key(press(KeyCode::Down));
        }
        app.key(press(KeyCode::Enter)); // to Audio
        for _ in 0..5 {
            app.key(press(KeyCode::Up)); // Next, on each page
            app.key(press(KeyCode::Enter));
        }
        let review = screen(&app);
        println!("{review}");
        assert!(
            review.contains("wlan0 (Wi-Fi \"Front of house\"): DHCP"),
            "{review}"
        );
        app.key(press(KeyCode::Enter));
        while app.busy() {
            app.tick();
        }

        let config = app.system.load_config().unwrap();
        let wifi = config.wifi.expect("Wi-Fi saved");
        assert_eq!(wifi.ssid, "Front of house");
        assert_eq!(
            wifi.psk.as_deref(),
            Some(config::wifi_psk("Front of house", "loud and clear").as_str())
        );
        let saved = std::fs::read_to_string(app.system.path(system::SETUP_CONF)).unwrap();
        assert!(!saved.contains("loud and clear"));
        let wpa =
            std::fs::read_to_string(app.system.path("/run/wpa_supplicant/wlan0.conf")).unwrap();
        assert!(wpa.contains("psk="), "{wpa}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn using_the_defaults_saves_them() {
        let (system, root) = tree();
        let mut app = App::new(system, true, false);
        app.key(press(KeyCode::Right)); // Use the defaults
        app.key(press(KeyCode::Enter));
        while app.busy() {
            app.tick();
        }
        assert_eq!(app.system.load_config(), Some(SetupConfig::default()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_short_password_is_refused() {
        let (system, root) = tree();
        // Set up before: the setup opens on the name, not the welcome.
        system.save_config(&SetupConfig::default()).unwrap();
        let mut app = App::new(system, false, true);
        // Name, Network, Audio, Storage, Web, Time: Next on each.
        for _ in 0..6 {
            app.key(press(KeyCode::BackTab));
            app.key(press(KeyCode::Enter));
        }
        typing(&mut app, "abc");
        app.key(press(KeyCode::Down));
        typing(&mut app, "abc");
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Enter));
        let text = screen(&app);
        assert!(text.contains("at least 6"), "{text}");
        assert!(text.contains("***"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The drives page from the home screen: the list, Record here, the
    /// format dialog (printed with `--nocapture`).
    #[test]
    fn the_drives_page_records_ejects_and_formats() {
        let (system, root) = crate::storage::tests::machine("console");
        system.save_config(&SetupConfig::default()).unwrap();
        let mut app = App::new(system, false, false);
        let home = screen(&app);
        println!("{home}");
        assert!(home.contains("[Storage]"), "{home}");
        assert!(home.contains("Recordings    Internal:"), "{home}");

        app.key(press(KeyCode::Right)); // Storage
        app.key(press(KeyCode::Enter));
        let page = screen(&app);
        println!("{page}");
        assert!(page.contains("Recordings go to Internal"), "{page}");
        assert!(page.contains("sdb1 SanDisk Ultra"), "{page}");
        assert!(page.contains("Recording here"), "{page}");
        assert!(page.contains("(no volume)"), "{page}");
        assert!(page.contains("unsupported"), "{page}");

        // The stick: Record here.
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Enter));
        let menu = screen(&app);
        println!("{menu}");
        assert!(
            menu.contains("Record here") && menu.contains("Eject"),
            "{menu}"
        );
        app.key(press(KeyCode::Enter));
        assert!(app.busy());
        println!("{}", screen(&app));
        app.tick();
        assert_eq!(
            app.system.load_config().unwrap().record_storage,
            "1A2B-3C4D"
        );
        // Nothing is mounted under --root: honest about it.
        let fallback = screen(&app);
        println!("{fallback}");
        assert!(fallback.contains("cannot be written to"), "{fallback}");
        // As the service leaves it: read-write.
        let mounts = app.system.path("/proc/mounts");
        let text = std::fs::read_to_string(&mounts)
            .unwrap()
            .replace("/media/1A2B-3C4D exfat ro", "/media/1A2B-3C4D exfat rw");
        std::fs::write(&mounts, text).unwrap();
        app.refresh();
        let recording = screen(&app);
        println!("{recording}");
        assert!(
            recording.contains("Recordings go to SHOW \"A\""),
            "{recording}"
        );
        assert!(
            recording.contains("/media/1A2B-3C4D/LiveStage Recordings"),
            "{recording}"
        );

        // The blank disk: Format, typed out.
        for _ in 0..10 {
            app.key(press(KeyCode::Down));
        }
        app.key(press(KeyCode::Enter));
        app.key(press(KeyCode::Enter)); // Format disk...
        typing(&mut app, "yes");
        app.key(press(KeyCode::Enter));
        let dialog = screen(&app);
        println!("{dialog}");
        assert!(
            dialog.contains("Type FORMAT to erase the disk."),
            "{dialog}"
        );
        assert!(dialog.contains("sdf, Blank (16 GB)"), "{dialog}");
        for _ in 0..3 {
            app.key(press(KeyCode::Backspace));
        }
        typing(&mut app, "FORMAT");
        app.key(press(KeyCode::Enter));
        assert!(app.busy());
        let working = screen(&app);
        println!("{working}");
        assert!(working.contains("Formatting sdf"), "{working}");
        app.tick();
        let done = screen(&app);
        println!("{done}");
        assert!(done.contains("one exFAT volume, LIVESTAGE"), "{done}");
        app.key(press(KeyCode::Enter)); // the message
        app.key(press(KeyCode::Esc)); // home
        let home = screen(&app);
        assert!(home.contains("Recordings    SHOW"), "{home}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The setup's Storage page opens the drives over the setup, and Esc
    /// comes back to the same page.
    #[test]
    fn the_setup_opens_the_drives_and_comes_back() {
        let (system, root) = crate::storage::tests::machine("console-setup");
        system.save_config(&SetupConfig::default()).unwrap();
        let mut app = App::new(system, false, true);
        // Name, Network, Audio: Next on each.
        for _ in 0..3 {
            app.key(press(KeyCode::BackTab));
            app.key(press(KeyCode::Enter));
        }
        let page = screen(&app);
        assert!(page.contains("setup  -  Storage"), "{page}");
        assert!(page.contains("SHOW \"A\""), "{page}");
        app.key(press(KeyCode::Enter)); // Drives...
        let drives = screen(&app);
        assert!(
            drives.contains("setup  -  Storage") && drives.contains("Drives"),
            "{drives}"
        );
        assert!(drives.contains("Esc back"), "{drives}");
        app.key(press(KeyCode::Esc));
        let back = screen(&app);
        assert!(back.contains("[ Drives... ]"), "{back}");
        assert!(back.contains("[ Next ]"), "{back}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The system disk and the internal volume offer nothing destructive.
    #[test]
    fn the_system_disk_cannot_be_formatted_from_the_console() {
        let (system, root) = crate::storage::tests::machine("console-system");
        let state = Storage::new().state(&system);
        let rows = drive_rows(&state);
        let internal = rows[0].actions(&state);
        assert!(internal.is_empty(), "{internal:?}");
        assert!(rows.iter().all(|row| {
            row.actions(&state)
                .iter()
                .all(|action| !matches!(action, DriveAction::Format { disk, .. } if disk == "sda"))
        }));
        let _ = std::fs::remove_dir_all(&root);
    }
}
