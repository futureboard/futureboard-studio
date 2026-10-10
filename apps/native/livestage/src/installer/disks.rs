//! The disks LiveStage can go onto: every whole disk in /sys/block, what is
//! on it now, and why one cannot be chosen (the installer's own stick, too
//! small, write-protected, in use). Read with the console's block readers
//! ([`blocks`]); decided by [`classify`], a plain function over what was
//! read, which the tests feed canned machines.

use std::collections::BTreeMap;

use super::blocks::{self, BlockDisk, MountEntry, Probe};
use super::system::{self, System};

/// Under `--root`: what `blkid` printed on the machine the tree is from (as
/// the console's storage reads it).
pub const BLKID_SAMPLE: &str = "/blkid.txt";
/// The installer's own root filesystem (on the USB stick it runs from).
pub const INSTALLER_LABEL: &str = "lsinstall";
/// Room a disk needs beyond the image: the data partition grows into the
/// rest at the first boot, and recordings need somewhere to go.
pub const SPARE_BYTES: u64 = 1 << 30;

/// How the disk is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Nvme,
    Sata,
    Usb,
    Virtio,
    Mmc,
    Other,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nvme => "NVMe",
            Self::Sata => "SATA",
            Self::Usb => "USB",
            Self::Virtio => "virtio",
            Self::Mmc => "SD/eMMC",
            Self::Other => "disk",
        }
    }
}

/// From the name the kernel gave the disk and where it sits among the
/// devices (`/usb` in the path: a USB disk, whatever its name).
pub fn transport(name: &str, bus_path: &str) -> Transport {
    if name.starts_with("nvme") {
        Transport::Nvme
    } else if bus_path.contains("/usb") {
        Transport::Usb
    } else if name.starts_with("vd") {
        Transport::Virtio
    } else if name.starts_with("mmcblk") {
        Transport::Mmc
    } else if name.starts_with("sd") {
        // SATA, and SAS or SCSI, which look the same here.
        Transport::Sata
    } else {
        Transport::Other
    }
}

/// A partition, with what `blkid` found on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub number: u32,
    pub size_bytes: u64,
    /// Empty when it has none.
    pub label: String,
    /// `ntfs`, `ext4`, `vfat`, …; empty when nothing was recognised.
    pub fs: String,
}

/// A whole disk, as the installer offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    pub name: String,
    pub model: Option<String>,
    pub size_bytes: u64,
    pub transport: Transport,
    pub removable: bool,
    pub parts: Vec<Part>,
    /// A filesystem on the disk itself, with no partitions (some sticks).
    pub whole: Option<Part>,
    /// What is on it now, for people: `Windows`, `Linux`, `LiveStage`, …
    pub holds: Vec<&'static str>,
    /// The disk the installer runs from.
    pub installer: bool,
    /// Why LiveStage cannot go onto it; `None` when it can.
    pub problem: Option<String>,
}

impl Disk {
    pub fn available(&self) -> bool {
        self.problem.is_none()
    }

    pub fn device(&self) -> String {
        format!("/dev/{}", self.name)
    }

    /// `vda, QEMU HARDDISK (8.6 GB, virtio)`.
    pub fn title(&self) -> String {
        let mut text = self.name.clone();
        if let Some(model) = &self.model {
            text.push_str(&format!(", {model}"));
        }
        text.push_str(&format!(
            " ({}, {})",
            system::megabytes(self.size_bytes),
            self.transport.label()
        ));
        text
    }

    /// What is on it, in a few words.
    pub fn holds_text(&self) -> String {
        if !self.holds.is_empty() {
            return self.holds.join(", ");
        }
        if self.parts.is_empty() && self.whole.is_none() {
            "empty".to_string()
        } else if self
            .parts
            .iter()
            .chain(self.whole.iter())
            .all(|p| p.fs.is_empty())
        {
            "partitions, nothing recognised".to_string()
        } else {
            "files".to_string()
        }
    }

    /// The short reason in the list, for a disk that cannot be chosen.
    pub fn problem_short(&self) -> &'static str {
        match &self.problem {
            None => "",
            Some(_) if self.installer => "installer",
            Some(problem) if problem.starts_with("Too small") => "too small",
            Some(problem) if problem.starts_with("Write-protected") => "read-only",
            Some(_) => "in use",
        }
    }
}

