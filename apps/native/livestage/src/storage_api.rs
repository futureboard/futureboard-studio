//! The LiveStage appliance's storage protocol: what `livestage-setup
//! --storage-service` (root) answers on [`SOCKET`], and what the console
//! shows. Types and pure checks only, no I/O: the console includes this
//! file (`src/console/storage.rs` is the service), and so does the server
//! (its only other client).
//!
//! One JSON object per line each way; one request, one reply, then the
//! client may send another or close:
//!
//! ```txt
//! {"op":"list"}
//! {"op":"use","id":"internal"}                       (or a volume's UUID)
//! {"op":"eject","id":"1A2B-3C4D"}
//! {"op":"format","disk":"sdb","label":"LIVESTAGE"}   (label optional)
//! -> {"ok":true,"storage":{...}}
//! -> {"ok":false,"error":"A sentence for a person.","storage":{...}}
//! ```
//!
//! Deviations from the shared contract: none. Where it left a detail open:
//!
//! - `Volume::label` is always a string: `""` when the filesystem has none.
//! - `Volume::id` is `""` for a volume that cannot be a target because it
//!   has no usable UUID (a partition with no filesystem, say).
//! - `Target::label` while that volume is not plugged in: its label as last
//!   seen (`RECORD_STORAGE_LABEL` in setup.conf, saved by `use` and kept up
//!   to date by the service); its UUID only if no label was ever known.
//! - `format`'s `label` may be left out: [`DEFAULT_LABEL`].
//! - Ejecting an unplugged or unknown volume, or using an unsupported one,
//!   is an `ok:false` reply with the reason.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// The service's socket (0660 root:livestage, in a 0750 root:livestage dir).
pub const SOCKET: &str = "/run/livestage/storage.sock";
pub const RUN_DIR: &str = "/run/livestage";
/// `RECORD_STORAGE` for the data partition.
pub const INTERNAL: &str = "internal";
pub const INTERNAL_LABEL: &str = "Internal";
/// Where recordings go on the data partition.
pub const INTERNAL_RECORDINGS: &str = "/data/livestage/recordings";
/// External volumes are mounted at `/media/<UUID>`.
pub const MEDIA: &str = "/media";
/// The folder recordings go to on an external volume.
pub const RECORDINGS_FOLDER: &str = "LiveStage Recordings";
pub const DEFAULT_LABEL: &str = "LIVESTAGE";
/// An exFAT label is at most 11 characters.
pub const MAX_LABEL: usize = 11;
/// The longest request line the service reads.
pub const MAX_REQUEST: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    List,
    Use {
        id: String,
    },
    Eject {
        id: String,
    },
    Format {
        disk: String,
        #[serde(default = "default_label")]
        label: String,
    },
}

fn default_label() -> String {
    DEFAULT_LABEL.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub storage: State,
}

impl Reply {
    pub fn ok(storage: State) -> Self {
        Self {
            ok: true,
            error: None,
            storage,
        }
    }

    pub fn failed(error: impl Into<String>, storage: State) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            storage,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct State {
    pub target: Target,
    pub volumes: Vec<Volume>,
    /// Whole disks, for Format.
    pub disks: Vec<Disk>,
}

impl State {
    pub fn volume(&self, id: &str) -> Option<&Volume> {
        self.volumes.iter().find(|v| !id.is_empty() && v.id == id)
    }

    /// An external volume is chosen but recordings go to the internal one.
    pub fn falling_back(&self) -> bool {
        self.target.id != INTERNAL && !self.target.available
    }
}

/// Where recordings go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// What `RECORD_STORAGE` says: [`INTERNAL`] or a volume's UUID.
    pub id: String,
    /// The volume's label, or [`INTERNAL_LABEL`].
    pub label: String,
    /// False: an external volume that is not plugged in or not mountable.
    pub available: bool,
    /// Where recordings go now (the internal folder when falling back).
    pub recordings_dir: String,
}

