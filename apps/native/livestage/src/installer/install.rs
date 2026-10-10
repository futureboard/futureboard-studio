//! Installing, on a thread of its own: the image onto the whole disk, read
//! back and checked, then the settings onto the new disk's settings
//! partition, the console password onto its system, and a UEFI boot entry.
//! The screen (or the unattended mode's printing) hears how far it is
//! through [`Event`]s.
//!
//! The order keeps a disk that is left half-done from looking like a
//! system: its first MiB (the partition tables) and last MiB (a backup GPT
//! from before) are zeroed first, the image goes on from its second MiB,
//! and its first MiB, with LiveStage's partition table, goes on last, once
//! the rest has been written and synced. The partitions are then found by
//! their number on this disk, never by label: another disk in the computer
//! may carry the same labels.
//!
//! Under `--root` the disk is the file `ROOT/dev/NAME` and no command is
//! run: the tests install onto a file.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::blocks;
use super::config::SetupConfig;
use super::disks::{self, SPARE_BYTES};
use super::payload::{self, Payload};
use super::system::System;

/// What is read and written at a time.
pub const CHUNK: usize = 4 << 20;
/// The image's first MiB: the protective MBR and the GPT.
const HEAD: u64 = 1 << 20;
/// Written data is pushed to the disk this often, so the speed shown is
/// the disk's, not the memory's.
const SYNC_EVERY: u64 = 64 << 20;
/// Progress is told this often.
const REPORT_EVERY: u64 = 8 << 20;
/// Where the new disk's partitions are mounted for a moment.
pub const SYS_MOUNT: &str = "/run/livestage-installer/lssys";
pub const ROOT_MOUNT: &str = "/run/livestage-installer/lsroot";
/// The appliance image's partitions (make-image.sh).
const BOOT_PART: u32 = 1;
const ROOT_PART: u32 = 2;
const SYS_PART: u32 = 3;
const DATA_PART: u32 = 4;
const ROOT_LABEL: &str = "lsroot";
const SYS_LABEL: &str = "lssys";
/// The settings file on the settings partition (`/var/lib/livestage` on
/// the installed system).
const SETUP_FILE: &str = "setup.conf";
/// The UEFI boot entry's name and what it starts.
pub const BOOT_ENTRY: &str = "LiveStage";
const LOADER: &str = "\\EFI\\BOOT\\BOOTX64.EFI";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Prepare,
    Write,
    Flush,
    Verify,
    Partitions,
    Settings,
    Password,
    BootEntry,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Prepare => "Preparing the disk",
            Self::Write => "Writing LiveStage",
            Self::Flush => "Finishing the writing",
            Self::Verify => "Checking what was written",
            Self::Partitions => "Reading the new partitions",
            Self::Settings => "Writing the settings",
            Self::Password => "Setting the console password",
            Self::BootEntry => "Adding a UEFI boot entry",
        }
    }

    /// Has a progress bar.
    pub fn measured(self) -> bool {
        matches!(self, Self::Write | Self::Verify)
    }
}

/// What to install where.
pub struct Job {
    /// The whole disk, as in /dev (`nvme0n1`).
    pub disk: String,
    pub payload: Payload,
    /// The setup's answers for the new disk; `None` leaves the setup to its
    /// first boot.
    pub setup: Option<SetupConfig>,
    /// The console's root password.
    pub password: Option<String>,
}

impl Job {
    pub fn stages(&self) -> Vec<Stage> {
        let mut stages = vec![
            Stage::Prepare,
            Stage::Write,
            Stage::Flush,
            Stage::Verify,
            Stage::Partitions,
        ];
        if self.setup.is_some() {
            stages.push(Stage::Settings);
        }
        if self.password.is_some() {
            stages.push(Stage::Password);
        }
        stages.push(Stage::BootEntry);
        stages
    }
}

pub enum Event {
    Stage(Stage),
    /// Bytes of the image written or checked, in the current stage.
    Progress {
        done: u64,
        total: u64,
    },
    Finished(Result<Report, Failure>),
}

