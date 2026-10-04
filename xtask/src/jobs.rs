//! Memory-aware Cargo parallelism.
//!
//! Cargo defaults to one rustc job per logical CPU. On a many-core machine with
//! modest RAM, the GPUI/CEF crates and the LTO link step can exhaust memory and
//! swap or OOM-kill the build. Every Cargo invocation the xtask spawns gets a
//! `CARGO_BUILD_JOBS` derived from currently available memory instead, capped
//! at the CPU count. Cargo forwards the job count to build scripts as
//! `NUM_JOBS`, so CMake-driven sidecars (CEF wrapper, Crashpad) follow it too.
//!
//! Explicit choices win: an existing `CARGO_BUILD_JOBS`, or `-j`/`--jobs` in
//! forwarded arguments, leaves the command untouched. The per-job budget
//! defaults to [`DEFAULT_MEM_PER_JOB_MIB`] and can be tuned with
//! `XTASK_MEM_PER_JOB_MB`.
//!
//! The job count cannot bound the release profile's final link: `lto = true`
//! with `codegen-units = 1` merges the whole application into one LLVM module
//! inside a single rustc process, which the kernel OOM killer has terminated
//! on 16 GB machines. When available memory is below [`FAT_LTO_MIN_MIB`], a
//! release build falls back to thin LTO via `CARGO_PROFILE_RELEASE_LTO=thin`
//! and says so. `XTASK_FAT_LTO=1` or an explicit `CARGO_PROFILE_RELEASE_LTO`
//! keeps the configured setting.

use std::process::Command;
use std::sync::OnceLock;

/// Budget per parallel rustc job. Heavy crates in this workspace peak around
/// 1.5–2 GiB each, so 2 GiB keeps headroom for the linker.
const DEFAULT_MEM_PER_JOB_MIB: u64 = 2048;
const MEM_PER_JOB_ENV: &str = "XTASK_MEM_PER_JOB_MB";

/// Available memory below which a fat-LTO release link is not attempted.
const FAT_LTO_MIN_MIB: u64 = 20 * 1024;
const FORCE_FAT_LTO_ENV: &str = "XTASK_FAT_LTO";
const RELEASE_LTO_ENV: &str = "CARGO_PROFILE_RELEASE_LTO";

/// Set `CARGO_BUILD_JOBS` on `command` unless the caller already chose a job
/// count, and fall back to thin LTO for a low-memory release build.
/// `profile` is the Cargo profile when known; otherwise it is read from
/// `forwarded`, the user-supplied Cargo arguments.
pub fn apply(command: &mut Command, profile: Option<&str>, forwarded: &[String]) {
    if std::env::var_os("CARGO_BUILD_JOBS").is_none() && !has_jobs_flag(forwarded) {
        if let Some(jobs) = memory_limited_jobs() {
            command.env("CARGO_BUILD_JOBS", jobs.to_string());
        }
    }

    let profile = profile.or_else(|| requested_profile(forwarded));
    if profile == Some("release") && use_thin_lto_fallback() {
        command.env(RELEASE_LTO_ENV, "thin");
    }
}

fn requested_profile(args: &[String]) -> Option<&str> {
    args.iter()
        .enumerate()
        .find_map(|(index, argument)| match argument.as_str() {
            "--release" | "-r" => Some("release"),
            "--profile" => args.get(index + 1).map(String::as_str),
            _ => argument.strip_prefix("--profile="),
        })
}

/// Decided once per run, like the job count, so every release build in a
/// chained xtask run links the same way.
fn use_thin_lto_fallback() -> bool {
    static THIN: OnceLock<bool> = OnceLock::new();
    *THIN.get_or_init(|| {
        if std::env::var_os(RELEASE_LTO_ENV).is_some()
            || std::env::var(FORCE_FAT_LTO_ENV).is_ok_and(|value| value == "1")
        {
            return false;
        }
        match available_memory_mib() {
            Some(available_mib) if available_mib < FAT_LTO_MIN_MIB => {
                eprintln!(
                    "[xtask] warning: {available_mib} MiB RAM available is below the \
                     {FAT_LTO_MIN_MIB} MiB needed for the release profile's fat LTO link; \
                     building with {RELEASE_LTO_ENV}=thin. This binary is not identical to a \
                     fat-LTO release; set {FORCE_FAT_LTO_ENV}=1 to keep fat LTO."
                );
                true
            }
            _ => false,
        }
    })
}

