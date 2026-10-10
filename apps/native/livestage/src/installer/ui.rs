//! The installer's screens, in the console's look (its widgets and colours):
//!
//! ```txt
//! Welcome -> Disk -> Settings -> (the setup's pages) -> Confirm -> Installing -> Done
//! ```
//!
//! Esc goes back a step everywhere before the disk is written; the confirm
//! step wants the disk's name typed out. Installing runs on its own thread
//! ([`install::spawn`]); the screen follows it through a channel.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Gauge, Paragraph, Wrap};

use super::config::SetupConfig;
use super::disks::{self, Disk, SPARE_BYTES};
use super::install::{self, Event, Failure, Job, Report, Stage};
use super::payload::Payload;
use super::system::{self, System};
use super::widgets::{
    ACCENT, BAD, DIM, FOCUS, GOOD, LABEL_WIDTH, TextInput, button_row, compact_button_row,
    draw_confirm, draw_message,
};
use super::wizard::{Picker, Scan, Wizard, WizardOutcome};

/// How often the disk list is read again (disks come and go).
const REFRESH: Duration = Duration::from_secs(2);

pub struct App {
    system: System,
    payload: Result<Payload, String>,
    /// On tty1: never quits (init would only start it again).
    console: bool,
    screen: Screen,
    popup: Option<Popup>,
    disks: Vec<Disk>,
    refreshed: Instant,
    /// The disk chosen on the disk screen.
    chosen: Option<Disk>,
    /// "Set up now": the setup's pages, kept so that going back finds the
    /// answers as they were.
    wizard: Option<Box<Wizard>>,
    /// The settings and console password for the new disk; `None` leaves
    /// the setup to its first boot.
    setup: Option<(SetupConfig, Option<String>)>,
    progress: Option<Progress>,
    pub quit: bool,
    /// Ask the terminal for a full redraw.
    pub redraw: bool,
    /// Ask the main loop for a shell on this terminal.
    pub shell: bool,
}

enum Screen {
    Welcome { focus: usize },
    Disks { selected: usize },
    Settings { focus: usize },
    Wizard,
    Confirm(ConfirmScreen),
    Installing,
    Done { focus: usize },
}

struct ConfirmScreen {
    typed: TextInput,
    /// 0 the name, 1 Back, 2 Install.
    focus: usize,
    error: Option<String>,
}

enum Popup {
    Picker(Picker),
    Confirm {
        title: &'static str,
        text: String,
        action: Action,
        yes: bool,
    },
    Message {
        title: &'static str,
        text: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Install,
    Disks,
    Shell,
    Reboot,
    PowerOff,
    Quit,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::Install => "Install",
            Self::Disks => "Back to the disks",
            Self::Shell => "Shell",
            Self::Reboot => "Reboot",
            Self::PowerOff => "Power off",
            Self::Quit => "Quit",
        }
    }
}

/// The settings screen's choices.
const SETTINGS: [(&str, &str); 3] = [
    (
        "Set up now",
        "Answer them here: the first boot starts LiveStage straight away.",
    ),
    (
        "On first boot",
        "The first boot asks them on its screen (until then: the defaults).",
    ),
    ("Back", ""),
];

/// The install as it goes.
struct Progress {
    disk: Disk,
    stages: Vec<Stage>,
    current: usize,
    done: u64,
    total: u64,
    /// When the current stage began, for its speed.
    since: Instant,
    receiver: Receiver<Event>,
    result: Option<Result<Report, Failure>>,
    /// For the last screen: where the web UI will be.
    setup: Option<SetupConfig>,
}

impl Progress {
    fn stage(&self) -> Stage {
        self.stages[self.current.min(self.stages.len() - 1)]
    }

    /// `512 of 1251 MB, 85 MB/s, 0:09 left`.
    fn rate_text(&self) -> String {
        let elapsed = self.since.elapsed().as_secs_f64();
        let mut text = format!("{} of {} MB", self.done / 1_000_000, self.total / 1_000_000);
        if elapsed >= 1.0 && self.done > 0 {
            let rate = self.done as f64 / elapsed;
            text.push_str(&format!(", {:.0} MB/s", rate / 1e6));
            let left = (self.total.saturating_sub(self.done)) as f64 / rate;
            text.push_str(&format!(
                ", {}:{:02} left",
                left as u64 / 60,
                left as u64 % 60
            ));
        }
        text
    }
}

/// The disk's name typed on the confirm screen: `vda`, or `/dev/vda`.
pub fn confirms(typed: &str, disk: &str) -> bool {
    let typed = typed.trim();
    !disk.is_empty() && (typed == disk || typed.strip_prefix("/dev/") == Some(disk))
}

impl App {
    pub fn new(system: System, payload: Result<Payload, String>, console: bool) -> Self {
        Self {
            system,
            payload,
            console,
            screen: Screen::Welcome { focus: 0 },
            popup: None,
            disks: Vec::new(),
            refreshed: Instant::now(),
            chosen: None,
            wizard: None,
            setup: None,
            progress: None,
            quit: false,
            redraw: false,
            shell: false,
        }
    }

    fn min_bytes(&self) -> u64 {
        self.payload.as_ref().map(|p| p.bytes).unwrap_or(0) + SPARE_BYTES
    }

    fn refresh_disks(&mut self) {
        let selected_name = match &self.screen {
            Screen::Disks { selected } => self.disks.get(*selected).map(|d| d.name.clone()),
            _ => None,
        };
        self.disks = disks::discover(&self.system, self.min_bytes());
        self.refreshed = Instant::now();
        if let (Screen::Disks { selected }, Some(name)) = (&mut self.screen, selected_name) {
            *selected = self.disks.iter().position(|d| d.name == name).unwrap_or(0);
        }
    }