/// How it went, when it worked: anything worth knowing (no boot entry).
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub notes: Vec<String>,
}

/// Where it stopped, why (the error as it came), and what that leaves on
/// the disk.
#[derive(Debug, Clone)]
pub struct Failure {
    pub stage: Stage,
    pub error: String,
    pub state: String,
}

/// Runs `job` on a thread; its events come through the receiver, the last
/// one [`Event::Finished`].
pub fn spawn(system: System, job: Job) -> mpsc::Receiver<Event> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run(&system, &job, &mut |event| {
            let _ = sender.send(event);
        });
        let _ = sender.send(Event::Finished(result));
    });
    receiver
}

// What a failure leaves on the disk.

fn untouched(disk: &str) -> String {
    format!("Nothing was written: {disk} is as it was.")
}

fn erased(disk: &str) -> String {
    format!(
        "{disk} has been erased: its partition table is gone, so what was on it no longer \
         shows and it holds no system. Nothing else was changed. Install again, or choose \
         another disk."
    )
}

fn bad_copy(disk: &str) -> String {
    format!(
        "{disk} holds a damaged copy, so its partition table was erased again: it does not \
         boot. A failing disk, cable or USB stick does this; install again, or choose \
         another disk."
    )
}

fn without_settings(disk: &str) -> String {
    format!(
        "LiveStage is on {disk} and checked, but without these settings: its first boot \
         asks for them."
    )
}

fn without_password(disk: &str) -> String {
    format!(
        "LiveStage is on {disk} with the settings, but the console has no password: set one \
         from the console's setup."
    )
}

