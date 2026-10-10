//! Where recordings go, and the drives they can go to (root's side; the
//! protocol and the shared types are in `storage_api.rs`).
//!
//! The internal volume is the data partition (`LABEL=lsdata`, /data, from
//! fstab). Every other partition with a filesystem on a disk that is not the
//! system's own (the one holding `LABEL=lsroot`) is an external volume:
//! mounted read-only at `/media/<UUID>`, or read-write when it is the one
//! `RECORD_STORAGE` names. `--storage-service` keeps that so every couple of
//! seconds and answers the server on the socket; the console calls the same
//! functions directly.
//!
//! What is read (sysfs, `blkid`, /proc/mounts) is parsed by plain functions,
//! and what to do about it is decided by plain functions ([`build_state`],
//! [`plan`]); both are tested on canned text. Under `--root` nothing is run:
//! `blkid`'s output is read from [`BLKID_SAMPLE`] in the tree instead.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::storage_api::{self as api, Disk, Mount, Reply, Request, State, Target, Volume};
use super::system::System;

const SYSTEM_LABEL: &str = "lsroot";
const DATA_LABEL: &str = "lsdata";
const DATA_MOUNT: &str = "/data";
/// Volumes ejected and still plugged in: `device uuid` per line.
const EJECTED: &str = "/run/livestage/ejected";
/// Held while anything is mounted, unmounted or formatted (by the service or
/// the console).
#[cfg(target_os = "linux")]
const LOCK: &str = "/run/livestage/storage.lock";
/// Under `--root`: what `blkid` printed on the machine the tree is from.
pub const BLKID_SAMPLE: &str = "/blkid.txt";
/// GPT type of a Microsoft basic data partition (what Windows and macOS
/// expect an exFAT drive to have).
const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
/// Partitions smaller than this (an MBR extended partition's 1 KiB) are not
/// volumes.
const MIN_VOLUME: u64 = 1 << 20;
const SERVICE_USER: &str = "livestage";
/// How often the service looks at the drives.
#[cfg_attr(not(unix), allow(dead_code))]
pub const POLL: Duration = Duration::from_secs(2);
/// blkid runs again when the block devices change, and at least this often.
const REPROBE: Duration = Duration::from_secs(60);
/// How long an action waits for another one (a format) to finish.
const LOCK_WAIT: Duration = Duration::from_secs(20);

// ── What is read ────────────────────────────────────────────────────────────

/// What `blkid` says about one device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    pub label: String,
    pub uuid: String,
    /// `TYPE`: `exfat`, `vfat`, `ntfs`, `ext4`, …
    pub fs: String,
}

/// `blkid`'s usual output, a line per device:
/// `/dev/sdb1: LABEL="SHOW" UUID="1A2B-3C4D" BLOCK_SIZE="512" TYPE="exfat"`.
/// Keyed by the name in /dev.
pub fn parse_blkid(text: &str) -> BTreeMap<String, Probe> {
    let mut found = BTreeMap::new();
    for line in text.lines() {
        let Some((device, rest)) = line.split_once(": ") else {
            continue;
        };
        let Some(name) = device.trim().strip_prefix("/dev/") else {
            continue;
        };
        if !valid_device_name(name) {
            continue;
        }
        let mut probe = Probe::default();
        for (key, value) in blkid_pairs(rest) {
            match key.as_str() {
                "LABEL" => probe.label = printable(&value),
                "UUID" => probe.uuid = value,
                "TYPE" => probe.fs = value,
                _ => {}
            }
        }
        found.insert(name.to_string(), probe);
    }
    found
}

/// `KEY="value"` pairs; `\"` and `\\` inside a value are what util-linux
/// escapes.
fn blkid_pairs(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            key.push(c);
            chars.next();
        }
        if chars.next() != Some('=') || chars.next() != Some('"') {
            break;
        }
        let mut value = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        value.push(next);
                    }
                }
                '"' => {
                    closed = true;
                    break;
                }
                c => value.push(c),
            }
        }
        pairs.push((key, value));
        if !closed {
            break;
        }
    }
    pairs
}

/// A label as shown: no control characters (it ends up on the console).
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// A name under /dev as the kernel makes them: `sdb1`, `nvme0n1p2`.
fn valid_device_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// One line of /proc/mounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub source: String,
    pub path: String,
    pub fs: String,
    pub rw: bool,
}

impl MountEntry {
    /// The /dev name it was mounted from, if it was.
    fn device(&self) -> Option<&str> {
        self.source
            .strip_prefix("/dev/")
            .filter(|name| valid_device_name(name))
    }
}

/// /proc/mounts: `source path type options 0 0`, with spaces in paths
/// written as `\040`.
pub fn parse_mounts(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = unescape_mount(fields.next()?);
            let path = unescape_mount(fields.next()?);
            let fs = fields.next()?.to_string();
            let rw = fields.next()?.split(',').any(|o| o == "rw");
            Some(MountEntry {
                source,
                path,
                fs,
                rw,
            })
        })
        .collect()
}

