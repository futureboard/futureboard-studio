//! Which processor a plug-in module was built for, read from its file.
//!
//! A process can only load a module of its own word size. On Windows the
//! Studio's plug-in host and scanner are 64-bit, so a 32-bit VST2 `.dll`, a
//! 32-bit single-file `.vst3` or a bundle that only ships `Contents/x86-win`
//! goes to the 32-bit host (`FutureboardPluginHostX86.exe`) and the 32-bit
//! scanner instead. The answer comes from the PE header and the bundle's
//! architecture folders, never from loading the module.

use std::io::Read;
use std::path::Path;

/// The processor a plug-in module targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleArch {
    /// 64-bit x86 (`x86_64-win`, PE machine `0x8664`).
    X64,
    /// 32-bit x86 (`x86-win`, PE machine `0x014c`).
    X86,
    /// 64-bit Arm (`arm64-win` / `arm64x-win`, PE machine `0xaa64`).
    Arm64,
    /// Not a module this can read: a macOS or Linux bundle, a missing file, a
    /// built-in, an Audio Unit. Loaded by the default host.
    Unknown,
}

impl ModuleArch {
    /// Whether this module needs the 32-bit host on this platform. Only a
    /// 64-bit Windows build bridges: everywhere else there is no 32-bit host.
    pub fn needs_x86_bridge(self) -> bool {
        cfg!(all(windows, target_pointer_width = "64")) && self == Self::X86
    }

    /// Short label for logs and the plug-in list.
    pub fn label(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::X86 => "x86",
            Self::Arm64 => "arm64",
            Self::Unknown => "unknown",
        }
    }
}

const IMAGE_FILE_MACHINE_I386: u16 = 0x014c;
const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const IMAGE_FILE_MACHINE_ARM64: u16 = 0xaa64;

/// The architecture of the module at `path`: a PE file (`.dll`, a single-file
/// `.vst3`, `.clap`) or a VST3 bundle directory.
///
/// A bundle is judged by its `Contents/<arch>-win` folders, preferring the
/// 64-bit build when it ships both, since that is the one the 64-bit host
/// loads. A bundle with no Windows architecture folder is [`ModuleArch::Unknown`].
pub fn module_arch(path: &Path) -> ModuleArch {
    if path.is_dir() {
        return bundle_arch(path);
    }
    pe_machine(path).map_or(ModuleArch::Unknown, arch_from_machine)
}

fn bundle_arch(bundle: &Path) -> ModuleArch {
    let contents = bundle.join("Contents");
    let has = |dir: &str| contents.join(dir).is_dir();
    if has("x86_64-win") {
        ModuleArch::X64
    } else if has("arm64-win") || has("arm64x-win") || has("arm64ec-win") {
        ModuleArch::Arm64
    } else if has("x86-win") {
        ModuleArch::X86
    } else {
        ModuleArch::Unknown
    }
}

fn arch_from_machine(machine: u16) -> ModuleArch {
    match machine {
        IMAGE_FILE_MACHINE_AMD64 => ModuleArch::X64,
        IMAGE_FILE_MACHINE_I386 => ModuleArch::X86,
        IMAGE_FILE_MACHINE_ARM64 => ModuleArch::Arm64,
        _ => ModuleArch::Unknown,
    }
}

/// The `Machine` field of a PE file's COFF header, or `None` when `path` is
/// not a PE image. Reads the DOS header and the few bytes after `e_lfanew`.
fn pe_machine(path: &Path) -> Option<u16> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos).ok()?;
    pe_machine_from(&dos, |offset, buf| {
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(offset)).ok()?;
        file.read_exact(buf).ok()
    })
}

/// [`pe_machine`] over a reader: `dos` is the first 64 bytes, `read_at`
/// fills a buffer from an absolute offset.
fn pe_machine_from(
    dos: &[u8; 64],
    mut read_at: impl FnMut(u64, &mut [u8]) -> Option<()>,
) -> Option<u16> {
    if &dos[..2] != b"MZ" {
        return None;
    }
    let e_lfanew = u32::from_le_bytes(dos[0x3c..0x40].try_into().ok()?);
    // A real header sits within the first few KiB; anything far beyond is
    // not a PE image worth seeking through.
    if e_lfanew < 64 || e_lfanew > 0x10_0000 {
        return None;
    }
    let mut header = [0u8; 6];
    read_at(u64::from(e_lfanew), &mut header)?;
    if &header[..4] != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes([header[4], header[5]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PE image: DOS header pointing at a `PE\0\0` signature and
    /// the given machine.
    fn image(machine: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x80 + 6];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        bytes
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fb-module-arch-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_dll_is_judged_by_its_pe_machine() {
        let dir = temp_dir("pe");
        for (machine, arch) in [
            (IMAGE_FILE_MACHINE_I386, ModuleArch::X86),
            (IMAGE_FILE_MACHINE_AMD64, ModuleArch::X64),
            (IMAGE_FILE_MACHINE_ARM64, ModuleArch::Arm64),
            (0x01c4, ModuleArch::Unknown),
        ] {
            let path = dir.join(format!("{machine:x}.dll"));
            std::fs::write(&path, image(machine)).unwrap();
            assert_eq!(module_arch(&path), arch, "machine {machine:#x}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_is_not_a_pe_image_is_unknown() {
        let dir = temp_dir("not-pe");
        let text = dir.join("readme.dll");
        std::fs::write(&text, b"not a module at all, just some text").unwrap();
        assert_eq!(module_arch(&text), ModuleArch::Unknown);
        let mut no_signature = image(IMAGE_FILE_MACHINE_I386);
        no_signature[0x80..0x84].copy_from_slice(b"NE\0\0");
        let ne = dir.join("old.dll");
        std::fs::write(&ne, no_signature).unwrap();
        assert_eq!(module_arch(&ne), ModuleArch::Unknown);
        assert_eq!(module_arch(&dir.join("missing.dll")), ModuleArch::Unknown);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bundle_is_judged_by_its_architecture_folders() {
        let dir = temp_dir("bundle");
        let bundle = |name: &str, arches: &[&str]| {
            let path = dir.join(name);
            for arch in arches {
                std::fs::create_dir_all(path.join("Contents").join(arch)).unwrap();
            }
            std::fs::create_dir_all(path.join("Contents").join("Resources")).unwrap();
            path
        };
        assert_eq!(
            module_arch(&bundle("Old.vst3", &["x86-win"])),
            ModuleArch::X86
        );
        // Both builds: the 64-bit host takes it.
        assert_eq!(
            module_arch(&bundle("Both.vst3", &["x86-win", "x86_64-win"])),
            ModuleArch::X64
        );
        assert_eq!(
            module_arch(&bundle("Mac.vst3", &["MacOS"])),
            ModuleArch::Unknown
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_32_bit_module_on_64_bit_windows_is_bridged() {
        assert!(!ModuleArch::X64.needs_x86_bridge());
        assert!(!ModuleArch::Unknown.needs_x86_bridge());
        assert!(!ModuleArch::Arm64.needs_x86_bridge());
        assert_eq!(
            ModuleArch::X86.needs_x86_bridge(),
            cfg!(all(windows, target_pointer_width = "64"))
        );
    }
}
