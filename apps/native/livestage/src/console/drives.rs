//! The drives as the console lists them (a line per volume, or per disk
//! with none) and where recordings go, drawn from the storage state. What
//! can be done with a drive is the console's (`ui.rs`).

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::storage_api::{self as api, Mount, State as StorageState, Volume};
use super::system;
use super::widgets::{BAD, DIM, FOCUS, GOOD};

/// Where recordings go, for the home screen: the drive and its free space,
/// or the warning that its drive is missing.
pub fn recordings_summary(state: &StorageState) -> Vec<Span<'static>> {
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
pub fn fallback_reason(state: &StorageState) -> String {
    match state.volume(&state.target.id) {
        Some(volume) if volume.ejected => format!("{} is ejected", state.target.label),
        Some(_) => format!("{} cannot be written to", state.target.label),
        None => format!("{} is not plugged in", state.target.label),
    }
}

/// A volume as people know it: its label, else its device.
pub fn volume_name(volume: &Volume) -> String {
    if volume.id == api::INTERNAL {
        api::INTERNAL_LABEL.to_string()
    } else if volume.label.is_empty() {
        format!("{} (no name)", volume.device)
    } else {
        volume.label.clone()
    }
}

/// A line of the drives list: a volume, or a disk with none on it.
#[derive(Clone, Copy)]
pub enum DriveRow {
    Volume(usize),
    Blank(usize),
}

pub fn drive_rows(state: &StorageState) -> Vec<DriveRow> {
    let mut rows: Vec<DriveRow> = (0..state.volumes.len()).map(DriveRow::Volume).collect();
    for (index, disk) in state.disks.iter().enumerate() {
        if !disk.system && !state.volumes.iter().any(|v| v.disk == disk.disk) {
            rows.push(DriveRow::Blank(index));
        }
    }
    rows
}

/// The column names over [`DriveRow::line`].
pub fn drive_header() -> Line<'static> {
    Line::styled(
        format!(
            "  {:<12} {:<19} {:<7}{:>8}{:>8}  {}",
            "Volume", "Drive", "Format", "Size", "Free", "State"
        ),
        Style::new().fg(DIM),
    )
}

pub fn fs_name(fs: Option<&str>) -> String {
    match fs {
        Some("exfat") => "exFAT".into(),
        Some("vfat") => "FAT32".into(),
        Some("ntfs") => "NTFS".into(),
        Some(other) => other.into(),
        None => "-".into(),
    }
}

/// At most `width` characters.
pub fn fit(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

impl DriveRow {
    pub fn is_recording(self, state: &StorageState) -> bool {
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
    pub fn disk_text(state: &StorageState, disk: &str) -> String {
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

    pub fn title(self, state: &StorageState) -> String {
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

    pub fn line(self, state: &StorageState, selected: bool) -> Line<'static> {
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
    pub fn details(self, state: &StorageState) -> String {
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