fn unescape_mount(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && let Some(octal) = field.get(i + 1..i + 4)
            && let Ok(byte) = u8::from_str_radix(octal, 8)
        {
            out.push(byte);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A whole disk, from /sys/block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDisk {
    pub name: String,
    pub model: Option<String>,
    pub size_bytes: u64,
    pub removable: bool,
    /// `major:minor`.
    pub dev: String,
    pub parts: Vec<BlockPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockPart {
    pub name: String,
    pub number: u32,
    pub size_bytes: u64,
    pub dev: String,
}

/// Devices that are not disks someone plugged in.
fn is_virtual(name: &str) -> bool {
    [
        "loop", "ram", "zram", "sr", "fd", "dm-", "md", "nbd", "mtdblock",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

/// The ID of a device as `major:minor` (Linux's `dev_t` layout).
#[cfg_attr(not(unix), allow(dead_code))]
fn dev_string(dev: u64) -> String {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    format!("{major}:{minor}")
}

/// What the machine has, read at one moment.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub disks: Vec<BlockDisk>,
    pub probes: BTreeMap<String, Probe>,
    pub mounts: Vec<MountEntry>,
    /// `major:minor` of the device `/` is on (`None` under `--root`).
    pub root_dev: Option<String>,
}

impl Inventory {
    fn label(&self, device: &str) -> Option<&str> {
        self.probes.get(device).map(|p| p.label.as_str())
    }

    /// The disk the system runs from: the one `/` is mounted from, else the
    /// one with the `lsroot` label.
    pub fn system_disk(&self) -> Option<&BlockDisk> {
        let holds = |disk: &BlockDisk, test: &dyn Fn(&str, &str) -> bool| {
            test(&disk.name, &disk.dev) || disk.parts.iter().any(|p| test(&p.name, &p.dev))
        };
        if let Some(root) = &self.root_dev
            && let Some(disk) = self.disks.iter().find(|d| holds(d, &|_, dev| dev == root))
        {
            return Some(disk);
        }
        if let Some(root) = self
            .mounts
            .iter()
            .find(|m| m.path == "/")
            .and_then(|m| m.device())
            && let Some(disk) = self
                .disks
                .iter()
                .find(|d| holds(d, &|name, _| name == root))
        {
            return Some(disk);
        }
        self.disks
            .iter()
            .find(|d| holds(d, &|name, _| self.label(name) == Some(SYSTEM_LABEL)))
    }

    /// The data partition: what /data is mounted from, else `lsdata` on the
    /// system disk. `(device, disk)`.
    pub fn internal(&self) -> Option<(&BlockPart, &BlockDisk)> {
        let system = self.system_disk();
        let on_system = |test: &dyn Fn(&BlockPart) -> bool| {
            self.disks
                .iter()
                .filter(|d| system.is_none_or(|s| s.name == d.name))
                .find_map(|d| d.parts.iter().find(|p| test(p)).map(|p| (p, d)))
        };
        let mounted = self
            .mounts
            .iter()
            .find(|m| m.path == DATA_MOUNT)
            .and_then(|m| m.device());
        mounted
            .and_then(|device| on_system(&|p| p.name == device))
            .or_else(|| on_system(&|p| self.label(&p.name) == Some(DATA_LABEL)))
    }

    /// Every block device name there is now.
    fn present(&self) -> BTreeSet<&str> {
        self.disks
            .iter()
            .flat_map(|d| {
                std::iter::once(d.name.as_str()).chain(d.parts.iter().map(|p| p.name.as_str()))
            })
            .collect()
    }
}

/// Volumes ejected and still plugged in.
pub type Ejected = BTreeSet<(String, String)>;

fn parse_ejected(text: &str) -> Ejected {
    text.lines()
        .filter_map(|line| {
            let (device, uuid) = line.trim().split_once(' ')?;
            Some((device.to_string(), uuid.to_string()))
        })
        .collect()
}

fn ejected_file(ejected: &Ejected) -> String {
    ejected
        .iter()
        .map(|(device, uuid)| format!("{device} {uuid}\n"))
        .collect()
}

/// Forgets the ejected volumes that have been unplugged (or reformatted).
fn prune_ejected(ejected: &Ejected, inventory: &Inventory) -> Ejected {
    ejected
        .iter()
        .filter(|(device, uuid)| {
            inventory
                .probes
                .get(device)
                .is_some_and(|p| p.uuid == *uuid)
                && inventory.present().contains(device.as_str())
        })
        .cloned()
        .collect()
}

// ── Deciding ────────────────────────────────────────────────────────────────

/// A UUID that may name a volume (and a folder under /media).
fn volume_id(probe: Option<&Probe>) -> String {
    probe
        .map(|p| p.uuid.as_str())
        .filter(|uuid| *uuid != api::INTERNAL && api::valid_id(uuid))
        .unwrap_or("")
        .to_string()
}

/// The mount a volume has, leaving out one under /media that is another
/// volume's folder (a stale mount of a drive that was swapped for this one
/// under the same device name).
fn mount_of<'a>(inventory: &'a Inventory, device: &str, id: &str) -> Option<&'a MountEntry> {
    let mut mounts = inventory
        .mounts
        .iter()
        .filter(|m| m.device() == Some(device))
        .filter(|m| !is_media_folder(&m.path) || (!id.is_empty() && m.path == api::mount_dir(id)));
    let first = mounts.next()?;
    Some(first)
}

/// `/media/<one name>`.
fn is_media_folder(path: &str) -> bool {
    path.strip_prefix(api::MEDIA)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

/// The state: the volumes, the disks and where recordings go. `free` gives
/// the free bytes of a mounted path.
pub fn build_state(
    inventory: &Inventory,
    target: &str,
    target_label: &str,
    ejected: &Ejected,
    free: &dyn Fn(&str) -> Option<u64>,
) -> State {
    let system = inventory.system_disk().map(|d| d.name.clone());
    let mut volumes = Vec::new();

    // The internal volume, always (even with its partition missing, so the
    // fallback shows).
    let data_mount = inventory.mounts.iter().find(|m| m.path == DATA_MOUNT);
    let internal = inventory.internal();
    volumes.push(Volume {
        id: api::INTERNAL.to_string(),
        label: internal
            .and_then(|(p, _)| inventory.label(&p.name))
            .filter(|l| !l.is_empty())
            .unwrap_or(DATA_LABEL)
            .to_string(),
        fs: internal
            .and_then(|(p, _)| inventory.probes.get(&p.name))
            .map(|p| p.fs.clone())
            .or_else(|| data_mount.map(|m| m.fs.clone()))
            .filter(|fs| !fs.is_empty()),
        device: internal.map(|(p, _)| p.name.clone()).unwrap_or_default(),
        disk: internal.map(|(_, d)| d.name.clone()).unwrap_or_default(),
        model: internal.and_then(|(_, d)| d.model.clone()),
        size_bytes: internal.map(|(p, _)| p.size_bytes).unwrap_or(0),
        free_bytes: data_mount.and_then(|_| free(DATA_MOUNT)),
        mounted: data_mount.map(|m| if m.rw { Mount::Rw } else { Mount::Ro }),
        mount_path: data_mount.map(|_| DATA_MOUNT.to_string()),
        supported: true,
        ejected: false,
    });

    for disk in &inventory.disks {
        if Some(&disk.name) == system.as_ref() {
            continue;
        }
        let units: Vec<(&str, u64)> = if disk.parts.is_empty() {
            // A filesystem on the whole disk (a "superfloppy" stick).
            inventory
                .probes
                .contains_key(&disk.name)
                .then_some((disk.name.as_str(), disk.size_bytes))
                .into_iter()
                .collect()
        } else {
            disk.parts
                .iter()
                .filter(|p| p.size_bytes >= MIN_VOLUME)
                .map(|p| (p.name.as_str(), p.size_bytes))
                .collect()
        };
        for (device, size_bytes) in units {
            let probe = inventory.probes.get(device);
            let id = volume_id(probe);
            let fs = probe.map(|p| p.fs.clone()).filter(|fs| !fs.is_empty());
            let mount = mount_of(inventory, device, &id);
            volumes.push(Volume {
                supported: !id.is_empty()
                    && fs
                        .as_deref()
                        .is_some_and(|fs| api::SUPPORTED_FS.contains(&fs)),
                ejected: !id.is_empty() && ejected.contains(&(device.to_string(), id.clone())),
                label: probe.map(|p| p.label.clone()).unwrap_or_default(),
                fs,
                device: device.to_string(),
                disk: disk.name.clone(),
                model: disk.model.clone(),
                size_bytes,
                free_bytes: mount.and_then(|m| free(&m.path)),
                mounted: mount.map(|m| if m.rw { Mount::Rw } else { Mount::Ro }),
                mount_path: mount.map(|m| m.path.clone()),
                id,
            });
        }
    }

    let disks = inventory
        .disks
        .iter()
        .map(|d| Disk {
            disk: d.name.clone(),
            model: d.model.clone(),
            size_bytes: d.size_bytes,
            removable: d.removable,
            system: Some(&d.name) == system.as_ref(),
        })
        .collect();

    State {
        target: target_of(target, target_label, &volumes),
        volumes,
        disks,
    }
}

/// `saved_label` is the name the target had when last seen: what to call
/// it while it is missing.
fn target_of(id: &str, saved_label: &str, volumes: &[Volume]) -> Target {
    if id == api::INTERNAL || !api::valid_id(id) {
        return Target::default();
    }
    let volume = volumes.iter().find(|v| v.id == id);
    let available = volume.is_some_and(|v| {
        v.supported
            && !v.ejected
            && v.mounted == Some(Mount::Rw)
            && v.mount_path.as_deref() == Some(api::mount_dir(id).as_str())
    });
    Target {
        id: id.to_string(),
        label: volume
            .map(|v| v.label.clone())
            .filter(|l| !l.is_empty())
            .or_else(|| (!saved_label.is_empty()).then(|| saved_label.to_string()))
            .unwrap_or_else(|| id.to_string()),
        available,
        recordings_dir: if available {
            api::recordings_dir(id)
        } else {
            api::INTERNAL_RECORDINGS.to_string()
        },
    }
}

/// One thing to do to keep the mounts as the policy says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Its device is gone (or is another drive now): `umount -l`.
    Detach {
        path: String,
    },
    /// A folder under /media with nothing mounted on it.
    RemoveDir {
        path: String,
    },
    Mount {
        device: String,
        id: String,
        fs: String,
        rw: bool,
    },
    Remount {
        path: String,
        rw: bool,
    },
}

/// What to do: stale mounts away, every external volume that can be one
/// mounted (read-only, the target read-write), the rest left alone.
/// `media` is the folders now under /media.
pub fn plan(state: &State, inventory: &Inventory, media: &[String]) -> Vec<Step> {
    let mut steps = Vec::new();
    let present = inventory.present();
    let live: BTreeSet<&str> = state
        .volumes
        .iter()
        .filter_map(|v| v.mount_path.as_deref())
        .collect();
    for mount in &inventory.mounts {
        let under_media = mount
            .path
            .strip_prefix(api::MEDIA)
            .is_some_and(|rest| rest.starts_with('/'));
        if !under_media {
            continue;
        }
        let gone = match mount.device() {
            Some(device) => !present.contains(device),
            None => false,
        };
        // Still there, but another drive than the folder is for.
        let swapped = is_media_folder(&mount.path)
            && mount.device().is_some()
            && !gone
            && !live.contains(mount.path.as_str());
        if gone || swapped {
            steps.push(Step::Detach {
                path: mount.path.clone(),
            });
            if is_media_folder(&mount.path) {
                steps.push(Step::RemoveDir {
                    path: mount.path.clone(),
                });
            }
        }
    }
    let mounted: BTreeSet<&str> = inventory.mounts.iter().map(|m| m.path.as_str()).collect();
    for folder in media {
        let path = format!("{}/{folder}", api::MEDIA);
        if !mounted.contains(path.as_str()) {
            steps.push(Step::RemoveDir { path });
        }
    }
    for volume in &state.volumes {
        if volume.id == api::INTERNAL || !volume.supported || volume.ejected {
            continue;
        }
        let rw = volume.id == state.target.id;
        let folder = api::mount_dir(&volume.id);
        match (volume.mounted, volume.mount_path.as_deref()) {
            (None, _) => steps.push(Step::Mount {
                device: volume.device.clone(),
                id: volume.id.clone(),
                fs: volume.fs.clone().unwrap_or_default(),
                rw,
            }),
            (Some(mode), Some(path)) if path == folder && (mode == Mount::Rw) != rw => {
                steps.push(Step::Remount { path: folder, rw })
            }
            // Right as it is, or mounted somewhere by hand: left alone.
            _ => {}
        }
    }
    steps
}

/// The partition a fresh GPT's first entry makes on `disk`.
fn first_partition_name(disk: &str) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p1")
    } else {
        format!("{disk}1")
    }
}

