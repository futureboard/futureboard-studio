//! The block devices as Linux shows them: the disks and partitions in
//! /sys/block, what `blkid` says is on them, and what /proc/mounts says is
//! mounted. Plain readers and parsers, shared by the console's storage
//! (`storage.rs`) and the installer's disk list.

use std::collections::BTreeMap;

use super::system::System;

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
pub fn blkid_pairs(text: &str) -> Vec<(String, String)> {
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
pub fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// A name under /dev as the kernel makes them: `sdb1`, `nvme0n1p2`.
pub fn valid_device_name(name: &str) -> bool {
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
    pub fn device(&self) -> Option<&str> {
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

pub fn unescape_mount(field: &str) -> String {
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
    /// Write-protected (`ro`: a stick's switch, a read-only card).
    pub read_only: bool,
    /// Where the disk sits among the devices (/sys/block's link, resolved):
    /// `/usb` in it is a USB disk.
    pub bus_path: String,
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

/// Devices that are not disks someone plugged in: virtual ones, optical
/// drives, and an eMMC's boot and RPMB areas (`mmcblk0boot0`,
/// `mmcblk0rpmb`), which are parts of the chip rather than disks.
pub fn is_virtual(name: &str) -> bool {
    [
        "loop", "ram", "zram", "sr", "fd", "dm-", "md", "nbd", "mtdblock",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || (name.starts_with("mmcblk") && (name.contains("boot") || name.ends_with("rpmb")))
}

/// The ID of a device as `major:minor` (Linux's `dev_t` layout).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn dev_string(dev: u64) -> String {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    format!("{major}:{minor}")
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
                read_only: text(dir.join("ro")).is_some_and(|r| r == "1"),
                bus_path: std::fs::canonicalize(&dir)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default(),
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

pub fn root_dev(system: &System) -> Option<String> {
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

/// The name of partition `number` of `disk`: `sdb2`, but `nvme0n1p2` and
/// `mmcblk0p2` when the disk's name ends in a digit.
pub fn partition_name(disk: &str, number: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p{number}")
    } else {
        format!("{disk}{number}")
    }
}
