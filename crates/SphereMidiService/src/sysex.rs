//! System Exclusive messages: framing, hex text, manufacturer identification,
//! and the vendor structure of Roland, Yamaha and Korg messages.
//!
//! Control-path only. Nothing here runs on the audio thread: the editor and
//! the importers decode and validate here, and playback carries the finished
//! bytes.
//!
//! Every function takes a **complete** message, `F0 … F7`. Standard MIDI Files
//! store the bytes after `F0` (length-prefixed), so callers converting from
//! that form use [`from_smf_payload`] / [`to_smf_payload`].

use std::fmt::Write as _;

/// Start of System Exclusive.
pub const SOX: u8 = 0xF0;
/// End of Exclusive.
pub const EOX: u8 = 0xF7;

/// Manufacturer ID bytes that matter to the decoders.
pub mod id {
    pub const ROLAND: u8 = 0x41;
    pub const KORG: u8 = 0x42;
    pub const YAMAHA: u8 = 0x43;
    /// Universal Non-Real Time (GM on/off, identity, sample dump…).
    pub const UNIVERSAL_NON_REALTIME: u8 = 0x7E;
    /// Universal Real Time (master volume, MTC, MMC…).
    pub const UNIVERSAL_REALTIME: u8 = 0x7F;
    /// Reserved for non-commercial / educational use.
    pub const NON_COMMERCIAL: u8 = 0x7D;
    /// "All devices" in the device-ID position of a universal message.
    pub const ALL_DEVICES: u8 = 0x7F;
}

/// Who a message is addressed to, from its manufacturer ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Manufacturer {
    Roland,
    Korg,
    Yamaha,
    UniversalNonRealtime,
    UniversalRealtime,
    NonCommercial,
    /// Any other one-byte ID.
    Other(u8),
    /// A three-byte ID (`00 hh ll`).
    Extended(u8, u8),
}

impl Manufacturer {
    /// The ID at the start of `body` (the bytes after `F0`) and how many bytes
    /// it occupies.
    pub fn parse(body: &[u8]) -> Option<(Self, usize)> {
        let first = *body.first()?;
        if first & 0x80 != 0 {
            return None;
        }
        let manufacturer = match first {
            0x00 => {
                let hi = *body.get(1)?;
                let lo = *body.get(2)?;
                return Some((Self::Extended(hi, lo), 3));
            }
            id::ROLAND => Self::Roland,
            id::KORG => Self::Korg,
            id::YAMAHA => Self::Yamaha,
            id::UNIVERSAL_NON_REALTIME => Self::UniversalNonRealtime,
            id::UNIVERSAL_REALTIME => Self::UniversalRealtime,
            id::NON_COMMERCIAL => Self::NonCommercial,
            other => Self::Other(other),
        };
        Some((manufacturer, 1))
    }

    pub fn label(self) -> String {
        match self {
            Self::Roland => "Roland".into(),
            Self::Korg => "Korg".into(),
            Self::Yamaha => "Yamaha".into(),
            Self::UniversalNonRealtime => "Universal (Non-RT)".into(),
            Self::UniversalRealtime => "Universal (RT)".into(),
            Self::NonCommercial => "Non-commercial".into(),
            Self::Other(id) => format!("ID {id:02X}"),
            Self::Extended(hi, lo) => format!("ID 00 {hi:02X} {lo:02X}"),
        }
    }
}

// ── Framing ───────────────────────────────────────────────────────────────

/// Why a byte string is not a usable SysEx message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Empty,
    MissingStart,
    MissingEnd,
    /// A status byte (≥ 0x80) inside the body, at this index of the message.
    StatusInBody(usize),
}

impl FrameError {
    pub fn message(&self) -> String {
        match self {
            Self::Empty => "Empty message".into(),
            Self::MissingStart => "Must start with F0".into(),
            Self::MissingEnd => "Must end with F7".into(),
            Self::StatusInBody(i) => format!("Byte {} is ≥ 80h; data bytes are 00–7F", i + 1),
        }
    }
}

/// Check `F0 <7-bit data…> F7`.
pub fn validate_frame(message: &[u8]) -> Result<(), FrameError> {
    let (&first, rest) = message.split_first().ok_or(FrameError::Empty)?;
    if first != SOX {
        return Err(FrameError::MissingStart);
    }
    let (&last, body) = rest.split_last().ok_or(FrameError::MissingEnd)?;
    if last != EOX {
        return Err(FrameError::MissingEnd);
    }
    if let Some(i) = body.iter().position(|b| b & 0x80 != 0) {
        return Err(FrameError::StatusInBody(i + 1));
    }
    Ok(())
}