/// `/etc/passwd` or `/etc/group`: the number in the third field of `name`.
fn id_in(text: &str, name: &str) -> Option<u32> {
    text.lines()
        .find(|line| line.split(':').next() == Some(name))
        .and_then(|line| line.split(':').nth(2))
        .and_then(|id| id.parse().ok())
}

// ── Doing ───────────────────────────────────────────────────────────────────

struct Probes {
    fingerprint: String,
    at: Instant,
    found: BTreeMap<String, Probe>,
}

/// The drives, as the service and the console see them. Shared between the
/// service's threads; actions take a lock on /run that the console's take
/// too.
#[derive(Default)]
pub struct Storage {
    probes: Mutex<Option<Probes>>,
    /// Steps that failed, not tried again (nor logged again) until a drive
    /// comes or goes, or a minute passes.
    failed: Mutex<BTreeSet<String>>,
}

impl Storage {
    pub fn new() -> Self {
        Self::default()
    }

    /// The block devices as names and sizes: a change means a drive came
    /// or went.
    fn fingerprint(system: &System) -> String {
        let mut names: Vec<String> = std::fs::read_dir(system.path("/sys/class/block"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let size = std::fs::read_to_string(entry.path().join("size")).unwrap_or_default();
                format!("{name}={}", size.trim())
            })
            .collect();
        names.sort();
        names.join(" ")
    }

    fn blkid(system: &System) -> BTreeMap<String, Probe> {
        let text = if system.is_live() {
            // No cache: a stick formatted elsewhere since must show as it is.
            system
                .run("blkid", &["-c", "/dev/null"])
                .unwrap_or_default()
        } else {
            system.read(BLKID_SAMPLE).unwrap_or_default()
        };
        parse_blkid(&text)
    }

    /// Forget what blkid said (after changing a filesystem).
    fn invalidate(&self) {
        *self.probes.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn inventory(&self, system: &System) -> Inventory {
        let fingerprint = Self::fingerprint(system);
        let probes = {
            let mut cache = self.probes.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = cache
                .as_ref()
                .is_some_and(|c| c.fingerprint == fingerprint && c.at.elapsed() < REPROBE);
            if !fresh {
                // A drive came or went, or a minute passed: what failed is
                // tried again.
                self.failed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                *cache = Some(Probes {
                    fingerprint,
                    at: Instant::now(),
                    found: Self::blkid(system),
                });
            }
            cache.as_ref().map(|c| c.found.clone()).unwrap_or_default()
        };
        Inventory {
            disks: read_disks(system),
            probes,
            mounts: parse_mounts(&system.read("/proc/mounts").unwrap_or_default()),
            root_dev: root_dev(system),
        }
    }

    /// `RECORD_STORAGE` and the label saved with it.
    fn target(system: &System) -> (String, String) {
        system
            .load_config()
            .map(|c| (c.record_storage, c.record_storage_label))
            .unwrap_or_else(|| (api::INTERNAL.to_string(), String::new()))
    }

    fn state_of(system: &System, inventory: &Inventory) -> State {
        let ejected = parse_ejected(&system.read(EJECTED).unwrap_or_default());
        let free = |path: &str| system.space(path).map(|(free, _)| free);
        let (target, label) = Self::target(system);
        build_state(inventory, &target, &label, &ejected, &free)
    }

    /// The state as it is now (nothing is changed).
    pub fn state(&self, system: &System) -> State {
        Self::state_of(system, &self.inventory(system))
    }

    /// One look at the drives by the service: the mounts put right. Skipped
    /// while an action holds the lock. Returns what it did, for the log.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn poll(&self, system: &System) -> Vec<String> {
        match Lock::take(system, Duration::ZERO) {
            Ok(Some(_lock)) => self.reconcile(system),
            _ => Vec::new(),
        }
    }