    fn welcome_actions(&self) -> Vec<Action> {
        let mut actions = vec![
            Action::Install,
            Action::Shell,
            Action::Reboot,
            Action::PowerOff,
        ];
        if !self.console {
            actions.push(Action::Quit);
        }
        actions
    }

    fn done_actions(&self) -> Vec<Action> {
        let worked = self
            .progress
            .as_ref()
            .is_some_and(|p| matches!(p.result, Some(Ok(_))));
        let mut actions = if worked {
            vec![Action::Reboot, Action::PowerOff, Action::Shell]
        } else {
            vec![
                Action::Disks,
                Action::Shell,
                Action::Reboot,
                Action::PowerOff,
            ]
        };
        if !self.console {
            actions.push(Action::Quit);
        }
        actions
    }

    /// Work between key presses: a Wi-Fi scan, the install's news, the
    /// disk list.
    pub fn tick(&mut self) {
        if let (Screen::Wizard, Some(wizard)) = (&self.screen, &mut self.wizard)
            && wizard.scan == Scan::Pending
        {
            if let Some(picker) = wizard.run_scan(&self.system) {
                self.popup = Some(Popup::Picker(picker));
            }
            return;
        }
        if let Some(progress) = &mut self.progress
            && progress.result.is_none()
        {
            loop {
                match progress.receiver.try_recv() {
                    Ok(Event::Stage(stage)) => {
                        if let Some(index) = progress.stages.iter().position(|s| *s == stage) {
                            progress.current = index;
                        }
                        progress.done = 0;
                        progress.since = Instant::now();
                    }
                    Ok(Event::Progress { done, total }) => {
                        progress.done = done;
                        progress.total = total;
                    }
                    Ok(Event::Finished(result)) => {
                        progress.result = Some(result);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        progress.result = Some(Err(Failure {
                            stage: progress.stage(),
                            error: "The installer's worker stopped without a word.".into(),
                            state: "What is on the disk is unknown: install again.".into(),
                        }));
                        break;
                    }
                }
            }
            if progress.result.is_some() {
                self.screen = Screen::Done { focus: 0 };
            }
            return;
        }
        if matches!(self.screen, Screen::Disks { .. }) && self.refreshed.elapsed() >= REFRESH {
            self.refresh_disks();
        }
    }

    /// Whether [`App::tick`] has more to do straight away.
    pub fn busy(&self) -> bool {
        matches!(self.screen, Screen::Wizard)
            && self
                .wizard
                .as_ref()
                .is_some_and(|w| w.scan == Scan::Pending)
    }

    /// Installing (keys wait).
    pub fn installing(&self) -> bool {
        matches!(self.screen, Screen::Installing)
    }

    // ── Keys ────────────────────────────────────────────────────────────────

    pub fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('l') {
            self.redraw = true;
            return;
        }
        if self.installing() {
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
        let welcome = self.welcome_actions();
        let done = self.done_actions();
        match &mut self.screen {
            Screen::Welcome { focus } => {
                if let Some(index) = button_key(focus, welcome.len(), key) {
                    self.act(welcome[index]);
                }
            }
            Screen::Done { focus } => {
                if let Some(index) = button_key(focus, done.len(), key) {
                    self.act(done[index]);
                }
            }
            Screen::Disks { selected } => {
                let last = self.disks.len().saturating_sub(1);
                match key.code {
                    KeyCode::Up | KeyCode::BackTab => *selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Tab => *selected = (*selected + 1).min(last),
                    KeyCode::Home => *selected = 0,
                    KeyCode::End => *selected = last,
                    KeyCode::Esc | KeyCode::Left => self.screen = Screen::Welcome { focus: 0 },
                    KeyCode::Char('r') | KeyCode::F(5) => self.refresh_disks(),
                    KeyCode::Enter | KeyCode::Right => {
                        let Some(disk) = self.disks.get(*selected).cloned() else {
                            return;
                        };
                        match &disk.problem {
                            Some(problem) => {
                                self.popup = Some(Popup::Message {
                                    title: "This disk cannot be used",
                                    text: problem.clone(),
                                })
                            }
                            None => {
                                self.chosen = Some(disk);
                                self.screen = Screen::Settings { focus: 0 };
                            }
                        }
                    }
                    _ => {}
                }
            }
            Screen::Settings { focus } => match key.code {
                KeyCode::Up | KeyCode::BackTab | KeyCode::Left => *focus = focus.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab | KeyCode::Right => {
                    *focus = (*focus + 1).min(SETTINGS.len() - 1)
                }
                KeyCode::Esc => self.back_to_disks(),
                KeyCode::Enter => match *focus {
                    0 => {
                        if self.wizard.is_none() {
                            self.wizard = Some(Box::new(Wizard::for_install(&self.system)));
                        }
                        self.screen = Screen::Wizard;
                    }
                    1 => {
                        self.setup = None;
                        self.open_confirm();
                    }
                    _ => self.back_to_disks(),
                },
                _ => {}
            },
            Screen::Wizard => {
                let Some(wizard) = &mut self.wizard else {
                    self.screen = Screen::Settings { focus: 0 };
                    return;
                };
                match wizard.key(key) {
                    WizardOutcome::Stay | WizardOutcome::Storage => {}
                    WizardOutcome::Leave => self.screen = Screen::Settings { focus: 0 },
                    WizardOutcome::Pick(picker) => self.popup = Some(Popup::Picker(picker)),
                    WizardOutcome::Apply { defaults } => {
                        self.setup = Some(wizard.answers(defaults));
                        self.open_confirm();
                    }
                }
            }
            Screen::Confirm(confirm) => {
                let name = self
                    .chosen
                    .as_ref()
                    .map(|d| d.name.clone())
                    .unwrap_or_default();
                match key.code {
                    KeyCode::Esc => self.back_from_confirm(),
                    KeyCode::Tab | KeyCode::Down => confirm.focus = (confirm.focus + 1) % 3,
                    KeyCode::BackTab | KeyCode::Up => confirm.focus = (confirm.focus + 2) % 3,
                    KeyCode::Left if confirm.focus > 0 => confirm.focus = 1,
                    KeyCode::Right if confirm.focus > 0 => confirm.focus = 2,
                    KeyCode::Enter if confirm.focus == 1 => self.back_from_confirm(),
                    KeyCode::Enter => {
                        if confirms(&confirm.typed.value, &name) {
                            self.start();
                        } else {
                            confirm.error = Some(format!(
                                "Type {name} (the disk's name) exactly, to erase it and install."
                            ));
                            confirm.focus = 0;
                        }
                    }
                    _ if confirm.focus == 0 => {
                        if confirm.typed.key(key) {
                            confirm.error = None;
                        }
                    }
                    _ => {}
                }
            }
            Screen::Installing => {}
        }
    }