/// SMF `F0` event payload (the bytes after `F0`) → complete message.
pub fn from_smf_payload(payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(payload.len() + 1);
    message.push(SOX);
    message.extend_from_slice(payload);
    message
}

/// Complete message → SMF `F0` event payload. `None` unless it starts `F0`.
pub fn to_smf_payload(message: &[u8]) -> Option<Vec<u8>> {
    (message.first() == Some(&SOX)).then(|| message[1..].to_vec())
}

// ── Hex text ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HexError {
    /// A token that is not one or two hex digits, 1-based token index.
    BadToken { index: usize, token: String },
}

impl HexError {
    pub fn message(&self) -> String {
        match self {
            Self::BadToken { index, token } => format!("\"{token}\" (byte {index}) is not hex"),
        }
    }
}

/// Parse hex bytes. Accepts `F0 41 10`, `F0,41,10`, `0xF0 0x41`, `F04110`
/// and `41h` — whatever a synth manual or another DAW pasted.
pub fn parse_hex(text: &str) -> Result<Vec<u8>, HexError> {
    let mut bytes = Vec::new();
    let tokens = text
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-'))
        .filter(|t| !t.is_empty());
    for raw in tokens {
        let mut token = raw;
        if let Some(t) = token
            .strip_prefix("0x")
            .or_else(|| token.strip_prefix("0X"))
            .or_else(|| token.strip_prefix('$'))
        {
            token = t;
        }
        if let Some(t) = token.strip_suffix('h').or_else(|| token.strip_suffix('H')) {
            token = t;
        }
        let index = bytes.len() + 1;
        let bad = || HexError::BadToken {
            index,
            token: raw.to_string(),
        };
        if token.is_empty() || !token.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad());
        }
        // A run of digits with no separators is a packed byte string.
        if token.len() % 2 == 1 && token.len() > 2 {
            return Err(bad());
        }
        for chunk in token.as_bytes().chunks(2) {
            let s = std::str::from_utf8(chunk).map_err(|_| bad())?;
            bytes.push(u8::from_str_radix(s, 16).map_err(|_| bad())?);
        }
    }
    Ok(bytes)
}

/// `F0 41 10 …` — upper-case, space separated.
pub fn format_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{b:02X}");
    }
    out
}

// ── Checksums ─────────────────────────────────────────────────────────────

/// Roland / Yamaha bulk checksum: the 7-bit value that makes the covered
/// bytes plus the checksum sum to 0 mod 128.
pub fn checksum_7bit(covered: &[u8]) -> u8 {
    let sum: u32 = covered.iter().map(|b| u32::from(*b & 0x7F)).sum();
    ((128 - (sum % 128)) % 128) as u8
}

/// Whether a message's checksum is right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checksum {
    /// The format has no checksum (or this decoder does not know where it is).
    None,
    Valid,
    Invalid {
        found: u8,
        expected: u8,
    },
}

/// Where a checksum lives in a message, as indices into the full message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChecksumSpan {
    /// First covered byte.
    from: usize,
    /// Index of the checksum byte; covered bytes are `from..at`.
    at: usize,
}

fn checksum_span(message: &[u8]) -> Option<ChecksumSpan> {
    validate_frame(message).ok()?;
    let body = &message[1..message.len() - 1];
    let (manufacturer, id_len) = Manufacturer::parse(body)?;
    let at = message.len() - 2;
    match manufacturer {
        Manufacturer::Roland => {
            let roland = RolandHeader::parse(&body[id_len..])?;
            let from = 1 + id_len + roland.header_len;
            // A DT1/RQ1 needs at least one address byte before the checksum.
            (roland.command.is_some() && at > from).then_some(ChecksumSpan { from, at })
        }
        Manufacturer::Yamaha => {
            let sub = *body.get(1)?;
            let model = *body.get(2)?;
            // XG bulk dump: F0 43 0n 4C bh bl ah am al data… cs F7. The sum
            // covers byte count, address and data.
            (sub & 0xF0 == 0x00 && model == YAMAHA_XG && at > 4)
                .then_some(ChecksumSpan { from: 4, at })
        }
        _ => None,
    }
}