/// `filesystem (label)` of one partition, for lists.
pub fn describe_part(part: &Part) -> String {
    let fs = match part.fs.as_str() {
        "" => "-".to_string(),
        "vfat" => "FAT32".to_string(),
        "exfat" => "exFAT".to_string(),
        "ntfs" => "NTFS".to_string(),
        other => other.to_string(),
    };
    let mut text = format!(
        "{:<12} {:>8}  {fs}",
        part.name,
        system::megabytes(part.size_bytes)
    );
    if !part.label.is_empty() {
        text.push_str(&format!("  \"{}\"", part.label));
    }
    text
}

/// What was read from the machine at one moment.
pub struct Facts<'a> {
    pub disks: &'a [BlockDisk],
    pub probes: &'a BTreeMap<String, Probe>,
    pub mounts: &'a [MountEntry],
    /// Devices in /proc/swaps (`sda2`).
    pub swaps: &'a [String],
    /// `major:minor` of the device `/` is on.
    pub root_dev: Option<&'a str>,
    /// The image's size plus [`SPARE_BYTES`].
    pub min_bytes: u64,
}

/// /proc/swaps: a header, then `/dev/sda2  partition  …` per line.
pub fn parse_swaps(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|device| device.strip_prefix("/dev/"))
        .map(str::to_string)
        .collect()
}

/// The disk the installer runs from: the one `/` is on (by device number,
/// else by /proc/mounts), else the one with the installer's label.
fn own_disk(facts: &Facts) -> Option<String> {
    let names = |disk: &BlockDisk| {
        std::iter::once((disk.name.clone(), disk.dev.clone()))
            .chain(disk.parts.iter().map(|p| (p.name.clone(), p.dev.clone())))
            .collect::<Vec<_>>()
    };
    if let Some(root) = facts.root_dev
        && let Some(disk) = facts
            .disks
            .iter()
            .find(|d| names(d).iter().any(|(_, dev)| dev == root))
    {
        return Some(disk.name.clone());
    }
    if let Some(device) = facts
        .mounts
        .iter()
        .find(|m| m.path == "/")
        .and_then(|m| m.device())
        && let Some(disk) = facts
            .disks
            .iter()
            .find(|d| names(d).iter().any(|(name, _)| name == device))
    {
        return Some(disk.name.clone());
    }
    facts
        .disks
        .iter()
        .find(|d| {
            names(d).iter().any(|(name, _)| {
                facts
                    .probes
                    .get(name)
                    .is_some_and(|p| p.label.eq_ignore_ascii_case(INSTALLER_LABEL))
            })
        })
        .map(|d| d.name.clone())
}

/// What a filesystem says about the system on the disk.
fn system_of(probe: &Probe) -> Option<&'static str> {
    match probe.label.as_str() {
        "lsroot" | "lssys" | "lsdata" | "LSBOOT" => return Some("LiveStage"),
        label if label.eq_ignore_ascii_case(INSTALLER_LABEL) => {
            return Some("LiveStage installer");
        }
        _ => {}
    }
    match probe.fs.as_str() {
        "ntfs" | "BitLocker" | "ReFS" => Some("Windows"),
        "ext2" | "ext3" | "ext4" | "btrfs" | "xfs" | "f2fs" | "swap" | "LVM2_member"
        | "crypto_LUKS" | "zfs_member" => Some("Linux"),
        "hfsplus" | "apfs" => Some("macOS"),
        _ => None,
    }
}