/// Does the whole job, telling `send` how far it is.
pub fn run(system: &System, job: &Job, send: &mut dyn FnMut(Event)) -> Result<Report, Failure> {
    let disk = job.disk.as_str();
    let bytes = job.payload.bytes;
    let fail = |stage: Stage, error: String, state: String| Failure {
        stage,
        error,
        state,
    };
    let mut report = Report::default();

    // ── The disk, checked again, then erased ────────────────────────────────
    send(Event::Stage(Stage::Prepare));
    let found = disks::discover(system, bytes + SPARE_BYTES)
        .into_iter()
        .find(|d| d.name == disk);
    match found {
        None => {
            return Err(fail(
                Stage::Prepare,
                format!("There is no disk {disk} on this computer."),
                untouched(disk),
            ));
        }
        Some(found) if !found.available() => {
            return Err(fail(
                Stage::Prepare,
                found.problem.unwrap_or_default(),
                untouched(disk),
            ));
        }
        Some(_) => {}
    }
    let path = system.path(&format!("/dev/{disk}"));
    let shown = format!("/dev/{disk}");
    let mut file = open_disk(&path, system.is_live())
        .map_err(|e| fail(Stage::Prepare, format!("{shown}: {e}"), untouched(disk)))?;
    let size = file
        .seek(SeekFrom::End(0))
        .map_err(|e| fail(Stage::Prepare, format!("{shown}: {e}"), untouched(disk)))?;
    if size < bytes {
        return Err(fail(
            Stage::Prepare,
            format!("{shown} holds {size} bytes; LiveStage's image is {bytes}."),
            untouched(disk),
        ));
    }
    let erase = |file: &mut File| -> std::io::Result<()> {
        zero(file, 0, HEAD.min(size))?;
        // The old backup GPT, at the end of the disk (beyond the image).
        let tail = size.saturating_sub(HEAD) & !4095;
        if tail >= bytes && tail > 0 {
            zero(file, tail, size - tail)?;
        }
        file.sync_all()
    };
    erase(&mut file).map_err(|e| fail(Stage::Prepare, format!("{shown}: {e}"), erased(disk)))?;

    // ── The image ───────────────────────────────────────────────────────────
    send(Event::Stage(Stage::Write));
    let mut source = job
        .payload
        .open()
        .map_err(|e| fail(Stage::Write, e, erased(disk)))?;
    let head = write_image(&mut source, &mut file, &job.payload, &mut |done| {
        send(Event::Progress { done, total: bytes })
    })
    .map_err(|e| fail(Stage::Write, e, erased(disk)))?;
    source
        .finish()
        .map_err(|e| fail(Stage::Write, e, erased(disk)))?;

    send(Event::Stage(Stage::Flush));
    let finish = |file: &mut File| -> std::io::Result<()> {
        file.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&head)?;
        file.sync_all()
    };
    finish(&mut file).map_err(|e| fail(Stage::Flush, format!("{shown}: {e}"), erased(disk)))?;
    drop(file);

    // ── Read back ───────────────────────────────────────────────────────────
    send(Event::Stage(Stage::Verify));
    let digest = read_back(&path, system.is_live(), bytes, &mut |done| {
        send(Event::Progress { done, total: bytes })
    })
    .map_err(|e| fail(Stage::Verify, format!("{shown}: {e}"), erased(disk)))?;
    if digest != job.payload.sha256 {
        // Not bootable rather than half right.
        let state = match open_disk(&path, system.is_live())
            .and_then(|mut file| zero(&mut file, 0, HEAD).and_then(|_| file.sync_all()))
        {
            Ok(()) => bad_copy(disk),
            Err(_) => format!(
                "{disk} holds a damaged copy, and erasing its start failed too: do not boot \
                 it. Install again, or choose another disk."
            ),
        };
        return Err(fail(
            Stage::Verify,
            format!(
                "What was read back from {shown} is not what was written (sha256 {digest}, \
                 expected {}).",
                job.payload.sha256
            ),
            state,
        ));
    }

    // ── The new disk's partitions ───────────────────────────────────────────
    send(Event::Stage(Stage::Partitions));
    let needed = job.setup.is_some() || job.password.is_some();
    if let Err(error) = reread_partitions(system, disk) {
        if needed {
            return Err(fail(Stage::Partitions, error, without_settings(disk)));
        }
        report.notes.push(format!(
            "The new partitions were not read here ({error}); the first boot reads them."
        ));
    }

    if let Some(setup) = &job.setup {
        send(Event::Stage(Stage::Settings));
        write_settings(system, disk, setup)
            .map_err(|e| fail(Stage::Settings, e, without_settings(disk)))?;
    }
    if let Some(password) = &job.password {
        send(Event::Stage(Stage::Password));
        set_password(system, disk, password)
            .map_err(|e| fail(Stage::Password, e, without_password(disk)))?;
    }

    // ── Boot entry (the disk boots without it on most firmware) ─────────────
    send(Event::Stage(Stage::BootEntry));
    if let Err(error) = boot_entry(system, disk) {
        report.notes.push(format!(
            "No UEFI boot entry was added ({error}). Most computers start a disk like this one \
             anyway; if this one does not, choose {disk} in the firmware's boot menu."
        ));
    }
    Ok(report)
}

/// The whole disk, to write. On the machine, exclusively: Linux refuses
/// while anything on it is mounted or in use.
fn open_disk(path: &Path, live: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(target_os = "linux")]
    if live {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_EXCL);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = live;
    options.open(path)
}

fn zero(file: &mut File, at: u64, length: u64) -> std::io::Result<()> {
    let zeros = vec![0u8; length.min(HEAD) as usize];
    let mut done = 0;
    file.seek(SeekFrom::Start(at))?;
    while done < length {
        let n = (length - done).min(zeros.len() as u64) as usize;
        file.write_all(&zeros[..n])?;
        done += n as u64;
    }
    Ok(())
}