/// Validate the checksum a Roland DT1/RQ1 or Yamaha XG bulk dump carries.
pub fn checksum(message: &[u8]) -> Checksum {
    let Some(span) = checksum_span(message) else {
        return Checksum::None;
    };
    let expected = checksum_7bit(&message[span.from..span.at]);
    let found = message[span.at];
    if found == expected {
        Checksum::Valid
    } else {
        Checksum::Invalid { found, expected }
    }
}

/// Recompute the checksum in place. Returns `true` when a byte changed.
pub fn fix_checksum(message: &mut [u8]) -> bool {
    let Some(span) = checksum_span(message) else {
        return false;
    };
    let expected = checksum_7bit(&message[span.from..span.at]);
    let changed = message[span.at] != expected;
    message[span.at] = expected;
    changed
}

// ── Roland ────────────────────────────────────────────────────────────────

/// Roland command IDs.
pub const ROLAND_RQ1: u8 = 0x11;
pub const ROLAND_DT1: u8 = 0x12;
/// GS model ID (SC-55/88 and every GS-compatible module).
pub const ROLAND_GS: u8 = 0x42;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RolandHeader {
    device: u8,
    model: Vec<u8>,
    command: Option<u8>,
    /// Bytes from the device ID through the command, inclusive.
    header_len: usize,
}

impl RolandHeader {
    /// `body` starts at the device ID (after `F0 41`) and excludes `F7`.
    fn parse(body: &[u8]) -> Option<Self> {
        let device = *body.first()?;
        // Extended model IDs lead with 00 bytes (Integra-7 is 00 00 64).
        let mut i = 1;
        while body.get(i) == Some(&0x00) && i < 4 {
            i += 1;
        }
        let model_end = i + 1;
        let model = body.get(1..model_end)?.to_vec();
        let command = body
            .get(model_end)
            .copied()
            .filter(|c| matches!(*c, ROLAND_RQ1 | ROLAND_DT1));
        Some(Self {
            device,
            model,
            command,
            header_len: model_end + 1,
        })
    }
}

fn roland_model_name(model: &[u8]) -> Option<&'static str> {
    Some(match model {
        [0x42] => "GS",
        [0x16] => "MT-32/LA",
        [0x45] => "SC display",
        [0x6A] => "JV/XP",
        [0x00, 0x00, 0x64] => "INTEGRA-7",
        _ => return None,
    })
}

/// Address width for models whose map is known; everything else shows its
/// payload undivided rather than a guessed split.
fn roland_address_len(model: &[u8]) -> Option<usize> {
    match model {
        [0x42] | [0x16] | [0x45] => Some(3),
        [0x6A] | [0x00, 0x00, 0x64] => Some(4),
        _ => None,
    }
}

fn gs_address_name(address: &[u8]) -> Option<&'static str> {
    Some(match address {
        [0x40, 0x00, 0x7F] => "GS Reset",
        [0x00, 0x00, 0x7F] => "System Mode Set",
        [0x40, 0x00, 0x00] => "Master Tune",
        [0x40, 0x00, 0x04] => "Master Volume",
        [0x40, 0x00, 0x05] => "Master Key Shift",
        [0x40, 0x00, 0x06] => "Master Pan",
        [0x40, 0x01, 0x30] => "Reverb Macro",
        [0x40, 0x01, 0x38] => "Chorus Macro",
        [0x40, p, 0x15] if (0x10..=0x1F).contains(p) => "Use for Rhythm Part",
        [0x40, p, 0x02] if (0x10..=0x1F).contains(p) => "Rx Channel",
        _ => return None,
    })
}

/// GS part block (`40 1x …`) → MIDI channel 1–16. GS numbers part 10 first
/// (`40 10`), then 1–9 (`40 11`…`40 19`), then 11–16 (`40 1A`…`40 1F`).
pub fn gs_part_channel(block: u8) -> Option<u8> {
    match block {
        0x10 => Some(10),
        0x11..=0x19 => Some(block - 0x10),
        0x1A..=0x1F => Some(block - 0x10 + 1),
        _ => None,
    }
}

// ── Yamaha ────────────────────────────────────────────────────────────────

/// XG model ID.
pub const YAMAHA_XG: u8 = 0x4C;