/// Every whole disk, each with what is on it and whether LiveStage can go
/// onto it: those it can first, then the others with the reason.
pub fn classify(facts: &Facts) -> Vec<Disk> {
    let own = own_disk(facts);
    let mut disks: Vec<Disk> = facts
        .disks
        .iter()
        .map(|block| {
            let probe = |name: &str| facts.probes.get(name).cloned().unwrap_or_default();
            let parts: Vec<Part> = block
                .parts
                .iter()
                .map(|p| {
                    let found = probe(&p.name);
                    Part {
                        name: p.name.clone(),
                        number: p.number,
                        size_bytes: p.size_bytes,
                        label: found.label,
                        fs: found.fs,
                    }
                })
                .collect();
            let whole = facts.probes.get(&block.name).map(|found| Part {
                name: block.name.clone(),
                number: 0,
                size_bytes: block.size_bytes,
                label: found.label.clone(),
                fs: found.fs.clone(),
            });
            let mut holds: Vec<&'static str> = Vec::new();
            let devices: Vec<&str> = std::iter::once(block.name.as_str())
                .chain(block.parts.iter().map(|p| p.name.as_str()))
                .collect();
            for device in &devices {
                if let Some(what) = facts.probes.get(*device).and_then(system_of)
                    && !holds.contains(&what)
                {
                    holds.push(what);
                }
            }
            // An installer stick is LiveStage's too, but not LiveStage.
            if holds.contains(&"LiveStage installer") {
                holds.retain(|h| *h != "LiveStage");
            }
            let installer = own.as_deref() == Some(block.name.as_str());
            let mounted = facts
                .mounts
                .iter()
                .find(|m| m.device().is_some_and(|d| devices.contains(&d)));
            let swap = facts
                .swaps
                .iter()
                .find(|s| devices.contains(&s.as_str()));
            let problem = if installer {
                Some("This is the installer's USB stick: LiveStage cannot go onto the disk it is installed from.".to_string())
            } else if block.read_only {
                Some("Write-protected: the disk (or its switch) does not allow writing.".to_string())
            } else if block.size_bytes < facts.min_bytes {
                Some(format!(
                    "Too small: LiveStage needs at least {}.",
                    system::megabytes(facts.min_bytes)
                ))
            } else if let Some(mount) = mounted {
                Some(format!(
                    "In use: /dev/{} is mounted at {}. Unmount it from the shell first.",
                    mount.device().unwrap_or("?"),
                    mount.path
                ))
            } else {
                swap.map(|swap| format!("In use: /dev/{swap} is used as swap (swapoff it first)."))
            };
            Disk {
                name: block.name.clone(),
                model: block.model.clone(),
                size_bytes: block.size_bytes,
                transport: transport(&block.name, &block.bus_path),
                removable: block.removable,
                parts,
                whole: if block.parts.is_empty() { whole } else { None },
                holds,
                installer,
                problem,
            }
        })
        .collect();
    disks.sort_by(|a, b| {
        b.available()
            .cmp(&a.available())
            .then_with(|| a.name.cmp(&b.name))
    });
    disks
}