/// Fills `buffer` from `source` unless it ends first; the bytes read.
fn read_full(source: &mut dyn Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match source.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Streams the image onto `disk` from its second MiB on, checking it
/// against the manifest as it goes; returns the image's first MiB, for
/// writing last. An image that is not what its manifest says (damaged, cut
/// short, too long) is an error.
pub fn write_image(
    source: &mut dyn Read,
    disk: &mut File,
    payload: &Payload,
    progress: &mut dyn FnMut(u64),
) -> Result<Vec<u8>, String> {
    let bytes = payload.bytes;
    let mut buffer = vec![0u8; CHUNK];
    let mut head = Vec::with_capacity(HEAD as usize);
    let mut hasher = Sha256::new();
    let (mut done, mut unsynced, mut reported) = (0u64, 0u64, 0u64);
    let disk_error = |e: std::io::Error| format!("Writing the disk failed: {e}");
    loop {
        let n = read_full(source, &mut buffer)
            .map_err(|e| format!("Reading the installer's copy of LiveStage failed: {e}"))?;
        if n == 0 {
            break;
        }
        if done + n as u64 > bytes {
            return Err(format!(
                "The installer's copy of LiveStage is longer than its manifest says \
                 ({bytes} bytes): it is damaged."
            ));
        }
        hasher.update(&buffer[..n]);
        let mut rest = &buffer[..n];
        if done < HEAD {
            let take = ((HEAD - done) as usize).min(rest.len());
            head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            done += take as u64;
        }
        if !rest.is_empty() {
            disk.seek(SeekFrom::Start(done)).map_err(disk_error)?;
            disk.write_all(rest).map_err(disk_error)?;
            done += rest.len() as u64;
            unsynced += rest.len() as u64;
        }
        if unsynced >= SYNC_EVERY {
            disk.sync_data().map_err(disk_error)?;
            unsynced = 0;
        }
        if done - reported >= REPORT_EVERY {
            progress(done);
            reported = done;
        }
    }
    if done != bytes {
        return Err(format!(
            "The installer's copy of LiveStage ended after {done} of its {bytes} bytes: it is \
             damaged."
        ));
    }
    let digest = payload::hex(&hasher.finalize());
    if digest != payload.sha256 {
        return Err(format!(
            "The installer's copy of LiveStage is damaged: its sha256 is {digest}, not {}. \
             Write the installer image to the USB stick again.",
            payload.sha256
        ));
    }
    progress(done);
    Ok(head)
}

/// The sha256 of the disk's first `bytes`, read from the disk itself: on
/// the machine past the page cache (O_DIRECT), so it is what the disk
/// holds, not what was just handed to it.
pub fn read_back(
    path: &Path,
    live: bool,
    bytes: u64,
    progress: &mut dyn FnMut(u64),
) -> std::io::Result<String> {
    let (mut file, direct) = open_to_check(path, live)?;
    // O_DIRECT reads into memory aligned to the disk's blocks, in whole
    // blocks: a chunk with room to align it.
    let mut storage = vec![0u8; CHUNK + 4096];
    let offset = storage.as_ptr().align_offset(4096);
    let buffer = &mut storage[offset..offset + CHUNK];
    let mut hasher = Sha256::new();
    let (mut done, mut reported) = (0u64, 0u64);
    while done < bytes {
        let want = (bytes - done).min(CHUNK as u64) as usize;
        let ask = if direct {
            want.div_ceil(4096) * 4096
        } else {
            want
        };
        let n = read_full(&mut file, &mut buffer[..ask])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("the disk ended after {done} bytes"),
            ));
        }
        let take = n.min(want);
        hasher.update(&buffer[..take]);
        done += take as u64;
        if done - reported >= REPORT_EVERY || done == bytes {
            progress(done);
            reported = done;
        }
    }
    Ok(payload::hex(&hasher.finalize()))
}