    fn back_to_disks(&mut self) {
        let selected = self
            .chosen
            .as_ref()
            .and_then(|c| self.disks.iter().position(|d| d.name == c.name))
            .unwrap_or(0);
        self.screen = Screen::Disks { selected };
        self.refresh_disks();
    }

    fn back_from_confirm(&mut self) {
        self.screen = if self.setup.is_some() && self.wizard.is_some() {
            Screen::Wizard
        } else {
            Screen::Settings {
                focus: usize::from(self.setup.is_none()),
            }
        };
    }

    fn open_confirm(&mut self) {
        self.screen = Screen::Confirm(ConfirmScreen {
            typed: TextInput::default(),
            focus: 0,
            error: None,
        });
    }

    /// The disk name was typed: off it goes.
    fn start(&mut self) {
        let (Some(disk), Ok(payload)) = (self.chosen.clone(), self.payload.clone()) else {
            return;
        };
        let (setup, password) = match self.setup.clone() {
            Some((setup, password)) => (Some(setup), password),
            None => (None, None),
        };
        let job = Job {
            disk: disk.name.clone(),
            payload,
            setup: setup.clone(),
            password,
        };
        let stages = job.stages();
        let receiver = install::spawn(self.system.clone(), job);
        self.progress = Some(Progress {
            disk,
            stages,
            current: 0,
            done: 0,
            total: 0,
            since: Instant::now(),
            receiver,
            result: None,
            setup,
        });
        // The password is not kept once it is handed over.
        if let Some((_, password)) = &mut self.setup {
            *password = None;
        }
        self.screen = Screen::Installing;
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::Install => match &self.payload {
                Ok(_) => {
                    self.screen = Screen::Disks { selected: 0 };
                    self.refresh_disks();
                }
                Err(error) => {
                    self.popup = Some(Popup::Message {
                        title: "Nothing to install",
                        text: format!("The LiveStage image is missing or damaged: {error}"),
                    })
                }
            },
            Action::Disks => {
                self.progress = None;
                self.back_to_disks();
            }
            Action::Shell => self.shell = true,
            Action::Reboot => {
                if self.done_worked() {
                    self.confirmed(Action::Reboot);
                } else {
                    self.popup = Some(Popup::Confirm {
                        title: "Reboot",
                        text: "Restart the computer?".into(),
                        action,
                        yes: false,
                    });
                }
            }
            Action::PowerOff => {
                self.popup = Some(Popup::Confirm {
                    title: "Power off",
                    text: "Turn the computer off?".into(),
                    action,
                    yes: false,
                })
            }
            Action::Quit => self.quit = true,
        }
    }

    fn done_worked(&self) -> bool {
        matches!(self.screen, Screen::Done { .. })
            && self
                .progress
                .as_ref()
                .is_some_and(|p| matches!(p.result, Some(Ok(_))))
    }

    fn confirmed(&mut self, action: Action) {
        let (command, title, text) = match action {
            Action::Reboot => ("reboot", "Reboot", "Restarting..."),
            Action::PowerOff => ("poweroff", "Power off", "Shutting down..."),
            _ => return,
        };
        self.popup = Some(match self.system.run(command, &[]) {
            Ok(_) => Popup::Message {
                title,
                text: text.into(),
            },
            Err(error) => Popup::Message {
                title: "That did not work",
                text: error,
            },
        });
    }

    fn popup_key(&mut self, popup: Popup, key: KeyEvent) {
        match popup {
            Popup::Picker(mut picker) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if let (Some(choice), Some(wizard)) = (picker.chosen(), &mut self.wizard) {
                        wizard.set_choice(picker.field, &choice);
                    }
                }
                _ => {
                    picker.key(key);
                    self.popup = Some(Popup::Picker(picker));
                }
            },
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
            Popup::Message { .. } => {}
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
        let step = match &self.screen {
            Screen::Welcome { .. } => String::new(),
            Screen::Disks { .. } => "Disk".to_string(),
            Screen::Settings { .. } => "Settings".to_string(),
            Screen::Wizard => match &self.wizard {
                Some(wizard) => format!("Settings: {}", wizard.page().title()),
                None => "Settings".to_string(),
            },
            Screen::Confirm(_) => "Confirm".to_string(),
            Screen::Installing => "Installing".to_string(),
            Screen::Done { .. } => "Done".to_string(),
        };
        let title = if step.is_empty() {
            " LiveStage installer".to_string()
        } else {
            format!(" LiveStage installer  -  {step}")
        };
        let bar = Style::new().fg(Color::Black).bg(ACCENT);
        frame.render_widget(Paragraph::new(title).style(bar).bold(), header);
        let image = match &self.payload {
            Ok(payload) => format!("{} ", payload.name),
            Err(_) => "no image ".to_string(),
        };
        frame.render_widget(
            Paragraph::new(image).style(bar).right_aligned(),
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
            (Some(Popup::Confirm { .. }), _) => "Left/Right choose   Enter confirm   Esc cancel",
            (Some(Popup::Message { .. }), _) => "Any key closes",
            (None, Screen::Welcome { .. } | Screen::Done { .. }) if self.console => {
                "Left/Right choose   Enter open   Alt+F2 shell"
            }
            (None, Screen::Welcome { .. } | Screen::Done { .. }) => {
                "Left/Right choose   Enter open   Alt+F2 shell"
            }
            (None, Screen::Disks { .. }) => "Up/Down choose   Enter use it   r re-read   Esc back",
            (None, Screen::Settings { .. }) => "Up/Down choose   Enter go on   Esc back",
            (None, Screen::Wizard) => {
                "Up/Down move   Left/Right change   Enter list/next   Esc back"
            }
            (None, Screen::Confirm(_)) => "Type the name   Tab move   Enter go on   Esc back",
            (None, Screen::Installing) => "Working... do not turn the computer off",
        };
        frame.render_widget(Paragraph::new(format!(" {help}")).fg(DIM), footer);

        match &self.screen {
            Screen::Welcome { focus } => self.draw_welcome(frame, body, *focus),
            Screen::Disks { selected } => self.draw_disks(frame, body, *selected),
            Screen::Settings { focus } => self.draw_settings(frame, body, *focus),
            Screen::Wizard => {
                if let Some(wizard) = &self.wizard {
                    wizard.draw(frame, body);
                }
            }
            Screen::Confirm(confirm) => self.draw_confirm_screen(frame, body, confirm),
            Screen::Installing => self.draw_installing(frame, body),
            Screen::Done { focus } => self.draw_done(frame, body, *focus),
        }
        match &self.popup {
            Some(Popup::Picker(picker)) => picker.draw(frame, body),
            Some(Popup::Confirm {
                title, text, yes, ..
            }) => draw_confirm(frame, body, title, text, *yes),
            Some(Popup::Message { title, text }) => draw_message(frame, body, title, text),
            None => {}
        }
    }

    /// A bordered box over the body, with a row of buttons under it.
    fn framed(
        frame: &mut Frame,
        area: Rect,
        title: &str,
        border: Color,
        lines: Vec<Line<'static>>,
        buttons: Option<(&[&str], usize)>,
    ) {
        let [info, _, row] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(u16::from(buttons.is_some())),
        ])
        .areas(area.inner(Margin::new(1, 0)));
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .title(format!(" {title} "))
                    .border_style(Style::new().fg(border))
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            ),
            info,
        );
        if let Some((labels, focus)) = buttons {
            let wide: usize = 1 + labels.iter().map(|l| l.chars().count() + 6).sum::<usize>();
            frame.render_widget(
                if wide <= row.width as usize {
                    button_row(labels, Some(focus))
                } else {
                    compact_button_row(labels, Some(focus))
                },
                row,
            );
        }
    }

    fn draw_welcome(&self, frame: &mut Frame, area: Rect, focus: usize) {
        let mut lines = vec![
            Line::raw(
                "This puts LiveStage onto a disk of this computer, which then starts \
                 from it without the USB stick: a mixer you control from a browser on any \
                 phone, tablet or computer on the same network.",
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled(
                    "Everything on the disk you choose is erased",
                    Style::new().fg(FOCUS).bold(),
                ),
                Span::raw(
                    ": every partition and every file on it, Windows or Linux included. \
                     The other disks are not touched.",
                ),
            ]),
            Line::raw(""),
            Line::raw(
                "The steps: choose the disk; set LiveStage up now or at its first boot; \
                 check and confirm; install (a few minutes). Then remove the USB stick \
                 and reboot.",
            ),
            Line::raw(""),
        ];
        match &self.payload {
            Ok(payload) => lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<width$}", "Image", width = LABEL_WIDTH as usize),
                    Style::new().fg(ACCENT),
                ),
                Span::raw(format!(
                    "{}, {}",
                    payload.name,
                    system::megabytes(payload.bytes)
                )),
            ])),
            Err(error) => lines.push(Line::styled(
                format!("No LiveStage image to install: {error}"),
                Style::new().fg(BAD),
            )),
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<width$}", "Shell", width = LABEL_WIDTH as usize),
                Style::new().fg(ACCENT),
            ),
            Span::raw("Shell below, or Alt+F2 (log in as root)"),
        ]));
        let labels: Vec<&str> = self.welcome_actions().iter().map(|a| a.label()).collect();
        Self::framed(
            frame,
            area,
            "Install LiveStage",
            DIM,
            lines,
            Some((labels.as_slice(), focus)),
        );
    }

    fn draw_disks(&self, frame: &mut Frame, area: Rect, selected: usize) {
        let dim = Style::new().fg(DIM);
        // One line per disk, never wrapped: what does not fit is cut.
        let width = area.width.saturating_sub(6) as usize;
        let fit = |text: String| text.chars().take(width).collect::<String>();
        let mut lines = vec![Line::styled(
            fit(format!(
                "  {:<10} {:>8}  {:<8} {:<18} {}",
                "Disk", "Size", "Type", "Model", "On it now"
            )),
            dim,
        )];
        if self.disks.is_empty() {
            lines.push(Line::styled(
                "  No disk found. Is its controller supported (NVMe, SATA, USB, SD/eMMC, \
                 virtio)? The list follows disks plugged in.",
                Style::new().fg(FOCUS),
            ));
        }
        let selected = selected.min(self.disks.len().saturating_sub(1));
        for (index, disk) in self.disks.iter().enumerate() {
            let model: String = disk
                .model
                .clone()
                .unwrap_or_else(|| "-".into())
                .chars()
                .take(18)
                .collect();
            // Why not, first: the end of the line may be cut.
            let holds = if disk.available() {
                disk.holds_text()
            } else {
                format!("({}) {}", disk.problem_short(), disk.holds_text())
            };
            let text = fit(format!(
                "{}{:<10} {:>8}  {:<8} {:<18} {holds}",
                if index == selected { "> " } else { "  " },
                disk.name,
                system::megabytes(disk.size_bytes),
                disk.transport.label(),
                model,
            ));
            let style = if index == selected {
                Style::new().fg(Color::Black).bg(FOCUS)
            } else if disk.available() {
                Style::new()
            } else {
                dim
            };
            lines.push(Line::styled(text, style));
        }
        if let Some(disk) = self.disks.get(selected) {
            lines.push(Line::raw(""));
            lines.push(Line::styled(disk.title(), Style::new().bold()));
            if disk.parts.is_empty() && disk.whole.is_none() {
                lines.push(Line::styled("  No partitions.", dim));
            }
            for part in disk.parts.iter().chain(disk.whole.iter()).take(6) {
                lines.push(Line::styled(
                    format!("  {}", disks::describe_part(part)),
                    dim,
                ));
            }
            if disk.parts.len() > 6 {
                lines.push(Line::styled(
                    format!("  and {} more", disk.parts.len() - 6),
                    dim,
                ));
            }
            lines.push(match &disk.problem {
                Some(problem) => Line::styled(problem.clone(), Style::new().fg(BAD)),
                None => Line::styled(
                    "Enter: install LiveStage on it. Everything on it will be erased.",
                    Style::new().fg(FOCUS),
                ),
            });
        }
        Self::framed(
            frame,
            area,
            "Choose the disk to install onto",
            DIM,
            lines,
            None,
        );
    }

    fn draw_settings(&self, frame: &mut Frame, area: Rect, focus: usize) {
        let mut lines = vec![
            Line::raw(
                "LiveStage asks a few questions: the machine's name, the network (Wi-Fi \
                 too), the audio interface, the web UI, the time zone and a console \
                 password. Every one has a default.",
            ),
            Line::raw(""),
        ];
        for (index, (label, text)) in SETTINGS.iter().enumerate() {
            let style = if index == focus {
                Style::new().fg(Color::Black).bg(FOCUS).bold()
            } else {
                Style::new()
            };
            lines.push(Line::styled(format!("[ {label} ]"), style));
            if !text.is_empty() {
                lines.push(Line::styled(format!("    {text}"), Style::new().fg(DIM)));
            }
        }
        if let Some((config, _)) = &self.setup {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(
                    "Answered already: {} (Set up now shows them again).",
                    config.name
                ),
                Style::new().fg(DIM),
            ));
        }
        let title = match &self.chosen {
            Some(disk) => format!("Settings for LiveStage on {}", disk.name),
            None => "Settings".to_string(),
        };
        Self::framed(frame, area, &title, DIM, lines, None);
    }

    /// The settings in a few lines, for the confirm screen.
    fn settings_lines(&self) -> Vec<Line<'static>> {
        let label = |text: &str| {
            Span::styled(
                format!("{text:<width$}", width = LABEL_WIDTH as usize),
                Style::new().fg(ACCENT),
            )
        };
        let Some((config, password)) = &self.setup else {
            return vec![Line::from(vec![
                label("Settings"),
                Span::raw("asked on the first boot's screen"),
            ])];
        };
        let network = match (&config.interface, &config.fixed, &config.wifi) {
            (Some(port), Some(fixed), wifi) => format!(
                "{port}{}: {}/{}",
                wifi.as_ref()
                    .map(|w| format!(" (Wi-Fi \"{}\")", w.ssid))
                    .unwrap_or_default(),
                fixed.address,
                fixed.prefix
            ),
            (Some(port), None, wifi) => format!(
                "{port}{}: DHCP",
                wifi.as_ref()
                    .map(|w| format!(" (Wi-Fi \"{}\")", w.ssid))
                    .unwrap_or_default()
            ),
            (None, _, _) => "every wired port: DHCP".to_string(),
        };
        vec![
            Line::from(vec![
                label("Name"),
                Span::raw(format!(
                    "{}  (http://{}.local:{}/)",
                    config.name, config.name, config.web_port
                )),
            ]),
            Line::from(vec![label("Network"), Span::raw(network)]),
            Line::from(vec![
                label("Audio"),
                Span::raw(
                    config
                        .audio
                        .output
                        .clone()
                        .unwrap_or_else(|| "automatic".into()),
                ),
            ]),
            Line::from(vec![
                label("Web UI"),
                Span::raw(format!(
                    "{}, port {}",
                    if config.web_open {
                        "every network"
                    } else {
                        "this machine only"
                    },
                    config.web_port
                )),
            ]),
            Line::from(vec![label("Time zone"), Span::raw(config.timezone.clone())]),
            Line::from(vec![
                label("Password"),
                Span::raw(if password.is_some() { "set" } else { "none" }),
            ]),
        ]
    }

    fn draw_confirm_screen(&self, frame: &mut Frame, area: Rect, confirm: &ConfirmScreen) {
        let Some(disk) = &self.chosen else {
            return;
        };
        let label = |text: &str| {
            Span::styled(
                format!("{text:<width$}", width = LABEL_WIDTH as usize),
                Style::new().fg(ACCENT),
            )
        };
        let mut lines = vec![Line::from(vec![
            label("Disk"),
            Span::styled(disk.title(), Style::new().bold()),
        ])];
        let erased = if disk.parts.is_empty() && disk.whole.is_none() {
            "nothing: it is empty".to_string()
        } else {
            let mut parts: Vec<String> = disk
                .parts
                .iter()
                .chain(disk.whole.iter())
                .map(|p| {
                    let mut text = format!("{} ({}", p.name, system::megabytes(p.size_bytes));
                    if !p.fs.is_empty() {
                        text.push_str(&format!(" {}", p.fs));
                    }
                    if !p.label.is_empty() {
                        text.push_str(&format!(" \"{}\"", p.label));
                    }
                    text.push(')');
                    text
                })
                .collect();
            if parts.len() > 4 {
                let more = parts.len() - 3;
                parts.truncate(3);
                parts.push(format!("{more} more"));
            }
            let mut text = format!("everything: {}", parts.join(", "));
            if !disk.holds.is_empty() {
                text.push_str(&format!(" - {}", disk.holds.join(", ")));
            }
            text
        };
        lines.push(Line::from(vec![
            label("Erased"),
            Span::styled(erased, Style::new().fg(FOCUS)),
        ]));
        if let Ok(payload) = &self.payload {
            lines.push(Line::from(vec![
                label("LiveStage"),
                Span::raw(format!(
                    "{} ({})",
                    payload.name,
                    system::megabytes(payload.bytes)
                )),
            ]));
        }
        lines.extend(self.settings_lines());
        lines.push(Line::raw(""));

        let inner = area.inner(Margin::new(1, 0));
        let block = Block::bordered()
            .title(" Ready to install ")
            .border_style(Style::new().fg(BAD))
            .padding(ratatui::widgets::Padding::horizontal(1));
        let content = block.inner(inner);
        frame.render_widget(block, inner);
        let [info, field, error, _, buttons] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(content);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info);
        let prompt = format!("Type {}", disk.name);
        let shown = confirm.typed.shown();
        let focused = confirm.focus == 0;
        let width = 16usize.max(shown.chars().count() + 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{prompt:<width$}", width = LABEL_WIDTH as usize),
                    if focused {
                        Style::new().fg(FOCUS).bold()
                    } else {
                        Style::new().fg(ACCENT)
                    },
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{shown:<width$}"),
                    if focused {
                        Style::new().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::new().add_modifier(Modifier::UNDERLINED)
                    },
                ),
                Span::styled("  to erase it and install", Style::new().fg(DIM)),
            ])),
            field,
        );
        if focused {
            let x = field.x + LABEL_WIDTH + 2 + confirm.typed.cursor as u16;
            if x < field.right() {
                frame.set_cursor_position((x, field.y));
            }
        }
        if let Some(message) = &confirm.error {
            frame.render_widget(
                Paragraph::new(message.as_str())
                    .fg(BAD)
                    .wrap(Wrap { trim: true }),
                error,
            );
        }
        let focus = confirm.focus.checked_sub(1);
        frame.render_widget(button_row(&["Back", "Install"], focus), buttons);
    }

    fn draw_installing(&self, frame: &mut Frame, area: Rect) {
        let Some(progress) = &self.progress else {
            return;
        };
        let mut lines = vec![Line::raw(format!(
            "Installing {} onto {}.",
            self.payload
                .as_ref()
                .map(|p| p.name.as_str())
                .unwrap_or("LiveStage"),
            progress.disk.title()
        ))];
        lines.push(Line::raw(""));
        for (index, stage) in progress.stages.iter().enumerate() {
            let (mark, style) = if index < progress.current {
                ("  ok  ".to_string(), Style::new().fg(GOOD))
            } else if index == progress.current {
                let mark = if stage.measured() && progress.total > 0 {
                    format!(" {:>3}% ", progress.done * 100 / progress.total)
                } else {
                    " .... ".to_string()
                };
                (mark, Style::new().fg(FOCUS))
            } else {
                ("      ".to_string(), Style::new().fg(DIM))
            };
            let mut spans = vec![
                Span::raw("["),
                Span::styled(mark, style),
                Span::raw("] "),
                Span::raw(stage.label()),
            ];
            if index == progress.current && stage.measured() && progress.total > 0 {
                spans.push(Span::styled(
                    format!("   {}", progress.rate_text()),
                    Style::new().fg(DIM),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Do not turn the computer off or pull out the USB stick until it is done.",
            Style::new().fg(FOCUS),
        ));
        let inner = area.inner(Margin::new(1, 0));
        let block = Block::bordered()
            .title(" Installing ")
            .border_style(DIM)
            .padding(ratatui::widgets::Padding::horizontal(1));
        let content = block.inner(inner);
        frame.render_widget(block, inner);
        let [text, _, gauge] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(content);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text);
        let stage = progress.stage();
        if stage.measured() && progress.total > 0 {
            let ratio = (progress.done as f64 / progress.total as f64).clamp(0.0, 1.0);
            frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::new().fg(ACCENT).bg(Color::Black))
                    .ratio(ratio)
                    .label(format!("{} {:.0}%", stage.label(), ratio * 100.0)),
                gauge,
            );
        }
    }

    fn draw_done(&self, frame: &mut Frame, area: Rect, focus: usize) {
        let Some(progress) = &self.progress else {
            return;
        };
        let disk = &progress.disk.name;
        let mut lines = Vec::new();
        let (title, border) = match &progress.result {
            Some(Ok(report)) => {
                lines.push(Line::styled(
                    format!("LiveStage is installed on {disk}, and checked."),
                    Style::new().fg(GOOD).bold(),
                ));
                lines.push(Line::raw(""));
                for note in &report.notes {
                    lines.push(Line::styled(note.clone(), Style::new().fg(FOCUS)));
                    lines.push(Line::raw(""));
                }
                lines.push(Line::from(vec![
                    Span::styled("Remove the USB stick", Style::new().bold()),
                    Span::raw(format!(
                        ", then Reboot: the computer starts LiveStage from {disk}."
                    )),
                ]));
                lines.push(Line::raw(""));
                match &progress.setup {
                    Some(config) if config.web_open => lines.push(Line::raw(format!(
                        "Then open http://{}.local:{}/ in a browser on the same network \
                         (its screen shows the address too).",
                        config.name, config.web_port
                    ))),
                    Some(_) => lines.push(Line::raw(
                        "Its web UI answers on that machine only, as chosen.",
                    )),
                    None => lines.push(Line::raw("Its first boot shows the setup on its screen.")),
                }
                ("Done", GOOD)
            }
            Some(Err(failure)) => {
                lines.push(Line::styled(
                    "Installing did not work.",
                    Style::new().fg(BAD).bold(),
                ));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<width$}", "Stopped at", width = LABEL_WIDTH as usize),
                        Style::new().fg(ACCENT),
                    ),
                    Span::raw(failure.stage.label()),
                ]));
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<width$}", "Error", width = LABEL_WIDTH as usize),
                        Style::new().fg(ACCENT),
                    ),
                    Span::styled(failure.error.clone(), Style::new().fg(BAD)),
                ]));
                lines.push(Line::raw(""));
                lines.push(Line::styled(failure.state.clone(), Style::new().bold()));
                ("Not installed", BAD)
            }
            None => ("Installing", DIM),
        };
        let labels: Vec<&str> = self.done_actions().iter().map(|a| a.label()).collect();
        Self::framed(
            frame,
            area,
            title,
            border,
            lines,
            Some((labels.as_slice(), focus)),
        );
    }
}