fn xg_address_name(address: &[u8]) -> Option<&'static str> {
    Some(match address {
        [0x00, 0x00, 0x7E] => "XG System On",
        [0x00, 0x00, 0x7F] => "All Parameter Reset",
        [0x00, 0x00, 0x00] => "Master Tune",
        [0x00, 0x00, 0x04] => "Master Volume",
        [0x00, 0x00, 0x05] => "Master Attenuator",
        [0x00, 0x00, 0x06] => "Transpose",
        [0x02, 0x01, 0x00] => "Reverb Type",
        [0x02, 0x01, 0x20] => "Chorus Type",
        [0x02, 0x01, 0x40] => "Variation Type",
        [0x08, _, 0x07] => "Part Mode",
        [0x08, _, 0x04] => "Receive Channel",
        _ => return None,
    })
}

// ── Korg ──────────────────────────────────────────────────────────────────

fn korg_model_name(model: &[u8]) -> Option<&'static str> {
    Some(match model {
        [0x19] => "M1",
        [0x50] => "TRITON",
        [0x58] => "microKORG",
        [0x00, 0x01, 0x2C] => "minilogue",
        [0x00, 0x01, 0x51] => "minilogue xd",
        _ => return None,
    })
}

fn korg_function_name(function: u8) -> Option<&'static str> {
    Some(match function {
        0x10 => "Current Program Dump Request",
        0x1C => "Program Dump Request",
        0x0E => "Global Dump Request",
        0x40 => "Current Program Dump",
        0x41 => "Parameter Change",
        0x4C => "Program Dump",
        0x51 => "Global Dump",
        0x21 => "Write Completed",
        0x22 => "Write Error",
        0x23 => "Data Load Completed",
        0x24 => "Data Load Error",
        0x26 => "Data Format Error",
        _ => return None,
    })
}

// ── Decoding ──────────────────────────────────────────────────────────────

/// What the editor shows for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub manufacturer: Option<Manufacturer>,
    /// Device ID / channel as the vendor numbers it, when the format has one.
    pub device: Option<u8>,
    /// Model, e.g. "GS", "XG", "microKORG" — or hex when unknown.
    pub model: Option<String>,
    /// One line: what the message does.
    pub summary: String,
    pub checksum: Checksum,
    pub frame: Result<(), FrameError>,
}

/// Describe a complete message. Never fails: a malformed message still gets
/// a description saying what is wrong with it.
pub fn decode(message: &[u8]) -> Decoded {
    let frame = validate_frame(message);
    let mut decoded = Decoded {
        manufacturer: None,
        device: None,
        model: None,
        summary: String::new(),
        checksum: Checksum::None,
        frame: frame.clone(),
    };
    if let Err(error) = frame {
        decoded.summary = error.message();
        return decoded;
    }
    let body = &message[1..message.len() - 1];
    let Some((manufacturer, id_len)) = Manufacturer::parse(body) else {
        decoded.summary = "No manufacturer ID".into();
        return decoded;
    };
    decoded.manufacturer = Some(manufacturer);
    decoded.checksum = checksum(message);
    let rest = &body[id_len..];
    match manufacturer {
        Manufacturer::Roland => decode_roland(rest, &mut decoded),
        Manufacturer::Yamaha => decode_yamaha(rest, &mut decoded),
        Manufacturer::Korg => decode_korg(rest, &mut decoded),
        Manufacturer::UniversalNonRealtime => decode_universal(rest, false, &mut decoded),
        Manufacturer::UniversalRealtime => decode_universal(rest, true, &mut decoded),
        _ => decoded.summary = format!("{} bytes", message.len()),
    }
    decoded
}