fn has_jobs_flag(args: &[String]) -> bool {
    args.iter().any(|argument| {
        argument == "-j"
            || argument == "--jobs"
            || argument.starts_with("--jobs=")
            || (argument.starts_with("-j") && argument[2..].chars().all(|c| c.is_ascii_digit()))
    })
}

/// Computed once per xtask run so chained builds use a consistent value and
/// the decision is printed once.
fn memory_limited_jobs() -> Option<u64> {
    static JOBS: OnceLock<Option<u64>> = OnceLock::new();
    *JOBS.get_or_init(|| {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1);
        let Some(available_mib) = available_memory_mib() else {
            eprintln!("[xtask] could not read available memory; leaving Cargo jobs at default");
            return None;
        };
        let per_job_mib = mem_per_job_mib();
        let jobs = jobs_for(available_mib, per_job_mib, cpus);
        eprintln!(
            "[xtask] {available_mib} MiB RAM available, {per_job_mib} MiB per job → \
             CARGO_BUILD_JOBS={jobs} (of {cpus} CPUs; override with CARGO_BUILD_JOBS or {MEM_PER_JOB_ENV})"
        );
        Some(jobs)
    })
}

fn mem_per_job_mib() -> u64 {
    match std::env::var(MEM_PER_JOB_ENV) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(mib) if mib > 0 => mib,
            _ => {
                eprintln!(
                    "[xtask] ignoring invalid {MEM_PER_JOB_ENV}={value:?}; using {DEFAULT_MEM_PER_JOB_MIB}"
                );
                DEFAULT_MEM_PER_JOB_MIB
            }
        },
        Err(_) => DEFAULT_MEM_PER_JOB_MIB,
    }
}

fn jobs_for(available_mib: u64, per_job_mib: u64, cpus: u64) -> u64 {
    (available_mib / per_job_mib.max(1)).clamp(1, cpus.max(1))
}

#[cfg(target_os = "linux")]
fn available_memory_mib() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib = meminfo.lines().find_map(|line| {
        line.strip_prefix("MemAvailable:")?
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse::<u64>()
            .ok()
    })?;
    Some(kib / 1024)
}

#[cfg(target_os = "macos")]
fn available_memory_mib() -> Option<u64> {
    // macOS aggressively fills RAM with reclaimable cache, so "free" pages
    // understate what a build can use. Free + inactive + speculative +
    // purgeable approximates Activity Monitor's available memory.
    let output = Command::new("vm_stat").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let page_size = text
        .lines()
        .next()?
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    let pages = |label: &str| -> u64 {
        text.lines()
            .find_map(|line| {
                line.strip_prefix(label)?
                    .trim()
                    .trim_end_matches('.')
                    .parse::<u64>()
                    .ok()
            })
            .unwrap_or(0)
    };
    let available = pages("Pages free:")
        + pages("Pages inactive:")
        + pages("Pages speculative:")
        + pages("Pages purgeable:");
    Some(available * page_size / (1024 * 1024))
}

#[cfg(windows)]
fn available_memory_mib() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: `status` is a properly sized, initialised MEMORYSTATUSEX with
    // `dwLength` set, as GlobalMemoryStatusEx requires.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0).then(|| status.ullAvailPhys / (1024 * 1024))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn available_memory_mib() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::{has_jobs_flag, jobs_for, requested_profile};

    #[test]
    fn jobs_scale_with_memory_and_cap_at_cpus() {
        assert_eq!(jobs_for(12_000, 2048, 16), 5);
        assert_eq!(jobs_for(64_000, 2048, 16), 16);
        assert_eq!(jobs_for(1_000, 2048, 16), 1);
    }

    #[test]
    fn explicit_jobs_flags_are_detected() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(has_jobs_flag(&args(&["-j", "4"])));
        assert!(has_jobs_flag(&args(&["-j4"])));
        assert!(has_jobs_flag(&args(&["--jobs=2"])));
        assert!(!has_jobs_flag(&args(&["--release", "--target", "x"])));
        assert!(!has_jobs_flag(&args(&["-jx"])));
    }

    #[test]
    fn forwarded_profile_is_detected() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(requested_profile(&args(&["--release"])), Some("release"));
        assert_eq!(requested_profile(&args(&["--profile", "dev"])), Some("dev"));
        assert_eq!(
            requested_profile(&args(&["--profile=release"])),
            Some("release")
        );
        assert_eq!(requested_profile(&args(&["--target", "x"])), None);
    }
}
