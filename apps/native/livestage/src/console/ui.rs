//! The console's screens: the status of the machine (home), the setup
//! wizard ([`super::wizard`]), the progress of applying it, and the drives
//! recordings go to.
//!
//! Drawn for the Linux text console: 80×25 at the least, its sixteen colours,
//! and no glyphs beyond the box-drawing set its font has.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::config::{AudioChoice, SetupConfig};
use super::drives::{
    DriveRow, drive_header, drive_rows, fallback_reason, recordings_summary, volume_name,
};
use super::storage::Storage;
use super::storage_api::{self as api, State as StorageState};
use super::system::{self, NetPort, PasswordState, ServiceState, System};
use super::widgets::{
    ACCENT, BAD, DIM, FOCUS, GOOD, LABEL_WIDTH, TextInput, button_row, cable_span, centered,
    compact_button_row, draw_confirm, draw_message, wrapped_height,
};
use super::wizard::{Picker, Scan, Wizard, WizardOutcome};

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
            WizardOutcome::Apply { defaults } => {
                if let Screen::Wizard(wizard) = &self.screen {
                    let applying = applying(wizard, defaults);
                    self.screen = Screen::Applying(Box::new(applying));
                }
            }
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

fn draw_popup(frame: &mut Frame, area: Rect, popup: &Popup) {
    match popup {
        Popup::Confirm {
            title, text, yes, ..
        } => draw_confirm(frame, area, title, text, *yes),
        Popup::Message { title, text } => draw_message(frame, area, title, text),
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

/// What applying the setup's answers on this machine takes: only the steps
/// for what changed (all of them the first time).
fn applying(wizard: &Wizard, defaults: bool) -> Applying {
    let (config, password) = wizard.answers(defaults);
    let audio = config.audio.clone();
    let original = &wizard.original;
    let first = wizard.first;
    let network_changed = first
        || config.interface != original.interface
        || config.fixed != original.fixed
        || config.wifi != original.wifi;
    let name_changed =
        first || config.name != original.name || config.timezone != original.timezone;
    let audio_changed = audio != wizard.original_audio;
    let mixer_changed = first
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
        fresh_session: !wizard.session_exists,
        password,
        steps: steps.into_iter().map(|s| (s, StepState::Waiting)).collect(),
        next: 0,
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

impl DriveRow {
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
    use crate::config;
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