fn decode_roland(rest: &[u8], out: &mut Decoded) {
    let Some(header) = RolandHeader::parse(rest) else {
        out.summary = "Roland (truncated)".into();
        return;
    };
    out.device = Some(header.device);
    out.model = Some(
        roland_model_name(&header.model)
            .map(str::to_string)
            .unwrap_or_else(|| format_hex(&header.model)),
    );
    let Some(command) = header.command else {
        out.summary = "Roland message".into();
        return;
    };
    // Payload between the command and the checksum.
    let payload = rest
        .get(header.header_len..rest.len().saturating_sub(1))
        .unwrap_or(&[]);
    let verb = if command == ROLAND_DT1 { "DT1" } else { "RQ1" };
    let Some(addr_len) = roland_address_len(&header.model).filter(|n| payload.len() >= *n) else {
        out.summary = format!("{verb} · {}", format_hex(payload));
        return;
    };
    let (address, data) = payload.split_at(addr_len);
    let name = (header.model == [ROLAND_GS])
        .then(|| gs_address_name(address))
        .flatten();
    let mut summary = match name {
        Some(name) => format!("{verb} {name}"),
        None => format!("{verb} @ {}", format_hex(address)),
    };
    if header.model == [ROLAND_GS] && address.len() == 3 && address[0] == 0x40 {
        if let Some(ch) = gs_part_channel(address[1]) {
            let _ = write!(summary, " (Ch {ch})");
        }
    }
    if !data.is_empty() {
        let _ = write!(
            summary,
            " {} {}",
            if command == ROLAND_DT1 { "=" } else { "size" },
            format_hex(data)
        );
    }
    out.summary = summary;
}

fn decode_yamaha(rest: &[u8], out: &mut Decoded) {
    let Some(&sub) = rest.first() else {
        out.summary = "Yamaha (truncated)".into();
        return;
    };
    out.device = Some(sub & 0x0F);
    let model = rest.get(1).copied();
    out.model = model.map(|m| {
        if m == YAMAHA_XG {
            "XG".to_string()
        } else {
            format!("{m:02X}")
        }
    });
    let kind = match sub & 0xF0 {
        0x00 => "Bulk Dump",
        0x10 => "Parameter Change",
        0x20 => "Dump Request",
        0x30 => "Parameter Request",
        _ => "Message",
    };
    if model == Some(YAMAHA_XG) && sub & 0xF0 == 0x10 && rest.len() >= 5 {
        let address = &rest[2..5];
        let data = &rest[5..];
        out.summary = match xg_address_name(address) {
            Some(name) => name.to_string(),
            None => format!("{kind} @ {}", format_hex(address)),
        };
        if address[0] == 0x08 {
            let _ = write!(out.summary, " (Part {})", address[1] + 1);
        }
        if !data.is_empty() && address != [0x00, 0x00, 0x7E] {
            let _ = write!(out.summary, " = {}", format_hex(data));
        }
        return;
    }
    if sub & 0xF0 == 0x00 && model == Some(YAMAHA_XG) && rest.len() >= 7 {
        let count = (usize::from(rest[2]) << 7) | usize::from(rest[3]);
        out.summary = format!("{kind} @ {} · {count} bytes", format_hex(&rest[4..7]));
        return;
    }
    out.summary = kind.to_string();
}

fn decode_korg(rest: &[u8], out: &mut Decoded) {
    let Some(&format) = rest.first() else {
        out.summary = "Korg (truncated)".into();
        return;
    };
    // 3n: MIDI channel n. 5x is the Search Device handshake.
    if format == 0x50 {
        out.summary = match rest.get(1) {
            Some(0x00) => "Search Device Request".into(),
            Some(0x01) => "Search Device Reply".into(),
            _ => "Search Device".into(),
        };
        return;
    }
    if format & 0xF0 != 0x30 {
        out.summary = "Korg message".into();
        return;
    }
    out.device = Some(format & 0x0F);
    let model_len = if rest.get(1) == Some(&0x00) { 3 } else { 1 };
    let Some(model) = rest.get(1..1 + model_len) else {
        out.summary = "Korg (truncated)".into();
        return;
    };
    out.model = Some(
        korg_model_name(model)
            .map(str::to_string)
            .unwrap_or_else(|| format_hex(model)),
    );
    let function = rest.get(1 + model_len).copied();
    out.summary = match function {
        Some(f) => match korg_function_name(f) {
            Some(name) => name.to_string(),
            None => format!("Function {f:02X}"),
        },
        None => "Korg message".into(),
    };
    let data_len = rest.len().saturating_sub(2 + model_len);
    if data_len > 0 {
        let _ = write!(out.summary, " · {data_len} bytes");
    }
}