/// The disks as they are now.
pub fn discover(system: &System, min_bytes: u64) -> Vec<Disk> {
    let block_disks = blocks::read_disks(system);
    let blkid = if system.is_live() {
        // No cache: a disk formatted since must show as it is.
        system
            .run("blkid", &["-c", "/dev/null"])
            .unwrap_or_default()
    } else {
        system.read(BLKID_SAMPLE).unwrap_or_default()
    };
    let probes = blocks::parse_blkid(&blkid);
    let mounts = blocks::parse_mounts(&system.read("/proc/mounts").unwrap_or_default());
    let swaps = parse_swaps(&system.read("/proc/swaps").unwrap_or_default());
    let root = blocks::root_dev(system);
    classify(&Facts {
        disks: &block_disks,
        probes: &probes,
        mounts: &mounts,
        swaps: &swaps,
        root_dev: root.as_deref(),
        min_bytes,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn put(root: &std::path::Path, path: &str, text: &str) {
        let path = root.join(path.trim_start_matches('/'));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A disk in a fake /sys: `parts` are (name, number, MB).
    pub fn disk(
        root: &std::path::Path,
        name: &str,
        model: &str,
        mb: u64,
        read_only: bool,
        parts: &[(&str, u32, u64)],
    ) {
        let dir = format!("/sys/block/{name}");
        put(
            root,
            &format!("{dir}/size"),
            &format!("{}\n", mb * 1_000_000 / 512),
        );
        put(root, &format!("{dir}/removable"), "0\n");
        put(
            root,
            &format!("{dir}/ro"),
            if read_only { "1\n" } else { "0\n" },
        );
        put(root, &format!("{dir}/dev"), "8:0\n");
        put(root, &format!("{dir}/device/model"), &format!("{model}\n"));
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
        }
    }

    /// A computer with the installer on sda (a USB stick), an NVMe disk with
    /// Windows, an empty virtio disk, a small card, a write-protected stick,
    /// a disk in use, an eMMC's boot area and a loop device.
    pub fn computer(name: &str) -> (System, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("livestage-installer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        disk(
            &root,
            "sda",
            "Flash Drive",
            16_000,
            false,
            &[("sda1", 1, 128), ("sda2", 2, 1_000)],
        );
        disk(
            &root,
            "nvme0n1",
            "Samsung SSD 980",
            500_000,
            false,
            &[
                ("nvme0n1p1", 1, 100),
                ("nvme0n1p2", 2, 16),
                ("nvme0n1p3", 3, 499_000),
            ],
        );
        disk(&root, "vda", "", 8_000, false, &[]);
        disk(
            &root,
            "mmcblk0",
            "SD16G",
            1_000,
            false,
            &[("mmcblk0p1", 1, 1_000)],
        );
        disk(&root, "sdb", "Locked Stick", 32_000, true, &[]);
        disk(
            &root,
            "sdc",
            "Elements",
            2_000_000,
            false,
            &[("sdc1", 1, 2_000_000)],
        );
        disk(&root, "mmcblk1boot0", "", 4, false, &[]);
        put(&root, "/sys/block/loop0/size", "100\n");
        put(
            &root,
            BLKID_SAMPLE,
            "/dev/sda1: LABEL=\"LSINSTALL\" UUID=\"1111-2222\" TYPE=\"vfat\"\n\
             /dev/sda2: LABEL=\"lsinstall\" UUID=\"aaaa\" TYPE=\"ext4\"\n\
             /dev/nvme0n1p1: UUID=\"3333-4444\" TYPE=\"vfat\"\n\
             /dev/nvme0n1p3: LABEL=\"Windows\" UUID=\"0123\" TYPE=\"ntfs\"\n\
             /dev/mmcblk0p1: LABEL=\"lsdata\" UUID=\"5555-6666\" TYPE=\"exfat\"\n\
             /dev/sdc1: LABEL=\"Backup\" UUID=\"7777-8888\" TYPE=\"exfat\"\n",
        );
        put(
            &root,
            "/proc/mounts",
            "/dev/sda2 / ext4 ro,noatime 0 0\n\
             /dev/sdc1 /mnt/backup exfat rw 0 0\n\
             tmpfs /run tmpfs rw 0 0\n",
        );
        put(
            &root,
            "/proc/swaps",
            "Filename\tType\tSize\tUsed\tPriority\n",
        );
        (System::new(Some(root.clone())), root)
    }

    #[test]
    fn the_disks_say_what_is_on_them_and_why_not() {
        let (system, root) = computer("discover");
        let disks = discover(&system, 1_250_000_000 + SPARE_BYTES);
        let names: Vec<&str> = disks.iter().map(|d| d.name.as_str()).collect();
        // Available first; the eMMC's boot area and the loop device not at all.
        assert_eq!(
            names,
            ["nvme0n1", "vda", "mmcblk0", "sda", "sdb", "sdc"],
            "{disks:#?}"
        );
        let by = |name: &str| disks.iter().find(|d| d.name == name).unwrap();

        let nvme = by("nvme0n1");
        assert!(nvme.available());
        assert_eq!(nvme.transport, Transport::Nvme);
        assert_eq!(nvme.holds, ["Windows"]);
        assert_eq!(nvme.parts.len(), 3);
        assert_eq!(nvme.parts[2].label, "Windows");
        assert_eq!(nvme.title(), "nvme0n1, Samsung SSD 980 (500 GB, NVMe)");

        let vda = by("vda");
        assert!(vda.available());
        assert_eq!(vda.transport, Transport::Virtio);
        assert_eq!(vda.holds_text(), "empty");
        assert_eq!(vda.model, None);

        let sda = by("sda");
        assert!(sda.installer);
        assert_eq!(sda.problem_short(), "installer");
        assert_eq!(sda.holds, ["LiveStage installer"]);

        let card = by("mmcblk0");
        assert_eq!(card.problem_short(), "too small");
        assert!(
            card.problem.as_ref().unwrap().contains("2.3 GB"),
            "{card:?}"
        );
        assert_eq!(card.holds, ["LiveStage"]);
        assert_eq!(card.transport, Transport::Mmc);

        assert_eq!(by("sdb").problem_short(), "read-only");
        let backup = by("sdc");
        assert_eq!(backup.problem_short(), "in use");
        assert!(
            backup
                .problem
                .as_ref()
                .unwrap()
                .contains("/dev/sdc1 is mounted at /mnt/backup"),
            "{backup:?}"
        );
        assert_eq!(backup.holds_text(), "files");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_installer_disk_is_found_by_device_number_mount_or_label() {
        let disk = |name: &str, dev: &str, part: (&str, &str)| BlockDisk {
            name: name.into(),
            model: None,
            size_bytes: 64 << 30,
            removable: true,
            read_only: false,
            bus_path: String::new(),
            dev: dev.into(),
            parts: vec![blocks::BlockPart {
                name: part.0.into(),
                number: 1,
                size_bytes: 1 << 30,
                dev: part.1.into(),
            }],
        };
        let disks = [
            disk("sda", "8:0", ("sda1", "8:1")),
            disk("sdb", "8:16", ("sdb1", "8:17")),
        ];
        let mut probes = BTreeMap::new();
        probes.insert(
            "sda1".to_string(),
            Probe {
                label: "lsinstall".into(),
                ..Probe::default()
            },
        );
        let mounts = blocks::parse_mounts("/dev/sdb1 / ext4 ro 0 0\n");
        let facts = |root_dev, mounts: &'static [MountEntry]| Facts {
            disks: &disks,
            probes: &probes,
            mounts,
            swaps: &[],
            root_dev,
            min_bytes: 0,
        };
        // The device number wins, then the mount, then the label.
        assert_eq!(own_disk(&facts(Some("8:17"), &[])).as_deref(), Some("sdb"));
        let leaked: &'static [MountEntry] = Box::leak(mounts.into_boxed_slice());
        assert_eq!(own_disk(&facts(None, leaked)).as_deref(), Some("sdb"));
        assert_eq!(own_disk(&facts(None, &[])).as_deref(), Some("sda"));
        // Swap counts as in use.
        let swaps = parse_swaps("Filename Type Size Used Priority\n/dev/sdb1 partition 1 0 -2\n");
        assert_eq!(swaps, ["sdb1"]);
        let classified = classify(&Facts {
            swaps: &swaps,
            ..facts(None, &[])
        });
        let sdb = classified.iter().find(|d| d.name == "sdb").unwrap();
        assert!(sdb.problem.as_ref().unwrap().contains("swap"), "{sdb:?}");
    }

    #[test]
    fn the_transport_comes_from_the_name_and_the_bus() {
        assert_eq!(
            transport(
                "nvme0n1",
                "/sys/devices/pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1"
            ),
            Transport::Nvme
        );
        assert_eq!(
            transport(
                "sdb",
                "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb"
            ),
            Transport::Usb
        );
        assert_eq!(
            transport(
                "sda",
                "/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda"
            ),
            Transport::Sata
        );
        assert_eq!(
            transport(
                "vdb",
                "/sys/devices/pci0000:00/0000:00:04.0/virtio1/block/vdb"
            ),
            Transport::Virtio
        );
        assert_eq!(
            transport("mmcblk0", "/sys/devices/platform/mmc/block/mmcblk0"),
            Transport::Mmc
        );
        assert!(blocks::is_virtual("mmcblk0boot1"));
        assert!(blocks::is_virtual("mmcblk0rpmb"));
        assert!(!blocks::is_virtual("mmcblk0"));
        assert_eq!(blocks::partition_name("nvme0n1", 3), "nvme0n1p3");
        assert_eq!(blocks::partition_name("vda", 3), "vda3");
    }
}