    /// Keeps the mounts per the policy (the lock held).
    fn reconcile(&self, system: &System) -> Vec<String> {
        let inventory = self.inventory(system);
        let mut log = Vec::new();
        let ejected = parse_ejected(&system.read(EJECTED).unwrap_or_default());
        let pruned = prune_ejected(&ejected, &inventory);
        if pruned != ejected {
            let _ = system.write(EJECTED, &ejected_file(&pruned));
        }
        let state = Self::state_of(system, &inventory);
        let media: Vec<String> = std::fs::read_dir(system.path(api::MEDIA))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for step in plan(&state, &inventory, &media) {
            let key = format!("{step:?}");
            if self
                .failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&key)
            {
                continue;
            }
            let label = |id: &str| {
                state
                    .volume(id)
                    .map(|v| describe(v))
                    .unwrap_or_else(|| id.to_string())
            };
            let result = match &step {
                Step::Detach { path } => system
                    .run("umount", &["-l", path])
                    .map(|_| format!("{path}: its drive is gone; unmounted")),
                Step::RemoveDir { path } => {
                    // Only ever empty folders (remove_dir refuses others).
                    let _ = std::fs::remove_dir(system.path(path));
                    continue;
                }
                Step::Mount { device, id, fs, rw } => {
                    self.mount(system, device, id, fs, *rw).map(|_| {
                        format!(
                            "{} mounted {} at {}",
                            label(id),
                            if *rw { "read-write" } else { "read-only" },
                            api::mount_dir(id)
                        )
                    })
                }
                Step::Remount { path, rw } => remount(system, path, *rw).map(|_| {
                    format!(
                        "{path}: now {}",
                        if *rw { "read-write" } else { "read-only" }
                    )
                }),
            };
            match result {
                Ok(line) => log.push(line),
                Err(error) => {
                    log.push(error);
                    self.failed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(key);
                }
            }
        }
        // The target's name as last seen, kept for when it is missing.
        let (target, saved_label) = Self::target(system);
        if let Some(volume) = state.volume(&target)
            && target != api::INTERNAL
            && !volume.label.is_empty()
            && volume.label != saved_label
            && let Err(error) = system.set_record_storage(&target, &volume.label)
        {
            log.push(error);
        }
        // The target's folder, whenever it is mounted read-write.
        if state.target.id != api::INTERNAL {
            let state = self.state(system);
            if state.target.available
                && let Some(volume) = state.volume(&state.target.id)
            {
                let _ = recordings_folder(system, volume);
            }
        }
        log
    }

    /// Mounts an external volume at /media/<id>.
    fn mount(
        &self,
        system: &System,
        device: &str,
        id: &str,
        fs: &str,
        rw: bool,
    ) -> Result<(), String> {
        if !api::valid_id(id) || id == api::INTERNAL || !valid_device_name(device) {
            return Err(format!("{device}: not a volume that can be mounted"));
        }
        let folder = api::mount_dir(id);
        media_folder(system, &folder)?;
        let mut options = vec![if rw { "rw" } else { "ro" }.to_string(), "noatime".into()];
        if matches!(fs, "exfat" | "vfat" | "ntfs") {
            // No owners on these: the service account gets the files.
            match service_ids(system) {
                Some((uid, gid)) => {
                    options.push(format!("uid={uid},gid={gid},dmask=0027,fmask=0137"))
                }
                None if rw => {
                    return Err(
                        "There is no livestage account on this machine to give the drive to."
                            .to_string(),
                    );
                }
                None => {}
            }
        }
        if matches!(fs, "exfat" | "vfat") {
            options.push("iocharset=utf8".into());
        }
        let fstype = if fs == "ntfs" { "ntfs3" } else { fs };
        let result = system.run(
            "mount",
            &[
                "-t",
                fstype,
                "-o",
                &options.join(","),
                &format!("/dev/{device}"),
                &folder,
            ],
        );
        if let Err(error) = result {
            let _ = std::fs::remove_dir(system.path(&folder));
            return Err(format!("Could not mount {device}: {error}"));
        }
        Ok(())
    }

    // ── Requests ────────────────────────────────────────────────────────────

    /// A request from the socket, and the state after it.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn handle(&self, system: &System, request: &Request) -> Reply {
        let result = match request {
            Request::List => Ok(()),
            Request::Use { id } => self.use_target(system, id),
            Request::Eject { id } => self.eject(system, id),
            Request::Format { disk, label } => self.format(system, disk, label),
        };
        let state = self.state(system);
        match result {
            Ok(()) => Reply::ok(state),
            Err(error) => Reply::failed(error, state),
        }
    }

    fn locked(system: &System) -> Result<Lock, String> {
        match Lock::take(system, LOCK_WAIT) {
            Ok(Some(lock)) => Ok(lock),
            Ok(None) => Err(
                "The drives are busy (a drive is being formatted). Try again in a moment."
                    .to_string(),
            ),
            Err(error) => Err(error),
        }
    }

    /// Records to `id` from now on: mounts it read-write and makes its
    /// folder first, then saves the choice.
    pub fn use_target(&self, system: &System, id: &str) -> Result<(), String> {
        if !api::valid_id(id) {
            return Err("That is not a volume on this machine.".to_string());
        }
        let _lock = Self::locked(system)?;
        let mut label = String::new();
        if id != api::INTERNAL {
            let state = self.state(system);
            let volume = state
                .volume(id)
                .ok_or("That drive is not plugged in.")?
                .clone();
            if !volume.supported {
                return Err(format!(
                    "{} cannot be recorded to: LiveStage writes exFAT, FAT32, NTFS and ext4 drives. Format it to use it.",
                    describe(&volume)
                ));
            }
            if volume.ejected {
                let ejected = parse_ejected(&system.read(EJECTED).unwrap_or_default());
                let kept: Ejected = ejected
                    .into_iter()
                    .filter(|(device, _)| *device != volume.device)
                    .collect();
                system.write(EJECTED, &ejected_file(&kept))?;
            }
            let folder = api::mount_dir(id);
            match (volume.mounted, volume.mount_path.as_deref()) {
                (None, _) => self.mount(
                    system,
                    &volume.device,
                    id,
                    volume.fs.as_deref().unwrap_or(""),
                    true,
                )?,
                (Some(Mount::Ro), Some(path)) if path == folder => remount(system, &folder, true)?,
                (Some(Mount::Rw), Some(path)) if path == folder => {}
                (_, path) => {
                    return Err(format!(
                        "{} is mounted at {} by hand; unmount it there first.",
                        describe(&volume),
                        path.unwrap_or("?")
                    ));
                }
            }
            recordings_folder(system, &volume)?;
            label = volume.label.clone();
        }
        system.set_record_storage(id, &label)?;
        // The previous target goes back to read-only.
        self.reconcile(system);
        Ok(())
    }

    /// Unmounts a volume so it can be pulled out; it stays unmounted until
    /// it is.
    pub fn eject(&self, system: &System, id: &str) -> Result<(), String> {
        if id == api::INTERNAL {
            return Err("The internal storage cannot be ejected.".to_string());
        }
        let _lock = Self::locked(system)?;
        let state = self.state(system);
        let volume = state
            .volume(id)
            .ok_or("That drive is not plugged in.")?
            .clone();
        if let Some(path) = &volume.mount_path {
            let _ = system.run("sync", &[]);
            system.run("umount", &[path]).map_err(|error| {
                format!(
                    "{} is in use and cannot be ejected now ({error}).",
                    describe(&volume)
                )
            })?;
            if is_media_folder(path) {
                let _ = std::fs::remove_dir(system.path(path));
            }
        }
        let mut ejected = parse_ejected(&system.read(EJECTED).unwrap_or_default());
        ejected.insert((volume.device.clone(), volume.id.clone()));
        system.write(EJECTED, &ejected_file(&ejected))?;
        Ok(())
    }

    /// Erases a whole disk into one exFAT volume (GPT, one basic-data
    /// partition), then mounts it; the recording target again if it was.
    pub fn format(&self, system: &System, disk: &str, label: &str) -> Result<(), String> {
        api::valid_label(label)?;
        let _lock = Self::locked(system)?;
        let inventory = self.inventory(system);
        let state = Self::state_of(system, &inventory);
        let Some(found) = state.disks.iter().find(|d| d.disk == disk) else {
            return Err(format!("There is no disk {disk} on this machine."));
        };
        if found.system {
            return Err(
                "That disk holds the LiveStage system; it cannot be formatted.".to_string(),
            );
        }
        if state.volume(api::INTERNAL).is_some_and(|v| v.disk == disk) {
            return Err(
                "That disk holds the internal storage; it cannot be formatted.".to_string(),
            );
        }
        let Some(block) = inventory.disks.iter().find(|d| d.name == disk) else {
            return Err(format!("There is no disk {disk} on this machine."));
        };
        let held_target = state.target.id != api::INTERNAL
            && state
                .volumes
                .iter()
                .any(|v| v.disk == disk && v.id == state.target.id);

        // Nothing of it may stay mounted.
        let devices: BTreeSet<&str> = std::iter::once(block.name.as_str())
            .chain(block.parts.iter().map(|p| p.name.as_str()))
            .collect();
        for mount in &inventory.mounts {
            if !mount.device().is_some_and(|d| devices.contains(d)) {
                continue;
            }
            let _ = system.run("sync", &[]);
            system.run("umount", &[&mount.path]).map_err(|error| {
                format!(
                    "{} is in use and cannot be formatted now ({error}).",
                    mount.path
                )
            })?;
            if is_media_folder(&mount.path) {
                let _ = std::fs::remove_dir(system.path(&mount.path));
            }
        }
        let ejected = parse_ejected(&system.read(EJECTED).unwrap_or_default());
        let kept: Ejected = ejected
            .into_iter()
            .filter(|(device, _)| !devices.contains(device.as_str()))
            .collect();
        system.write(EJECTED, &ejected_file(&kept))?;

        let device = format!("/dev/{disk}");
        let table = format!("label: gpt\n,,{BASIC_DATA}\n");
        system
            .run_with_input(
                "sfdisk",
                &[
                    "--quiet",
                    "--wipe",
                    "always",
                    "--wipe-partitions",
                    "always",
                    &device,
                ],
                Some(&table),
            )
            .map_err(|error| format!("Could not partition {disk}: {error}"))?;
        // The kernel's view of the new table, and its device node (mdev
        // makes nodes only when asked this way).
        let _ = system.run("partx", &["-u", &device]);
        let _ = system.run("mdev", &["-s"]);
        self.invalidate();
        if !system.is_live() {
            return Ok(());
        }
        let partition = wait_for_partition(system, disk)
            .ok_or_else(|| format!("The new partition on {disk} did not appear."))?;
        system
            .run("mkfs.exfat", &["-L", label, &format!("/dev/{partition}")])
            .map_err(|error| format!("Could not make the filesystem on {disk}: {error}"))?;
        let _ = system.run("sync", &[]);
        self.invalidate();
        let inventory = self.inventory(system);
        let id = volume_id(inventory.probes.get(&partition));
        if id.is_empty() {
            return Err(format!(
                "{disk} was formatted, but its new volume was not found."
            ));
        }
        self.mount(system, &partition, &id, "exfat", held_target)?;
        if held_target {
            let state = self.state(system);
            if let Some(volume) = state.volume(&id) {
                recordings_folder(system, volume)?;
            }
            system.set_record_storage(&id, label)?;
        }
        self.reconcile(system);
        Ok(())
    }

    /// The folder recordings should go to now, for the service's start: the
    /// target mounted first if it is plugged in, else the internal folder.
    /// Made if missing.
    pub fn recordings_path(&self, system: &System) -> String {
        let lock = Lock::take(system, LOCK_WAIT).ok().flatten();
        if lock.is_some() {
            self.reconcile(system);
        }
        let state = self.state(system);
        let dir = state.target.recordings_dir;
        if let Err(error) = std::fs::create_dir_all(system.path(&dir)) {
            eprintln!("livestage-setup: {dir}: {error}");
        }
        drop(lock);
        dir
    }
}