fn decode_universal(rest: &[u8], realtime: bool, out: &mut Decoded) {
    let device = rest.first().copied();
    out.device = device;
    let sub1 = rest.get(1).copied();
    let sub2 = rest.get(2).copied();
    let data = rest.get(3..).unwrap_or(&[]);
    out.summary = match (realtime, sub1, sub2) {
        (false, Some(0x09), Some(0x01)) => "GM System On".into(),
        (false, Some(0x09), Some(0x02)) => "GM System Off".into(),
        (false, Some(0x09), Some(0x03)) => "GM2 System On".into(),
        (false, Some(0x06), Some(0x01)) => "Identity Request".into(),
        (false, Some(0x06), Some(0x02)) => match Manufacturer::parse(data) {
            Some((m, _)) => format!("Identity Reply · {}", m.label()),
            None => "Identity Reply".into(),
        },
        (false, Some(0x7B), _) => "End of File".into(),
        (false, Some(0x7C), _) => "Wait".into(),
        (false, Some(0x7D), _) => "Cancel".into(),
        (false, Some(0x7E), _) => "NAK".into(),
        (false, Some(0x7F), _) => "ACK".into(),
        (true, Some(0x04), Some(0x01)) => {
            let value = match data {
                [lsb, msb, ..] => (u16::from(*msb) << 7) | u16::from(*lsb),
                _ => 0,
            };
            format!("Master Volume = {:.0}%", f32::from(value) / 16383.0 * 100.0)
        }
        (true, Some(0x04), Some(0x02)) => "Master Balance".into(),
        (true, Some(0x04), Some(0x03)) => "Master Fine Tuning".into(),
        (true, Some(0x04), Some(0x04)) => "Master Coarse Tuning".into(),
        (true, Some(0x01), Some(0x01)) => "MTC Full Frame".into(),
        (true, Some(0x06), _) => "MMC Command".into(),
        (_, Some(a), Some(b)) => format!("Sub-ID {a:02X} {b:02X}"),
        _ => "Universal message".into(),
    };
}

// ── Templates ─────────────────────────────────────────────────────────────

/// Vendor grouping for the template picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Vendor {
    Universal,
    Roland,
    Yamaha,
    Korg,
}

impl Vendor {
    pub const ALL: [Vendor; 4] = [
        Vendor::Universal,
        Vendor::Roland,
        Vendor::Yamaha,
        Vendor::Korg,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Vendor::Universal => "Universal",
            Vendor::Roland => "Roland",
            Vendor::Yamaha => "Yamaha",
            Vendor::Korg => "Korg",
        }
    }
}

/// A ready-made message. Checksummed templates are stored with their correct
/// checksum; `tests::templates_are_valid` holds them to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Template {
    pub id: &'static str,
    pub vendor: Vendor,
    pub label: &'static str,
    pub bytes: &'static [u8],
}