fn open_to_check(path: &Path, live: bool) -> std::io::Result<(File, bool)> {
    #[cfg(target_os = "linux")]
    if live {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;
        // Whatever the page cache still holds of it goes first.
        if let Ok(file) = File::open(path) {
            // SAFETY: a valid descriptor; advice only.
            unsafe {
                libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            }
        }
        if let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(path)
        {
            return Ok((file, true));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = live;
    Ok((File::open(path)?, false))
}

/// /dev name of partition `number` of `disk`: as sysfs lists it, else as
/// the kernel names them.
fn partition(system: &System, disk: &str, number: u32) -> String {
    blocks::read_disks(system)
        .into_iter()
        .find(|d| d.name == disk)
        .and_then(|d| d.parts.into_iter().find(|p| p.number == number))
        .map(|p| p.name)
        .unwrap_or_else(|| blocks::partition_name(disk, number))
}

/// The kernel's view of the new partition table, their device nodes, and a
/// look that they are LiveStage's.
fn reread_partitions(system: &System, disk: &str) -> Result<(), String> {
    let device = format!("/dev/{disk}");
    if system.run("blockdev", &["--rereadpt", &device]).is_err() {
        system
            .run("partx", &["-u", &device])
            .map_err(|e| format!("The new partition table was not read: {e}"))?;
    }
    if !system.is_live() {
        return Ok(());
    }
    // mdev makes the nodes only when asked this way.
    let _ = system.run("mdev", &["-s"]);
    let ready = || {
        (BOOT_PART..=DATA_PART).all(|number| {
            let name = partition(system, disk, number);
            system.path(&format!("/dev/{name}")).exists()
                && blocks::read_disks(system)
                    .iter()
                    .any(|d| d.name == disk && d.parts.iter().any(|p| p.number == number))
        })
    };
    let mut tries = 0;
    while !ready() {
        tries += 1;
        if tries > 100 {
            return Err(format!("The new partitions of {device} did not appear."));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for (number, label) in [(ROOT_PART, ROOT_LABEL), (SYS_PART, SYS_LABEL)] {
        let part = format!("/dev/{}", partition(system, disk, number));
        let found = system
            .run(
                "blkid",
                &["-c", "/dev/null", "-o", "value", "-s", "LABEL", &part],
            )
            .unwrap_or_default();
        if found.trim() != label {
            return Err(format!(
                "{part} should be LiveStage's {label} but is labelled \"{}\".",
                found.trim()
            ));
        }
    }
    Ok(())
}

/// Mounts partition `number` of `disk` at `mount`, does `work` in it,
/// unmounts it.
fn in_partition(
    system: &System,
    disk: &str,
    number: u32,
    mount: &str,
    work: &mut dyn FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let part = format!("/dev/{}", partition(system, disk, number));
    let dir = system.path(mount);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{mount}: {e}"))?;
    system.run("mount", &["-t", "ext4", "-o", "rw,noatime", &part, mount])?;
    let result = work(&dir);
    let _ = system.run("sync", &[]);
    let unmounted = system.run("umount", &[mount]);
    result?;
    unmounted.map(|_| ())
}

/// `setup.conf` on the new disk's settings partition, as the console's
/// setup saves it: its first boot finds the setup done.
fn write_settings(system: &System, disk: &str, setup: &SetupConfig) -> Result<(), String> {
    let text = setup.to_file();
    in_partition(system, disk, SYS_PART, SYS_MOUNT, &mut |dir| {
        let file = dir.join(SETUP_FILE);
        let temporary = dir.join(format!("{SETUP_FILE}.new"));
        std::fs::write(&temporary, &text).map_err(|e| format!("{}: {e}", temporary.display()))?;
        // It holds the Wi-Fi key: root's alone, as the console leaves it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("{}: {e}", temporary.display()))?;
        }
        std::fs::rename(&temporary, &file).map_err(|e| format!("{}: {e}", file.display()))
    })
}

/// The console's root password on the new system, set by its own
/// `chpasswd` (as the console's setup does).
fn set_password(system: &System, disk: &str, password: &str) -> Result<(), String> {
    in_partition(system, disk, ROOT_PART, ROOT_MOUNT, &mut |_| {
        system
            .run_with_input(
                "chroot",
                &[ROOT_MOUNT, "chpasswd", "-c", "sha512"],
                Some(&format!("root:{password}\n")),
            )
            .map(|_| ())
    })
}

/// A UEFI boot entry for the new disk's `\EFI\BOOT\BOOTX64.EFI`, first in
/// the boot order; earlier LiveStage entries go.
fn boot_entry(system: &System, disk: &str) -> Result<(), String> {
    let device = format!("/dev/{disk}");
    let part = BOOT_PART.to_string();
    let create = [
        "-q", "-c", "-d", &device, "-p", &part, "-L", BOOT_ENTRY, "-l", LOADER,
    ];
    if !system.is_live() {
        return system.run("efibootmgr", &create).map(|_| ());
    }
    if !system.path("/sys/firmware/efi").exists() {
        return Err("the installer was not started by UEFI firmware".to_string());
    }
    let vars = "/sys/firmware/efi/efivars";
    let empty = std::fs::read_dir(system.path(vars))
        .map(|mut d| d.next().is_none())
        .unwrap_or(true);
    if empty {
        let _ = system.run("mount", &["-t", "efivarfs", "efivarfs", vars]);
    }
    let listed = system.run("efibootmgr", &[])?;
    for number in old_entries(&listed) {
        let _ = system.run("efibootmgr", &["-q", "-b", &number, "-B"]);
    }
    system.run("efibootmgr", &create).map(|_| ())
}

/// The numbers of the entries called LiveStage in `efibootmgr`'s list
/// (`Boot0003* LiveStage\tHD(1,GPT,…)`).
pub fn old_entries(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Boot")?;
            let number = rest.get(..4)?;
            if !number.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let label = rest[4..].trim_start_matches('*').trim_start();
            let label = label.split('\t').next().unwrap_or("").trim();
            (label == BOOT_ENTRY).then(|| number.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disks::tests::{disk, put};

    /// An image of `bytes` that is not all zeros anywhere.
    fn image(bytes: usize) -> Vec<u8> {
        (0..bytes)
            .map(|i| (i as u32).wrapping_mul(2654435761).rotate_right(13) as u8)
            .collect()
    }

    /// A computer with an 8 GB disk (vdb, a 6 MiB file standing in for it)
    /// that held something, and an image of `bytes` next to it.
    fn computer(name: &str, bytes: usize) -> (System, std::path::PathBuf, Payload, Vec<u8>) {
        let root =
            std::env::temp_dir().join(format!("livestage-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        disk(&root, "vdb", "Target", 8_000, false, &[("vdb1", 1, 8_000)]);
        put(&root, "/proc/mounts", "tmpfs /run tmpfs rw 0 0\n");
        put(&root, "/dev/vdb", "");
        let old = vec![0xEEu8; 6 << 20];
        std::fs::write(root.join("dev/vdb"), &old).unwrap();
        let image = image(bytes);
        let dir = root.join("payload");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("livestage.img"), &image).unwrap();
        std::fs::write(
            dir.join(payload::MANIFEST),
            payload::manifest("livestage-test", &image),
        )
        .unwrap();
        let payload = Payload::load(&dir.join("livestage.img")).unwrap();
        (System::new(Some(root.clone())), root, payload, image)
    }

    fn collect(system: &System, job: &Job) -> (Result<Report, Failure>, Vec<Stage>, u64) {
        let mut stages = Vec::new();
        let mut last = 0;
        let result = run(system, job, &mut |event| match event {
            Event::Stage(stage) => stages.push(stage),
            Event::Progress { done, .. } => last = done,
            Event::Finished(_) => {}
        });
        (result, stages, last)
    }

    #[test]
    fn the_image_goes_on_whole_with_the_settings() {
        let bytes = 3 << 20;
        let (system, root, payload, image) = computer("whole", bytes);
        let setup = SetupConfig {
            name: "foh-rack".into(),
            ..SetupConfig::default()
        };
        let job = Job {
            disk: "vdb".into(),
            payload,
            setup: Some(setup.clone()),
            password: Some("secret1".into()),
        };
        let (result, stages, last) = collect(&system, &job);
        let report = result.expect("installed");
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        assert_eq!(stages, job.stages());
        assert_eq!(last, bytes as u64);
        let written = std::fs::read(root.join("dev/vdb")).unwrap();
        assert_eq!(&written[..bytes], &image[..]);
        // The old backup GPT's place, at the end, is zeroed; between, as it was.
        assert!(written[written.len() - (1 << 20)..].iter().all(|b| *b == 0));
        assert_eq!(written[bytes], 0xEE);
        let saved = std::fs::read_to_string(system.path(SYS_MOUNT).join(SETUP_FILE)).unwrap();
        assert_eq!(saved, setup.to_file());
        assert_eq!(SetupConfig::parse(&saved).name, "foh-rack");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_payload_leaves_the_disk_without_a_table() {
        let (system, root, payload, mut image) = computer("damaged", 3 << 20);
        // One byte changed after the manifest was made.
        image[(2 << 20) + 5] ^= 1;
        std::fs::write(&payload.path, &image).unwrap();
        let job = Job {
            disk: "vdb".into(),
            payload,
            setup: None,
            password: None,
        };
        let (result, stages, _) = collect(&system, &job);
        let failure = result.expect_err("damaged");
        assert_eq!(failure.stage, Stage::Write);
        assert!(failure.error.contains("damaged"), "{failure:?}");
        assert!(failure.state.contains("erased"), "{failure:?}");
        assert_eq!(stages, [Stage::Prepare, Stage::Write]);
        // The first MiB (where the partition table goes) is zeros.
        let written = std::fs::read(root.join("dev/vdb")).unwrap();
        assert!(written[..1 << 20].iter().all(|b| *b == 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_short_payload_is_caught() {
        let (system, root, mut payload, _) = computer("short", 2 << 20);
        payload.bytes += 512;
        let job = Job {
            disk: "vdb".into(),
            payload,
            setup: None,
            password: None,
        };
        let failure = collect(&system, &job).0.expect_err("short");
        assert!(failure.error.contains("ended after"), "{failure:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_disk_that_cannot_be_used_is_not_touched() {
        let (system, root, payload, _) = computer("busy", 2 << 20);
        put(&root, "/proc/mounts", "/dev/vdb1 /mnt ext4 rw 0 0\n");
        let job = Job {
            disk: "vdb".into(),
            payload,
            setup: None,
            password: None,
        };
        let failure = collect(&system, &job).0.expect_err("in use");
        assert_eq!(failure.stage, Stage::Prepare);
        assert!(failure.error.contains("mounted at /mnt"), "{failure:?}");
        assert!(failure.state.contains("Nothing was written"), "{failure:?}");
        let written = std::fs::read(root.join("dev/vdb")).unwrap();
        assert!(written.iter().all(|b| *b == 0xEE));
        // And a disk that is not there.
        let job = Job {
            disk: "sdz".into(),
            ..job
        };
        assert!(collect(&system, &job).0.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reading_back_gives_the_checksum_of_what_is_there() {
        let (_system, root, payload, image) = computer("readback", 1 << 20);
        let path = root.join("dev/vdb");
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(&image).unwrap();
        drop(file);
        let digest = read_back(&path, false, image.len() as u64, &mut |_| {}).unwrap();
        assert_eq!(digest, payload.sha256);
        // One byte off on the disk.
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(100)).unwrap();
        file.write_all(&[image[100] ^ 0xFF]).unwrap();
        drop(file);
        let digest = read_back(&path, false, image.len() as u64, &mut |_| {}).unwrap();
        assert_ne!(digest, payload.sha256);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn old_boot_entries_are_found_by_name() {
        let text = "BootCurrent: 0001\nBootOrder: 0003,0001,0000\n\
                    Boot0000* UiApp\tFvVol(7cb8bdc9-f8eb-4f34-aaea-3ee4af6516a1)\n\
                    Boot0001* UEFI QEMU USB HARDDRIVE\tPciRoot(0x0)/Pci(0x2,0x0)\n\
                    Boot0003* LiveStage\tHD(1,GPT,0f0e,0x800,0x40000)/File(\\EFI\\BOOT\\BOOTX64.EFI)\n\
                    Boot0004  LiveStage\n\
                    Boot0005* LiveStage old\n";
        assert_eq!(old_entries(text), ["0003", "0004"]);
    }
}
