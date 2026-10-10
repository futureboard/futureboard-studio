//! The LiveStage disk image the installer writes, as the installer image
//! carries it: `livestage.img.zst` (the appliance image, compressed with
//! zstd) and next to it `payload.conf`, which the image build writes:
//!
//! ```txt
//! IMAGE_NAME="livestage-alpine3.24-x86_64"
//! IMAGE_BYTES="1250951168"          the image's size, uncompressed
//! IMAGE_SHA256="9f86d0…"            the image's sha256, uncompressed
//! ```
//!
//! An uncompressed image (any name not ending in `.zst`) is written as it
//! is, which the tests use.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::config;

pub const DEFAULT_PAYLOAD: &str = "/usr/share/livestage/installer/livestage.img.zst";
/// Next to the payload.
pub const MANIFEST: &str = "payload.conf";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub path: PathBuf,
    pub name: String,
    /// Uncompressed.
    pub bytes: u64,
    /// Of the uncompressed image, lower-case hex.
    pub sha256: String,
}

impl Payload {
    /// The payload at `path`, with what its manifest says it must be.
    pub fn load(path: &Path) -> Result<Self, String> {
        let manifest = path.with_file_name(MANIFEST);
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("{}: {e}", manifest.display()))?;
        if !path.is_file() {
            return Err(format!("{}: not found", path.display()));
        }
        parse_manifest(path, &text).map_err(|e| format!("{}: {e}", manifest.display()))
    }

    pub fn compressed(&self) -> bool {
        self.path.extension().is_some_and(|e| e == "zst")
    }

    /// The image, uncompressed, from its start.
    pub fn open(&self) -> Result<Source, String> {
        if !self.compressed() {
            let file = std::fs::File::open(&self.path)
                .map_err(|e| format!("{}: {e}", self.path.display()))?;
            return Ok(Source {
                reader: Box::new(file),
                child: None,
            });
        }
        let mut child = Command::new("zstd")
            .arg("-dc")
            .arg(&self.path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("zstd: {e}"))?;
        let stdout = child.stdout.take().ok_or("zstd: no output")?;
        Ok(Source {
            reader: Box::new(stdout),
            child: Some(child),
        })
    }
}

pub fn parse_manifest(path: &Path, text: &str) -> Result<Payload, String> {
    let mut name = String::new();
    let (mut bytes, mut sha256) = (None, None);
    for (key, value) in config::variables(text) {
        match key.as_str() {
            "IMAGE_NAME" => name = value,
            "IMAGE_BYTES" => bytes = value.trim().parse::<u64>().ok(),
            "IMAGE_SHA256" => {
                let value = value.trim().to_ascii_lowercase();
                if value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) {
                    sha256 = Some(value);
                }
            }
            _ => {}
        }
    }
    let bytes = bytes
        .filter(|b| *b > 0 && b % 512 == 0)
        .ok_or("IMAGE_BYTES is missing or not a whole number of sectors")?;
    let sha256 = sha256.ok_or("IMAGE_SHA256 is missing or not 64 hex digits")?;
    if name.is_empty() {
        name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    Ok(Payload {
        path: path.to_path_buf(),
        name,
        bytes,
        sha256,
    })
}

/// The manifest for an image, as the build writes it (for the tests).
#[cfg(test)]
pub fn manifest(name: &str, image: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "IMAGE_NAME=\"{name}\"\nIMAGE_BYTES=\"{}\"\nIMAGE_SHA256=\"{}\"\n",
        image.len(),
        hex(&Sha256::digest(image))
    )
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The image as it is read: from zstd, or the file.
pub struct Source {
    reader: Box<dyn Read + Send>,
    child: Option<Child>,
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Source {
    /// After the last byte: zstd's verdict (it checks its own checksum).
    pub fn finish(mut self) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        // Whatever is left unread (there should be nothing) is dropped.
        self.reader = Box::new(std::io::empty());
        let mut errors = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut errors);
        }
        let status = child.wait().map_err(|e| format!("zstd: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "zstd: {}",
                errors
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("failed")
            ))
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // Stopped part way (an error): zstd must not be left behind.
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn the_manifest_says_size_and_checksum() {
        let image = vec![7u8; 4096];
        let text = manifest("livestage-test", &image);
        let payload = parse_manifest(Path::new("/x/livestage.img.zst"), &text).unwrap();
        assert_eq!(payload.bytes, 4096);
        assert_eq!(payload.name, "livestage-test");
        assert_eq!(payload.sha256, hex(&Sha256::digest(&image)));
        assert!(payload.compressed());
        assert!(parse_manifest(Path::new("a.img"), "IMAGE_BYTES=100\nIMAGE_SHA256=00\n").is_err());
        assert!(
            parse_manifest(
                Path::new("a.img"),
                &format!("IMAGE_BYTES=0\nIMAGE_SHA256={}\n", "0".repeat(64))
            )
            .is_err()
        );
        let unnamed = parse_manifest(
            Path::new("/x/a.img"),
            &format!("IMAGE_BYTES=512\nIMAGE_SHA256={}\n", "A".repeat(64)),
        )
        .unwrap();
        assert_eq!(unnamed.name, "a.img");
        assert_eq!(unnamed.sha256, "a".repeat(64));
        assert!(!unnamed.compressed());
    }
}