pub const TEMPLATES: &[Template] = &[
    Template {
        id: "gm-on",
        vendor: Vendor::Universal,
        label: "GM System On",
        bytes: &[0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7],
    },
    Template {
        id: "gm2-on",
        vendor: Vendor::Universal,
        label: "GM2 System On",
        bytes: &[0xF0, 0x7E, 0x7F, 0x09, 0x03, 0xF7],
    },
    Template {
        id: "gm-off",
        vendor: Vendor::Universal,
        label: "GM System Off",
        bytes: &[0xF0, 0x7E, 0x7F, 0x09, 0x02, 0xF7],
    },
    Template {
        id: "master-volume",
        vendor: Vendor::Universal,
        label: "Master Volume (max)",
        bytes: &[0xF0, 0x7F, 0x7F, 0x04, 0x01, 0x7F, 0x7F, 0xF7],
    },
    Template {
        id: "identity-request",
        vendor: Vendor::Universal,
        label: "Identity Request",
        bytes: &[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7],
    },
    Template {
        id: "gs-reset",
        vendor: Vendor::Roland,
        label: "GS Reset",
        bytes: &[
            0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x41, 0xF7,
        ],
    },
    Template {
        id: "gs-mode-1",
        vendor: Vendor::Roland,
        label: "GS System Mode Set 1",
        bytes: &[
            0xF0, 0x41, 0x10, 0x42, 0x12, 0x00, 0x00, 0x7F, 0x00, 0x01, 0xF7,
        ],
    },
    Template {
        id: "gs-master-volume",
        vendor: Vendor::Roland,
        label: "GS Master Volume",
        bytes: &[
            0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x04, 0x7F, 0x3D, 0xF7,
        ],
    },
    Template {
        id: "gs-drum-ch11",
        vendor: Vendor::Roland,
        label: "GS Ch 11 as Drum Part",
        bytes: &[
            0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x1A, 0x15, 0x02, 0x0F, 0xF7,
        ],
    },
    Template {
        id: "roland-dt1",
        vendor: Vendor::Roland,
        label: "DT1 Data Set (GS)",
        bytes: &[
            0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x00, 0x00, 0x40, 0xF7,
        ],
    },
    Template {
        id: "xg-on",
        vendor: Vendor::Yamaha,
        label: "XG System On",
        bytes: &[0xF0, 0x43, 0x10, 0x4C, 0x00, 0x00, 0x7E, 0x00, 0xF7],
    },
    Template {
        id: "xg-reset",
        vendor: Vendor::Yamaha,
        label: "XG All Parameter Reset",
        bytes: &[0xF0, 0x43, 0x10, 0x4C, 0x00, 0x00, 0x7F, 0x00, 0xF7],
    },
    Template {
        id: "xg-master-volume",
        vendor: Vendor::Yamaha,
        label: "XG Master Volume",
        bytes: &[0xF0, 0x43, 0x10, 0x4C, 0x00, 0x00, 0x04, 0x7F, 0xF7],
    },
    Template {
        id: "xg-param",
        vendor: Vendor::Yamaha,
        label: "XG Parameter Change",
        bytes: &[0xF0, 0x43, 0x10, 0x4C, 0x00, 0x00, 0x00, 0x00, 0xF7],
    },
    Template {
        id: "korg-search",
        vendor: Vendor::Korg,
        label: "Search Device Request",
        bytes: &[0xF0, 0x42, 0x50, 0x00, 0x00, 0xF7],
    },
    Template {
        id: "korg-program-request",
        vendor: Vendor::Korg,
        label: "Current Program Dump Request (microKORG)",
        bytes: &[0xF0, 0x42, 0x30, 0x58, 0x10, 0xF7],
    },
];

pub fn template(id: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.id == id)
}

// ── Device ID ─────────────────────────────────────────────────────────────