/// `LABEL (model)` or the device, for messages.
fn describe(volume: &Volume) -> String {
    let name = if volume.label.is_empty() {
        volume.device.clone()
    } else {
        volume.label.clone()
    };
    match &volume.model {
        Some(model) => format!("{name} ({model})"),
        None => name,
    }
}

fn remount(system: &System, path: &str, rw: bool) -> Result<(), String> {
    let options = if rw { "remount,rw" } else { "remount,ro" };
    system
        .run("mount", &["-o", options, path])
        .map(|_| ())
        .map_err(|error| format!("Could not remount {path}: {error}"))
}

/// The recordings folder on an external volume, the service's on ext4.
fn recordings_folder(system: &System, volume: &Volume) -> Result<(), String> {
    let dir = api::recordings_dir(&volume.id);
    let path = system.path(&dir);
    std::fs::create_dir_all(&path).map_err(|e| format!("Could not make {dir}: {e}"))?;
    #[cfg(unix)]
    if system.is_live()
        && volume.fs.as_deref() == Some("ext4")
        && let Some((uid, gid)) = service_ids(system)
    {
        std::os::unix::fs::chown(&path, Some(uid), Some(gid))
            .map_err(|e| format!("Could not give {dir} to livestage: {e}"))?;
    }
    Ok(())
}

/// Makes /media/<id>. /media is on the read-only system: when nothing is
/// mounted there yet, a small tmpfs goes on it first.
fn media_folder(system: &System, folder: &str) -> Result<(), String> {
    let path = system.path(folder);
    match std::fs::create_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == std::io::ErrorKind::ReadOnlyFilesystem && system.is_live() =>
        {
            system
                .run(
                    "mount",
                    &[
                        "-t",
                        "tmpfs",
                        "-o",
                        "mode=0755,size=1m,nosuid,nodev,noexec",
                        "tmpfs",
                        api::MEDIA,
                    ],
                )
                .map_err(|e| format!("Could not make {}: {e}", api::MEDIA))?;
            std::fs::create_dir_all(&path).map_err(|e| format!("Could not make {folder}: {e}"))
        }
        Err(error) => Err(format!("Could not make {folder}: {error}")),
    }
}