/// Keys on a row of buttons: the one pressed, if Enter was.
fn button_key(focus: &mut usize, count: usize, key: KeyEvent) -> Option<usize> {
    match key.code {
        KeyCode::Left | KeyCode::BackTab | KeyCode::Up => *focus = (*focus + count - 1) % count,
        KeyCode::Right | KeyCode::Tab | KeyCode::Down => *focus = (*focus + 1) % count,
        KeyCode::Enter => return Some((*focus).min(count - 1)),
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disks::tests::{computer, put};
    use crate::payload;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typing(app: &mut App, text: &str) {
        for c in text.chars() {
            app.key(press(KeyCode::Char(c)));
        }
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

    /// The computer of the disk tests, with a 3 MiB image and vda a 6 MiB
    /// file, plus what the setup's pages read.
    fn installer(name: &str) -> (App, std::path::PathBuf, Vec<u8>) {
        let (system, root) = computer(name);
        let image: Vec<u8> = (0..3 << 20).map(|i: u32| (i % 251) as u8).collect();
        let dir = root.join("usr/share/livestage/installer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("livestage.img"), &image).unwrap();
        std::fs::write(
            dir.join(payload::MANIFEST),
            payload::manifest("livestage-alpine3.24-x86_64", &image),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("dev")).unwrap();
        std::fs::write(root.join("dev/vda"), vec![0xEEu8; 6 << 20]).unwrap();
        put(&root, "/sys/class/net/eth0/address", "52:54:00:12:34:56\n");
        put(&root, "/sys/class/net/eth0/carrier", "1\n");
        put(&root, "/sys/class/net/eth0/type", "1\n");
        put(&root, "/sys/class/net/eth0/device/uevent", "");
        put(
            &root,
            "/usr/share/zoneinfo/zone1970.tab",
            "TH,KH,LA,VN\t+1345+10031\tAsia/Bangkok\tIndochina\n",
        );
        put(&root, "/etc/shadow", "root::0:::::\n");
        let payload = Payload::load(&dir.join("livestage.img"));
        assert!(payload.is_ok(), "{payload:?}");
        (App::new(system, payload, true), root, image)
    }

    fn finish(app: &mut App) {
        for _ in 0..500 {
            app.tick();
            if !app.installing() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the install did not finish");
    }

    #[test]
    fn the_name_must_be_typed_exactly() {
        assert!(confirms("vda", "vda"));
        assert!(confirms(" /dev/vda ", "vda"));
        assert!(!confirms("vd", "vda"));
        assert!(!confirms("VDA", "vda"));
        assert!(!confirms("vda1", "vda"));
        assert!(!confirms("", ""));
    }

    /// Welcome, the disks, first-boot settings, a wrong name, the right
    /// one, the install, done (printed with --nocapture).
    #[test]
    fn installing_with_the_setup_left_to_the_first_boot() {
        let (mut app, root, image) = installer("ui-first-boot");
        let mut pages = vec![screen(&app)];
        assert!(
            pages[0].contains("Everything on the disk you choose is erased"),
            "{}",
            pages[0]
        );
        assert!(
            pages[0].contains("livestage-alpine3.24-x86_64"),
            "{}",
            pages[0]
        );
        assert!(!pages[0].contains("Quit"), "{}", pages[0]);

        app.key(press(KeyCode::Enter)); // Install
        let disks = screen(&app);
        pages.push(disks.clone());
        assert!(disks.contains("> nvme0n1"), "{disks}");
        assert!(disks.contains("Windows"), "{disks}");
        assert!(disks.contains("(installer)"), "{disks}");
        assert!(disks.contains("(too small)"), "{disks}");
        assert!(disks.contains("nvme0n1p3"), "{disks}");

        // The installer's stick: refused, with the reason.
        for _ in 0..3 {
            app.key(press(KeyCode::Down));
        }
        app.key(press(KeyCode::Enter));
        let refused = screen(&app);
        pages.push(refused.clone());
        assert!(refused.contains("installer's USB stick"), "{refused}");
        app.key(press(KeyCode::Enter)); // closes the message

        // vda: empty.
        app.key(press(KeyCode::Home));
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Enter));
        let settings = screen(&app);
        pages.push(settings.clone());
        assert!(
            settings.contains("Settings for LiveStage on vda"),
            "{settings}"
        );
        app.key(press(KeyCode::Down)); // On first boot
        app.key(press(KeyCode::Enter));

        let confirm = screen(&app);
        pages.push(confirm.clone());
        assert!(confirm.contains("Ready to install"), "{confirm}");
        assert!(confirm.contains("nothing: it is empty"), "{confirm}");
        assert!(confirm.contains("asked on the first boot"), "{confirm}");
        typing(&mut app, "vd");
        app.key(press(KeyCode::Enter));
        let wrong = screen(&app);
        pages.push(wrong.clone());
        assert!(
            wrong.contains("Type vda (the disk's name) exactly"),
            "{wrong}"
        );
        assert!(!app.installing());
        typing(&mut app, "a");
        app.key(press(KeyCode::Enter));
        assert!(app.installing());
        pages.push(screen(&app));
        // Keys wait while it works.
        app.key(press(KeyCode::Esc));
        assert!(app.installing() || matches!(app.screen, Screen::Done { .. }));
        finish(&mut app);
        let done = screen(&app);
        pages.push(done.clone());
        for page in &pages {
            println!("{page}");
        }
        assert!(done.contains("LiveStage is installed on vda"), "{done}");
        assert!(done.contains("Remove the USB stick"), "{done}");
        assert!(done.contains("first boot shows the setup"), "{done}");
        let written = std::fs::read(root.join("dev/vda")).unwrap();
        assert_eq!(&written[..image.len()], &image[..]);
        // No settings were written.
        assert!(
            !app.system
                .path(install::SYS_MOUNT)
                .join("setup.conf")
                .exists()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Set up now: the setup's pages for the new disk, a name, the review,
    /// back from the confirm screen, then the install with the settings.
    #[test]
    fn installing_with_the_setup_answered_now() {
        let (mut app, root, _) = installer("ui-setup-now");
        app.key(press(KeyCode::Enter)); // Install
        app.key(press(KeyCode::Down)); // vda
        app.key(press(KeyCode::Enter));
        app.key(press(KeyCode::Enter)); // Set up now
        let name = screen(&app);
        println!("{name}");
        assert!(name.contains("installer  -  Settings: Name"), "{name}");
        assert!(name.contains("Name > Network"), "{name}");
        assert!(!name.contains("Storage"), "{name}");
        // Esc on the first page: back to the choice.
        app.key(press(KeyCode::Esc));
        assert!(screen(&app).contains("[ Set up now ]"));
        app.key(press(KeyCode::Enter));
        for _ in 0..9 {
            app.key(press(KeyCode::Backspace));
        }
        typing(&mut app, "FOH-Rack");
        app.key(press(KeyCode::Enter)); // to Back
        app.key(press(KeyCode::Down)); // to Next
        app.key(press(KeyCode::Enter));
        // Network, Audio, Web, Time: Next on each (Up from the first field).
        for _ in 0..4 {
            app.key(press(KeyCode::Up));
            app.key(press(KeyCode::Enter));
        }
        // Password.
        let password = screen(&app);
        println!("{password}");
        assert!(
            password.contains("Settings: Console password"),
            "{password}"
        );
        typing(&mut app, "secret1");
        app.key(press(KeyCode::Down));
        typing(&mut app, "secret1");
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Enter));
        let review = screen(&app);
        println!("{review}");
        assert!(review.contains("[ Use these ]"), "{review}");
        assert!(review.contains("foh-rack"), "{review}");
        app.key(press(KeyCode::Enter)); // Use these
        let confirm = screen(&app);
        println!("{confirm}");
        assert!(
            confirm.contains("foh-rack  (http://foh-rack.local:8730/)"),
            "{confirm}"
        );
        assert!(confirm.contains("Password      set"), "{confirm}");
        // Back to the review, and on again.
        app.key(press(KeyCode::Esc));
        assert!(screen(&app).contains("[ Use these ]"));
        app.key(press(KeyCode::Enter));
        typing(&mut app, "vda");
        app.key(press(KeyCode::Enter));
        finish(&mut app);
        let done = screen(&app);
        println!("{done}");
        assert!(done.contains("http://foh-rack.local:8730/"), "{done}");
        let saved = std::fs::read_to_string(app.system.path(install::SYS_MOUNT).join("setup.conf"))
            .unwrap();
        let config = SetupConfig::parse(&saved);
        assert_eq!(config.name, "foh-rack");
        assert!(saved.contains("DEVICE_NAME=\"foh-rack\""));
        assert!(!saved.contains("secret1"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_install_says_what_is_left_on_the_disk() {
        let (mut app, root, mut image) = installer("ui-failed");
        // The image damaged after its manifest was made.
        image[(2 << 20) + 1] ^= 0x40;
        std::fs::write(
            root.join("usr/share/livestage/installer/livestage.img"),
            &image,
        )
        .unwrap();
        app.key(press(KeyCode::Enter));
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Enter));
        app.key(press(KeyCode::Down));
        app.key(press(KeyCode::Enter));
        typing(&mut app, "/dev/vda");
        app.key(press(KeyCode::Enter));
        finish(&mut app);
        let done = screen(&app);
        println!("{done}");
        assert!(done.contains("Installing did not work"), "{done}");
        assert!(done.contains("Writing LiveStage"), "{done}");
        assert!(done.contains("damaged"), "{done}");
        assert!(done.contains("vda has been erased"), "{done}");
        assert!(done.contains("[ Back to the disks ]"), "{done}");
        app.key(press(KeyCode::Enter)); // Back to the disks
        assert!(screen(&app).contains("Choose the disk"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn without_an_image_there_is_nothing_to_install() {
        let (system, root) = computer("ui-no-image");
        let payload = Payload::load(&system.path(payload::DEFAULT_PAYLOAD));
        let mut app = App::new(system, payload, false);
        let welcome = screen(&app);
        assert!(welcome.contains("No LiveStage image"), "{welcome}");
        assert!(welcome.contains("Quit"), "{welcome}");
        app.key(press(KeyCode::Enter));
        assert!(screen(&app).contains("Nothing to install"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