/// Rewrite the device ID / channel a Roland, Yamaha, Korg or Universal
/// message is addressed to, keeping the checksum right. `device` is the raw
/// value the vendor puts on the wire (Roland 00–1F with 10h the default,
/// Yamaha/Korg 0–15 in the low nibble, Universal 00–7F). Returns `false`
/// when the message has no device field.
pub fn set_device(message: &mut [u8], device: u8) -> bool {
    if validate_frame(message).is_err() || message.len() < 3 {
        return false;
    }
    let ok = match message[1] {
        id::ROLAND | id::UNIVERSAL_NON_REALTIME | id::UNIVERSAL_REALTIME => {
            message[2] = device & 0x7F;
            true
        }
        id::YAMAHA | id::KORG if message[2] != 0x50 => {
            message[2] = (message[2] & 0xF0) | (device & 0x0F);
            true
        }
        _ => false,
    };
    if ok {
        fix_checksum(message);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_valid() {
        for t in TEMPLATES {
            assert_eq!(validate_frame(t.bytes), Ok(()), "{}", t.id);
            assert!(
                !matches!(checksum(t.bytes), Checksum::Invalid { .. }),
                "{} checksum {:?}",
                t.id,
                checksum(t.bytes)
            );
        }
        assert_eq!(
            checksum(template("gs-reset").unwrap().bytes),
            Checksum::Valid
        );
    }

    #[test]
    fn roland_checksum_matches_the_gs_reset_every_manual_prints() {
        // F0 41 10 42 12 40 00 7F 00 41 F7
        assert_eq!(checksum_7bit(&[0x40, 0x00, 0x7F, 0x00]), 0x41);
        let mut message = vec![
            0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x00, 0xF7,
        ];
        assert_eq!(
            checksum(&message),
            Checksum::Invalid {
                found: 0x00,
                expected: 0x41
            }
        );
        assert!(fix_checksum(&mut message));
        assert_eq!(checksum(&message), Checksum::Valid);
    }

    #[test]
    fn integra7_extended_model_checksums_over_address_and_data() {
        // DT1 to INTEGRA-7 (model 00 00 64), 4-byte address.
        let mut message = vec![
            0xF0, 0x41, 0x10, 0x00, 0x00, 0x64, 0x12, 0x19, 0x00, 0x00, 0x00, 0x05, 0x00, 0xF7,
        ];
        fix_checksum(&mut message);
        assert_eq!(message[12], checksum_7bit(&[0x19, 0x00, 0x00, 0x00, 0x05]));
        let decoded = decode(&message);
        assert_eq!(decoded.model.as_deref(), Some("INTEGRA-7"));
        assert!(decoded.summary.starts_with("DT1 @ 19 00 00 00"));
    }

    #[test]
    fn decodes_the_vendor_resets() {
        assert_eq!(
            decode(template("gs-reset").unwrap().bytes).summary,
            "DT1 GS Reset = 00"
        );
        assert_eq!(
            decode(template("xg-on").unwrap().bytes).summary,
            "XG System On"
        );
        assert_eq!(
            decode(template("gm-on").unwrap().bytes).summary,
            "GM System On"
        );
        let korg = decode(template("korg-program-request").unwrap().bytes);
        assert_eq!(korg.manufacturer, Some(Manufacturer::Korg));
        assert_eq!(korg.model.as_deref(), Some("microKORG"));
        assert_eq!(korg.summary, "Current Program Dump Request");
    }

    #[test]
    fn gs_part_blocks_map_to_channels() {
        assert_eq!(gs_part_channel(0x10), Some(10));
        assert_eq!(gs_part_channel(0x11), Some(1));
        assert_eq!(gs_part_channel(0x19), Some(9));
        assert_eq!(gs_part_channel(0x1A), Some(11));
        assert_eq!(gs_part_channel(0x1F), Some(16));
        let d = decode(template("gs-drum-ch11").unwrap().bytes);
        assert_eq!(d.summary, "DT1 Use for Rhythm Part (Ch 11) = 02");
    }

    #[test]
    fn xg_bulk_dump_checksum_is_checked() {
        // XG bulk: count 00 01, address 00 00 7E, data 00.
        let mut message = vec![
            0xF0, 0x43, 0x00, 0x4C, 0x00, 0x01, 0x00, 0x00, 0x7E, 0x00, 0x00, 0xF7,
        ];
        fix_checksum(&mut message);
        assert_eq!(checksum(&message), Checksum::Valid);
        assert_eq!(
            message[10],
            checksum_7bit(&[0x00, 0x01, 0x00, 0x00, 0x7E, 0x00])
        );
    }

    #[test]
    fn hex_accepts_the_common_spellings() {
        let want = vec![0xF0, 0x41, 0x10, 0xF7];
        assert_eq!(parse_hex("F0 41 10 F7").unwrap(), want);
        assert_eq!(parse_hex("f0,41,10,f7").unwrap(), want);
        assert_eq!(parse_hex("0xF0 0x41 0x10 0xF7").unwrap(), want);
        assert!(parse_hex("F0411 0F7").is_err());
        assert_eq!(parse_hex("F04110F7").unwrap(), want);
        assert_eq!(parse_hex("F0h 41h 10h F7h").unwrap(), want);
        assert!(matches!(
            parse_hex("F0 4G"),
            Err(HexError::BadToken { index: 2, .. })
        ));
        assert_eq!(format_hex(&want), "F0 41 10 F7");
    }

    #[test]
    fn frame_rejects_status_bytes_in_the_body() {
        assert_eq!(
            validate_frame(&[0xF0, 0x41, 0x90, 0xF7]),
            Err(FrameError::StatusInBody(2))
        );
        assert_eq!(validate_frame(&[0xF0, 0x41]), Err(FrameError::MissingEnd));
        assert_eq!(validate_frame(&[0x41, 0xF7]), Err(FrameError::MissingStart));
    }

    #[test]
    fn set_device_keeps_roland_checksum() {
        let mut m = template("gs-reset").unwrap().bytes.to_vec();
        assert!(set_device(&mut m, 0x11));
        assert_eq!(m[2], 0x11);
        assert_eq!(checksum(&m), Checksum::Valid);
        let mut y = template("xg-on").unwrap().bytes.to_vec();
        assert!(set_device(&mut y, 3));
        assert_eq!(y[2], 0x13);
    }

    #[test]
    fn smf_payload_round_trips() {
        let full = template("gm-on").unwrap().bytes.to_vec();
        let payload = to_smf_payload(&full).unwrap();
        assert_eq!(payload[0], 0x7E);
        assert_eq!(from_smf_payload(&payload), full);
    }
}