/// The first partition of `disk` once the kernel and mdev have it (up to
/// a few seconds).
fn wait_for_partition(system: &System, disk: &str) -> Option<String> {
    for _ in 0..50 {
        let found = read_disks(system)
            .into_iter()
            .find(|d| d.name == disk)
            .and_then(|d| d.parts.into_iter().find(|p| p.number == 1))
            .map(|p| p.name);
        if let Some(name) = found
            && system.path(&format!("/dev/{name}")).exists()
        {
            return Some(name);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let guess = first_partition_name(disk);
    system
        .path(&format!("/dev/{guess}"))
        .exists()
        .then_some(guess)
}

/// The service account's uid and gid.
fn service_ids(system: &System) -> Option<(u32, u32)> {
    let uid = id_in(&system.read("/etc/passwd")?, SERVICE_USER)?;
    let gid = id_in(&system.read("/etc/group")?, SERVICE_USER)?;
    Some((uid, gid))
}

/// The disks in /sys/block, with their partitions.
pub fn read_disks(system: &System) -> Vec<BlockDisk> {
    let text = |path: std::path::PathBuf| {
        std::fs::read_to_string(path)
            .ok()
            .map(|t| t.trim().to_string())
    };
    let number = |path: std::path::PathBuf| text(path).and_then(|t| t.parse::<u64>().ok());
    let mut disks: Vec<BlockDisk> = std::fs::read_dir(system.path("/sys/block"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.path();
            if is_virtual(&name) || !valid_device_name(&name) || !dir.join("device").exists() {
                return None;
            }
            // sysfs counts 512-byte sectors whatever the device's own.
            let size_bytes = number(dir.join("size"))? * 512;
            if size_bytes == 0 {
                return None;
            }
            let mut parts: Vec<BlockPart> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|part| {
                    let path = part.path();
                    let number = number(path.join("partition"))?;
                    let name = part.file_name().to_string_lossy().into_owned();
                    valid_device_name(&name).then_some(())?;
                    Some(BlockPart {
                        number: number as u32,
                        size_bytes: text(path.join("size"))
                            .and_then(|t| t.parse::<u64>().ok())
                            .unwrap_or(0)
                            * 512,
                        dev: text(path.join("dev")).unwrap_or_default(),
                        name,
                    })
                })
                .collect();
            parts.sort_by_key(|p| p.number);
            Some(BlockDisk {
                model: text(dir.join("device/model")).filter(|m| !m.is_empty()),
                removable: text(dir.join("removable")).is_some_and(|r| r == "1"),
                dev: text(dir.join("dev")).unwrap_or_default(),
                size_bytes,
                parts,
                name,
            })
        })
        .collect();
    disks.sort_by(|a, b| a.name.cmp(&b.name));
    disks
}

fn root_dev(system: &System) -> Option<String> {
    if !system.is_live() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/").ok().map(|m| dev_string(m.dev()))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The lock on [`LOCK`], released when dropped.
struct Lock {
    #[cfg(target_os = "linux")]
    _file: std::fs::File,
}

impl Lock {
    /// `Ok(None)`: someone else held it for all of `wait`.
    fn take(system: &System, wait: Duration) -> Result<Option<Lock>, String> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let path = system.path(LOCK);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let start = Instant::now();
            loop {
                // SAFETY: an open descriptor; flock only locks it.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return Ok(Some(Lock { _file: file }));
                }
                if start.elapsed() >= wait {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (system, wait);
            Ok(Some(Lock {}))
        }
    }
}

// ── The service ─────────────────────────────────────────────────────────────

/// `--storage-path`: prints the recordings folder to use now. Never fails:
/// the internal folder is the answer when nothing else is.
pub fn print_path(system: &System) {
    println!("{}", Storage::new().recordings_path(system));
}

/// `--storage-service`: keeps the mounts and answers on the socket, for
/// good.
#[cfg(unix)]
pub fn serve(system: System) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    let gid = system
        .read("/etc/group")
        .and_then(|text| id_in(&text, SERVICE_USER));
    let own = |path: &std::path::Path, mode: u32| -> Result<(), String> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if system.is_live() {
            std::os::unix::fs::chown(path, Some(0), gid)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(())
    };
    let run_dir = system.path(api::RUN_DIR);
    std::fs::create_dir_all(&run_dir).map_err(|e| format!("{}: {e}", run_dir.display()))?;
    own(&run_dir, 0o750)?;
    let socket = system.path(api::SOCKET);
    // Left by a service that did not stop cleanly.
    match std::fs::remove_file(&socket) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("{}: {error}", socket.display()));
        }
        _ => {}
    }
    let listener = UnixListener::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    own(&socket, 0o660)?;
    if gid.is_none() {
        eprintln!(
            "[storage] no livestage group: only root can use {}",
            api::SOCKET
        );
    }
    eprintln!("[storage] listening on {}", api::SOCKET);

    let system = Arc::new(system);
    let storage = Arc::new(Storage::new());
    {
        let (system, storage) = (system.clone(), storage.clone());
        std::thread::Builder::new()
            .name("storage-poll".into())
            .spawn(move || {
                loop {
                    for line in storage.poll(&system) {
                        eprintln!("[storage] {line}");
                    }
                    std::thread::sleep(POLL);
                }
            })
            .map_err(|e| format!("poll thread: {e}"))?;
    }
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("[storage] accept: {error}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let (system, storage) = (system.clone(), storage.clone());
        let spawned = std::thread::Builder::new()
            .name("storage-client".into())
            .spawn(move || {
                if let Err(error) = answer(stream, &system, &storage) {
                    eprintln!("[storage] client: {error}");
                }
            });
        if let Err(error) = spawned {
            eprintln!("[storage] client thread: {error}");
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn serve(_system: System) -> Result<(), String> {
    Err("The storage service runs on the appliance (Linux) only.".to_string())
}

/// One client: a request per line, a reply per line, until it closes.
#[cfg(unix)]
fn answer(
    stream: std::os::unix::net::UnixStream,
    system: &System,
    storage: &Storage,
) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};

    // A client that goes quiet is dropped; a format can take minutes, but
    // that is time spent here, not waiting for the client.
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(&stream);
    let mut writer = &stream;
    loop {
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take(api::MAX_REQUEST as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        let too_long = line.len() > api::MAX_REQUEST;
        let text = String::from_utf8_lossy(&line);
        if !too_long && text.trim().is_empty() {
            continue;
        }
        let reply = if too_long {
            Reply::failed("That request is too long.", storage.state(system))
        } else {
            match serde_json::from_str::<Request>(text.trim()) {
                Ok(request) => {
                    let reply = storage.handle(system, &request);
                    if request != Request::List {
                        match &reply.error {
                            None => eprintln!("[storage] {request:?}: done"),
                            Some(error) => eprintln!("[storage] {request:?}: {error}"),
                        }
                    }
                    reply
                }
                Err(error) => Reply::failed(
                    format!("That request was not understood ({error})."),
                    storage.state(system),
                ),
            }
        };
        let mut out = serde_json::to_string(&reply).map_err(std::io::Error::other)?;
        out.push('\n');
        writer.write_all(out.as_bytes())?;
        if too_long {
            return Ok(());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const BLKID: &str = "/dev/sda1: LABEL_FATBOOT=\"LSBOOT\" LABEL=\"LSBOOT\" UUID=\"5E1A-0B2C\" BLOCK_SIZE=\"512\" TYPE=\"vfat\" PARTLABEL=\"boot\" PARTUUID=\"0f0e\"\n\
/dev/sda2: LABEL=\"lsroot\" UUID=\"6b8f2a1c-1111-4c3d-9e2f-0a1b2c3d4e5f\" BLOCK_SIZE=\"4096\" TYPE=\"ext4\"\n\
/dev/sda3: LABEL=\"lssys\" UUID=\"7c9a3b2d-2222-4c3d-9e2f-0a1b2c3d4e5f\" BLOCK_SIZE=\"4096\" TYPE=\"ext4\"\n\
/dev/sda4: LABEL=\"lsdata\" UUID=\"89AB-CDEF\" BLOCK_SIZE=\"512\" TYPE=\"exfat\"\n\
/dev/sdb1: LABEL=\"SHOW \\\"A\\\"\" UUID=\"1A2B-3C4D\" BLOCK_SIZE=\"512\" TYPE=\"exfat\" PARTUUID=\"9a\"\n\
/dev/sdc1: UUID=\"0123456789ABCDEF\" TYPE=\"ntfs\"\n\
/dev/sdc2: LABEL=\"Mac\" UUID=\"4d6f-6163\" TYPE=\"hfsplus\"\n\
/dev/sdd: LABEL=\"FLOPPY\" UUID=\"AAAA-BBBB\" TYPE=\"vfat\"\n\
/dev/sde1: UUID=\"../../etc\" TYPE=\"ext4\"\n";

    fn put(root: &std::path::Path, path: &str, text: &str) {
        let path = root.join(path.trim_start_matches('/'));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn disk(
        root: &std::path::Path,
        name: &str,
        model: &str,
        gb: u64,
        removable: bool,
        parts: &[(&str, u32, u64)],
    ) {
        let dir = format!("/sys/block/{name}");
        put(
            root,
            &format!("{dir}/size"),
            &format!("{}\n", gb * 1_000_000_000 / 512),
        );
        put(
            root,
            &format!("{dir}/removable"),
            if removable { "1\n" } else { "0\n" },
        );
        put(root, &format!("{dir}/dev"), "8:0\n");
        if model.is_empty() {
            put(root, &format!("{dir}/device/uevent"), "");
        } else {
            put(
                root,
                &format!("{dir}/device/model"),
                &format!("{model}  \n"),
            );
        }
        put(root, &format!("/sys/class/block/{name}/size"), "1\n");
        for (part, number, mb) in parts {
            put(
                root,
                &format!("{dir}/{part}/partition"),
                &format!("{number}\n"),
            );
            put(
                root,
                &format!("{dir}/{part}/size"),
                &format!("{}\n", mb * 1_000_000 / 512),
            );
            put(root, &format!("{dir}/{part}/dev"), &format!("8:{number}\n"));
            put(root, &format!("/sys/class/block/{part}/size"), "1\n");
        }
    }

    /// A machine: the system on sda, a stick (sdb), a Windows disk with an
    /// HFS+ partition (sdc), a superfloppy (sdd), a crafted UUID (sde), a
    /// blank disk (sdf) and a loop device.
    pub fn machine(name: &str) -> (System, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("livestage-storage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        disk(
            &root,
            "sda",
            "QEMU HARDDISK",
            32,
            false,
            &[
                ("sda1", 1, 64),
                ("sda2", 2, 1024),
                ("sda3", 3, 32),
                ("sda4", 4, 30_000),
            ],
        );
        disk(
            &root,
            "sdb",
            "SanDisk Ultra",
            64,
            true,
            &[("sdb1", 1, 64_000)],
        );
        disk(
            &root,
            "sdc",
            "Elements 2620",
            2000,
            false,
            &[("sdc1", 1, 1_000_000), ("sdc2", 2, 999_000), ("sdc5", 5, 0)],
        );
        disk(&root, "sdd", "", 1, true, &[]);
        disk(&root, "sde", "Odd", 8, true, &[("sde1", 1, 8_000)]);
        disk(&root, "sdf", "Blank", 16, true, &[]);
        put(&root, "/sys/block/loop0/size", "100\n");
        put(&root, BLKID_SAMPLE, BLKID);
        put(
            &root,
            "/proc/mounts",
            "/dev/sda2 / ext4 ro,noatime 0 0\n\
             /dev/sda3 /var/lib/livestage ext4 rw,noatime 0 0\n\
             /dev/sda4 /data exfat rw,noatime,uid=100,gid=101 0 0\n\
             /dev/sdb1 /media/1A2B-3C4D exfat ro,noatime 0 0\n\
             /dev/sdx1 /media/DEAD-BEEF vfat ro 0 0\n\
             tmpfs /run tmpfs rw 0 0\n",
        );
        put(
            &root,
            "/etc/passwd",
            "root:x:0:0::/root:/bin/sh\nlivestage:x:100:101::/data/livestage:/sbin/nologin\n",
        );
        put(
            &root,
            "/etc/group",
            "root:x:0:\nlivestage:x:101:\naudio:x:18:livestage\n",
        );
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("media/1A2B-3C4D")).unwrap();
        std::fs::create_dir_all(root.join("media/OLD-0000")).unwrap();
        (System::new(Some(root.clone())), root)
    }

    #[test]
    fn blkid_lines_give_label_uuid_and_type() {
        let probes = parse_blkid(BLKID);
        assert_eq!(probes["sda1"].label, "LSBOOT");
        assert_eq!(probes["sda4"].fs, "exfat");
        assert_eq!(probes["sdb1"].label, "SHOW \"A\"");
        assert_eq!(probes["sdb1"].uuid, "1A2B-3C4D");
        assert_eq!(probes["sdc1"].label, "");
        assert_eq!(probes["sdd"].fs, "vfat");
        assert!(parse_blkid("/dev/mapper/x: TYPE=\"ext4\"\n").is_empty());
        assert!(parse_blkid("garbage\n").is_empty());
    }

    #[test]
    fn mounts_unescape_their_paths() {
        let mounts = parse_mounts("/dev/sdb1 /media/My\\040Disk exfat rw,noatime 0 0\n");
        assert_eq!(mounts[0].path, "/media/My Disk");
        assert_eq!(mounts[0].device(), Some("sdb1"));
        assert!(mounts[0].rw);
    }

    #[test]
    fn devices_split_like_linux() {
        // 8:2 (sda2) and 259:1 (an NVMe partition), as stat gives them.
        assert_eq!(dev_string((8 << 8) | 2), "8:2");
        assert_eq!(dev_string((259 << 8) | 1), "259:1");
        assert_eq!(first_partition_name("sdb"), "sdb1");
        assert_eq!(first_partition_name("nvme0n1"), "nvme0n1p1");
        assert_eq!(first_partition_name("mmcblk0"), "mmcblk0p1");
    }

    #[test]
    fn the_machine_gives_volumes_disks_and_the_target() {
        let (system, root) = machine("state");
        let storage = Storage::new();
        let inventory = storage.inventory(&system);
        assert_eq!(inventory.system_disk().unwrap().name, "sda");
        assert_eq!(inventory.internal().unwrap().0.name, "sda4");
        let state = storage.state(&system);
        let ids: Vec<(&str, &str, bool)> = state
            .volumes
            .iter()
            .map(|v| (v.device.as_str(), v.id.as_str(), v.supported))
            .collect();
        assert_eq!(
            ids,
            [
                ("sda4", "internal", true),
                ("sdb1", "1A2B-3C4D", true),
                ("sdc1", "0123456789ABCDEF", true),
                ("sdc2", "4d6f-6163", false),
                ("sdd", "AAAA-BBBB", true),
                ("sde1", "", false),
            ]
        );
        let internal = &state.volumes[0];
        assert_eq!(internal.label, "lsdata");
        assert_eq!(internal.mounted, Some(Mount::Rw));
        assert_eq!(internal.mount_path.as_deref(), Some("/data"));
        assert_eq!(internal.model.as_deref(), Some("QEMU HARDDISK"));
        let stick = state.volume("1A2B-3C4D").unwrap();
        assert_eq!(stick.mounted, Some(Mount::Ro));
        assert_eq!(stick.model.as_deref(), Some("SanDisk Ultra"));
        assert_eq!(stick.size_bytes, 64_000 * 1_000_000 / 512 * 512);
        assert_eq!(state.volume("AAAA-BBBB").unwrap().model, None);
        // Disks: the system flagged, the loop device left out.
        let disks: Vec<(&str, bool, bool)> = state
            .disks
            .iter()
            .map(|d| (d.disk.as_str(), d.system, d.removable))
            .collect();
        assert_eq!(
            disks,
            [
                ("sda", true, false),
                ("sdb", false, true),
                ("sdc", false, false),
                ("sdd", false, true),
                ("sde", false, true),
                ("sdf", false, true),
            ]
        );
        assert_eq!(state.target, Target::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_policy_mounts_what_it_should() {
        let (system, root) = machine("plan");
        let storage = Storage::new();
        let inventory = storage.inventory(&system);
        let free = |_: &str| None;
        let media = vec!["1A2B-3C4D".to_string(), "OLD-0000".to_string()];

        // Recording internally: every supported volume read-only; the stale
        // mount and the empty folder go.
        let state = build_state(&inventory, "internal", "", &Ejected::new(), &free);
        let steps = plan(&state, &inventory, &media);
        assert_eq!(
            steps,
            [
                Step::Detach {
                    path: "/media/DEAD-BEEF".into()
                },
                Step::RemoveDir {
                    path: "/media/DEAD-BEEF".into()
                },
                Step::RemoveDir {
                    path: "/media/OLD-0000".into()
                },
                Step::Mount {
                    device: "sdc1".into(),
                    id: "0123456789ABCDEF".into(),
                    fs: "ntfs".into(),
                    rw: false
                },
                Step::Mount {
                    device: "sdd".into(),
                    id: "AAAA-BBBB".into(),
                    fs: "vfat".into(),
                    rw: false
                },
            ]
        );

        // The stick is the target: read-write. Ejected: left alone.
        let state = build_state(&inventory, "1A2B-3C4D", "", &Ejected::new(), &free);
        assert!(!state.target.available);
        assert_eq!(state.target.label, "SHOW \"A\"");
        assert_eq!(state.target.recordings_dir, api::INTERNAL_RECORDINGS);
        assert!(plan(&state, &inventory, &[]).contains(&Step::Remount {
            path: "/media/1A2B-3C4D".into(),
            rw: true
        }));
        let ejected: Ejected = [("sdd".to_string(), "AAAA-BBBB".to_string())].into();
        let state = build_state(&inventory, "1A2B-3C4D", "", &ejected, &free);
        assert!(state.volume("AAAA-BBBB").unwrap().ejected);
        assert!(
            !plan(&state, &inventory, &[])
                .iter()
                .any(|s| matches!(s, Step::Mount { device, .. } if device == "sdd"))
        );

        // Once it is read-write, it is where recordings go.
        let mut mounted = inventory.clone();
        mounted.mounts[3].rw = true;
        let state = build_state(&mounted, "1A2B-3C4D", "", &Ejected::new(), &free);
        assert!(state.target.available);
        assert_eq!(
            state.target.recordings_dir,
            "/media/1A2B-3C4D/LiveStage Recordings"
        );
        assert!(!state.falling_back());
        // Another drive took sdb's name: the old mount is stale.
        let mut swapped = mounted.clone();
        swapped.probes.get_mut("sdb1").unwrap().uuid = "5555-6666".into();
        let state = build_state(&swapped, "1A2B-3C4D", "", &Ejected::new(), &free);
        assert!(state.falling_back());
        // No name ever known for it: its UUID.
        assert_eq!(state.target.label, "1A2B-3C4D");
        // Its name as saved when it was last seen.
        let state = build_state(&swapped, "1A2B-3C4D", "SHOW", &Ejected::new(), &free);
        assert_eq!(state.target.label, "SHOW");
        assert!(state.falling_back());
        let steps = plan(&state, &swapped, &[]);
        assert!(steps.contains(&Step::Detach {
            path: "/media/1A2B-3C4D".into()
        }));
        assert!(steps.contains(&Step::Mount {
            device: "sdb1".into(),
            id: "5555-6666".into(),
            fs: "exfat".into(),
            rw: false
        }));
        // Ejected memory ends when the drive is gone or changed.
        let ejected: Ejected = [
            ("sdb1".to_string(), "1A2B-3C4D".to_string()),
            ("sdz1".to_string(), "1111-2222".to_string()),
        ]
        .into();
        assert_eq!(prune_ejected(&ejected, &inventory).len(), 1);
        assert!(prune_ejected(&ejected, &swapped).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn requests_are_checked_against_what_is_there() {
        let (system, root) = machine("requests");
        let storage = Storage::new();
        let failed = |request: Request| storage.handle(&system, &request).error.unwrap_or_default();
        assert!(
            failed(Request::Eject {
                id: "internal".into()
            })
            .contains("cannot be ejected")
        );
        assert!(
            failed(Request::Eject {
                id: "9999-9999".into()
            })
            .contains("not plugged in")
        );
        assert!(
            failed(Request::Use {
                id: "../../etc".into()
            })
            .contains("not a volume")
        );
        assert!(
            failed(Request::Use {
                id: "4d6f-6163".into()
            })
            .contains("cannot be recorded to")
        );
        assert!(
            failed(Request::Format {
                disk: "sda".into(),
                label: "X".into()
            })
            .contains("LiveStage system")
        );
        assert!(
            failed(Request::Format {
                disk: "sdq".into(),
                label: "X".into()
            })
            .contains("no disk sdq")
        );
        assert!(
            failed(Request::Format {
                disk: "../sda".into(),
                label: "X".into()
            })
            .contains("no disk")
        );
        assert!(
            failed(Request::Format {
                disk: "sdb".into(),
                label: "TWELVE CHARS".into()
            })
            .contains("11 characters")
        );

        // Use: the choice is saved (the mount is not run under --root).
        let reply = storage.handle(
            &system,
            &Request::Use {
                id: "1A2B-3C4D".into(),
            },
        );
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(system.load_config().unwrap().record_storage, "1A2B-3C4D");
        assert_eq!(
            system.load_config().unwrap().record_storage_label,
            "SHOW \"A\""
        );
        assert!(!system.is_set_up(), "choosing storage is not the setup");
        assert!(root.join("media/1A2B-3C4D/LiveStage Recordings").is_dir());
        assert_eq!(reply.storage.target.id, "1A2B-3C4D");
        // Eject: remembered.
        let reply = storage.handle(
            &system,
            &Request::Eject {
                id: "AAAA-BBBB".into(),
            },
        );
        assert!(reply.ok, "{:?}", reply.error);
        assert!(reply.storage.volume("AAAA-BBBB").unwrap().ejected);
        // Format: refused for the internal storage's disk, done for a stick.
        let reply = storage.handle(
            &system,
            &Request::Format {
                disk: "sdf".into(),
                label: "SHOW".into(),
            },
        );
        assert!(reply.ok, "{:?}", reply.error);
        // Back to internal.
        let reply = storage.handle(
            &system,
            &Request::Use {
                id: "internal".into(),
            },
        );
        assert!(reply.ok);
        assert_eq!(reply.storage.target, Target::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The target keeps its name while it is unplugged: saved by `use`,
    /// brought up to date whenever the volume is seen.
    #[test]
    fn the_target_keeps_its_name_while_unplugged() {
        let (system, root) = machine("label");
        let storage = Storage::new();
        let reply = storage.handle(
            &system,
            &Request::Use {
                id: "1A2B-3C4D".into(),
            },
        );
        assert!(reply.ok, "{:?}", reply.error);
        // Renamed on a computer since: the new name is kept once seen.
        system.set_record_storage("1A2B-3C4D", "OLD NAME").unwrap();
        storage.poll(&system);
        assert_eq!(
            system.load_config().unwrap().record_storage_label,
            "SHOW \"A\""
        );
        // Unplugged: gone from sysfs, blkid and the mounts.
        std::fs::remove_dir_all(root.join("sys/block/sdb")).unwrap();
        std::fs::remove_dir_all(root.join("sys/class/block/sdb1")).unwrap();
        let blkid = BLKID
            .lines()
            .filter(|l| !l.starts_with("/dev/sdb1"))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        put(&root, BLKID_SAMPLE, &blkid);
        let state = storage.state(&system);
        assert!(state.volume("1A2B-3C4D").is_none());
        assert!(state.falling_back());
        assert_eq!(state.target.label, "SHOW \"A\"");
        assert_eq!(state.target.recordings_dir, api::INTERNAL_RECORDINGS);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_path_falls_back_to_the_internal_folder() {
        let (system, root) = machine("path");
        system.set_record_storage("1A2B-3C4D", "SHOW").unwrap();
        let storage = Storage::new();
        // Plugged in but not mounted read-write (nothing runs under --root).
        assert_eq!(storage.recordings_path(&system), api::INTERNAL_RECORDINGS);
        assert!(root.join("data/livestage/recordings").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn passwd_and_group_give_ids() {
        assert_eq!(
            id_in("root:x:0:0\nlivestage:x:100:101::/x:/y\n", "livestage"),
            Some(100)
        );
        assert_eq!(id_in("livestagex:x:5:\n", "livestage"), None);
    }
}