impl Default for Target {
    fn default() -> Self {
        Self {
            id: INTERNAL.to_string(),
            label: INTERNAL_LABEL.to_string(),
            available: true,
            recordings_dir: INTERNAL_RECORDINGS.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// [`INTERNAL`] or the filesystem's UUID; `""` when it has none.
    pub id: String,
    /// The filesystem's label; `""` when it has none.
    pub label: String,
    /// `exfat`, `vfat`, `ntfs`, `ext4`, another name, or none.
    pub fs: Option<String>,
    /// The partition (or whole disk) in /dev: `sdb1`.
    pub device: String,
    pub disk: String,
    pub model: Option<String>,
    pub size_bytes: u64,
    /// None when not mounted.
    pub free_bytes: Option<u64>,
    pub mounted: Option<Mount>,
    pub mount_path: Option<String>,
    /// Can be a recording target.
    pub supported: bool,
    /// Ejected, and not mounted again until it is unplugged.
    pub ejected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mount {
    Rw,
    Ro,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub disk: String,
    pub model: Option<String>,
    pub size_bytes: u64,
    pub removable: bool,
    /// The disk the system runs from: never formatted.
    pub system: bool,
}

/// The filesystems an external volume can be recorded to.
pub const SUPPORTED_FS: [&str; 4] = ["exfat", "vfat", "ntfs", "ext4"];

/// A recording target: [`INTERNAL`], or a UUID as blkid prints it (hex
/// digits and dashes, 4 to 36 of them). Nothing else ever reaches a path.
pub fn valid_id(id: &str) -> bool {
    id == INTERNAL
        || ((4..=36).contains(&id.len())
            && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
            && id.chars().any(|c| c.is_ascii_hexdigit()))
}

/// A label for Format: 1 to 11 letters, digits, spaces, `-` or `_`.
pub fn valid_label(label: &str) -> Result<(), &'static str> {
    if label.trim().is_empty() {
        return Err("Give the drive a name.");
    }
    if label.chars().count() > MAX_LABEL {
        return Err("A drive's name can be at most 11 characters.");
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
    {
        return Err("Use only letters, digits, spaces, '-' and '_' in the name.");
    }
    if label.starts_with(' ') || label.ends_with(' ') {
        return Err("The name cannot start or end with a space.");
    }
    Ok(())
}

/// Where an external volume is mounted.
pub fn mount_dir(uuid: &str) -> String {
    format!("{MEDIA}/{uuid}")
}

/// Where recordings go on an external volume.
pub fn recordings_dir(uuid: &str) -> String {
    format!("{MEDIA}/{uuid}/{RECORDINGS_FOLDER}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_as_the_contract_writes_them() {
        let parse = |text: &str| serde_json::from_str::<Request>(text).unwrap();
        assert_eq!(parse(r#"{"op":"list"}"#), Request::List);
        assert_eq!(
            parse(r#"{"op":"use","id":"internal"}"#),
            Request::Use {
                id: "internal".into()
            }
        );
        assert_eq!(
            parse(r#"{"op":"eject","id":"1A2B-3C4D"}"#),
            Request::Eject {
                id: "1A2B-3C4D".into()
            }
        );
        assert_eq!(
            parse(r#"{"op":"format","disk":"sdb","label":"SHOW"}"#),
            Request::Format {
                disk: "sdb".into(),
                label: "SHOW".into()
            }
        );
        assert_eq!(
            parse(r#"{"op":"format","disk":"sdb"}"#),
            Request::Format {
                disk: "sdb".into(),
                label: DEFAULT_LABEL.into()
            }
        );
        assert!(serde_json::from_str::<Request>(r#"{"op":"mkfs"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"use"}"#).is_err());
        // Round trip, as the server writes them.
        let request = Request::Use {
            id: "1A2B-3C4D".into(),
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"op":"use","id":"1A2B-3C4D"}"#
        );
    }

    #[test]
    fn the_state_has_the_contracts_shape() {
        let text = r#"{
          "target": {"id":"internal","label":"Internal","available":true,
                     "recordings_dir":"/data/livestage/recordings"},
          "volumes": [{"id":"internal","label":"lsdata","fs":"exfat","device":"sda4",
                       "disk":"sda","model":"SanDisk Ultra","size_bytes":64023257088,
                       "free_bytes":61000000000,"mounted":"rw","mount_path":"/data",
                       "supported":true,"ejected":false},
                      {"id":"","label":"","fs":null,"device":"sdc1","disk":"sdc",
                       "model":null,"size_bytes":1000,"free_bytes":null,"mounted":null,
                       "mount_path":null,"supported":false,"ejected":false}],
          "disks": [{"disk":"sdb","model":"SanDisk Ultra","size_bytes":64023257088,
                     "removable":true,"system":false}]
        }"#;
        let state: State = serde_json::from_str(text).unwrap();
        assert_eq!(state.target, Target::default());
        assert_eq!(state.volumes[0].mounted, Some(Mount::Rw));
        assert_eq!(state.volumes[1].fs, None);
        assert!(!state.falling_back());
        assert_eq!(state.volume("internal").unwrap().device, "sda4");
        assert!(state.volume("").is_none());
        // And back: every field the contract names, null where it says so.
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["volumes"][0]["mounted"], "rw");
        assert!(value["volumes"][1]["free_bytes"].is_null());
        assert!(value["volumes"][1]["mount_path"].is_null());
        assert_eq!(value["disks"][0]["removable"], true);
        assert_eq!(serde_json::from_value::<State>(value).unwrap(), state);
    }

    #[test]
    fn replies_carry_an_error_only_when_failed() {
        let ok = serde_json::to_value(Reply::ok(State::default())).unwrap();
        assert_eq!(ok["ok"], true);
        assert!(ok.get("error").is_none());
        assert_eq!(ok["storage"]["target"]["id"], "internal");
        let failed = Reply::failed("That drive is not plugged in.", State::default());
        let text = serde_json::to_string(&failed).unwrap();
        assert!(text.starts_with(r#"{"ok":false,"error":"That drive is not plugged in.""#));
        assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), failed);
    }

    #[test]
    fn ids_are_internal_or_a_uuid() {
        for id in [
            "internal",
            "1A2B-3C4D",
            "0123456789ABCDEF",
            "3f1c2a9e-8d0b-4e6f-9a7c-5b1d2e3f4a5b",
        ] {
            assert!(valid_id(id), "{id}");
        }
        for id in [
            "",
            "abc",
            "----",
            "../../etc",
            "1A2B 3C4D",
            "1A2B-3C4D/..",
            "Internal",
            &"a".repeat(37),
        ] {
            assert!(!valid_id(id), "{id}");
        }
        assert_eq!(
            recordings_dir("1A2B-3C4D"),
            "/media/1A2B-3C4D/LiveStage Recordings"
        );
    }

    #[test]
    fn labels_fit_exfat() {
        assert!(valid_label("LIVESTAGE").is_ok());
        assert!(valid_label("Show 2026").is_ok());
        assert!(valid_label("").is_err());
        assert!(valid_label("TWELVE CHARS").is_err());
        assert!(valid_label("a/b").is_err());
        assert!(valid_label(" x").is_err());
    }
}
