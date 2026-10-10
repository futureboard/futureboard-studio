//! Open Sound Control 1.0, as much as the remote needs: messages and
//! bundles, with `i` (int32), `f` (float32), `s` (string), `T`/`F` (true,
//! false) — and `N`/`I` (nil, impulse), which carry no data — read and
//! written. Every other type tag makes the packet unreadable; the caller
//! counts it and moves on.
//!
//! Everything is big-endian and padded to 4 bytes; a bundle is `#bundle`, an
//! 8-byte time tag, then elements each prefixed by its size.

/// Bundles inside bundles are taken this deep.
const MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum OscArg {
    Int(i32),
    Float(f32),
    Str(String),
    Bool(bool),
    /// `N` or `I`: an argument with no value.
    Nil,
}

impl OscArg {
    /// The argument as a number, whatever its type (`T` = 1).
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            OscArg::Int(i) => Some(*i as f32),
            OscArg::Float(f) if f.is_finite() => Some(*f),
            OscArg::Bool(b) => Some(f32::from(u8::from(*b))),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OscMessage {
    pub address: String,
    pub args: Vec<OscArg>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OscPacket {
    Message(OscMessage),
    /// Bundled elements are applied at once: the time tag is read and
    /// ignored.
    Bundle(Vec<OscPacket>),
}

impl OscPacket {
    /// Every message, bundles flattened, in order.
    pub fn into_messages(self) -> Vec<OscMessage> {
        let mut out = Vec::new();
        self.flatten(&mut out);
        out
    }

    fn flatten(self, out: &mut Vec<OscMessage>) {
        match self {
            OscPacket::Message(message) => out.push(message),
            OscPacket::Bundle(packets) => {
                for packet in packets {
                    packet.flatten(out);
                }
            }
        }
    }
}

/// Read one UDP datagram.
pub fn decode(bytes: &[u8]) -> Result<OscPacket, &'static str> {
    decode_at(bytes, 0)
}

fn decode_at(bytes: &[u8], depth: usize) -> Result<OscPacket, &'static str> {
    if !bytes.len().is_multiple_of(4) || bytes.is_empty() {
        return Err("size is not a multiple of 4");
    }
    if bytes.starts_with(b"#bundle\0") {
        if depth >= MAX_DEPTH {
            return Err("bundles nested too deep");
        }
        let mut at = 16; // "#bundle\0" + time tag
        if bytes.len() < at {
            return Err("short bundle");
        }
        let mut packets = Vec::new();
        while at < bytes.len() {
            let size = read_i32(bytes, at)?;
            at += 4;
            let size = usize::try_from(size).map_err(|_| "negative element size")?;
            let end = at.checked_add(size).ok_or("element too long")?;
            if end > bytes.len() {
                return Err("element runs past the bundle");
            }
            packets.push(decode_at(&bytes[at..end], depth + 1)?);
            at = end;
        }
        return Ok(OscPacket::Bundle(packets));
    }
    if bytes[0] != b'/' {
        return Err("not a message or a bundle");
    }
    let (address, mut at) = read_str(bytes, 0)?;
    // A message with no type tag string at all: no arguments (OSC 1.0
    // allows it for old senders).
    if at >= bytes.len() {
        return Ok(OscPacket::Message(OscMessage {
            address,
            args: Vec::new(),
        }));
    }
    let (tags, next) = read_str(bytes, at)?;
    at = next;
    let Some(tags) = tags.strip_prefix(',') else {
        return Err("no type tag string");
    };
    let mut args = Vec::with_capacity(tags.len());
    for tag in tags.bytes() {
        match tag {
            b'i' => {
                args.push(OscArg::Int(read_i32(bytes, at)?));
                at += 4;
            }
            b'f' => {
                args.push(OscArg::Float(f32::from_bits(read_i32(bytes, at)? as u32)));
                at += 4;
            }
            b's' => {
                let (text, next) = read_str(bytes, at)?;
                args.push(OscArg::Str(text));
                at = next;
            }
            b'T' => args.push(OscArg::Bool(true)),
            b'F' => args.push(OscArg::Bool(false)),
            b'N' | b'I' => args.push(OscArg::Nil),
            _ => return Err("unsupported argument type"),
        }
    }
    Ok(OscPacket::Message(OscMessage { address, args }))
}

fn read_i32(bytes: &[u8], at: usize) -> Result<i32, &'static str> {
    let chunk = bytes.get(at..at + 4).ok_or("truncated argument")?;
    Ok(i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
}

/// A NUL-terminated string padded to 4 bytes; returns it and where the next
/// field starts.
fn read_str(bytes: &[u8], at: usize) -> Result<(String, usize), &'static str> {
    let rest = bytes.get(at..).ok_or("truncated string")?;
    let len = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or("unterminated string")?;
    let text = std::str::from_utf8(&rest[..len]).map_err(|_| "string is not UTF-8")?;
    let next = at + (len + 4) / 4 * 4;
    if next > bytes.len() {
        return Err("truncated string");
    }
    Ok((text.to_string(), next))
}

/// Append `message` in wire form.
pub fn encode_message(message: &OscMessage, out: &mut Vec<u8>) {
    write_str(&message.address, out);
    let mut tags = String::with_capacity(message.args.len() + 1);
    tags.push(',');
    for arg in &message.args {
        tags.push(match arg {
            OscArg::Int(_) => 'i',
            OscArg::Float(_) => 'f',
            OscArg::Str(_) => 's',
            OscArg::Bool(true) => 'T',
            OscArg::Bool(false) => 'F',
            OscArg::Nil => 'N',
        });
    }
    write_str(&tags, out);
    for arg in &message.args {
        match arg {
            OscArg::Int(i) => out.extend_from_slice(&i.to_be_bytes()),
            OscArg::Float(f) => out.extend_from_slice(&f.to_bits().to_be_bytes()),
            OscArg::Str(s) => write_str(s, out),
            OscArg::Bool(_) | OscArg::Nil => {}
        }
    }
}

/// One message as a datagram.
pub fn message_bytes(message: &OscMessage) -> Vec<u8> {
    let mut out = Vec::new();
    encode_message(message, &mut out);
    out
}

/// Messages packed into bundles ("immediately" time tag) of at most
/// `max_size` bytes each — one datagram per bundle. A message too big for
/// a bundle of its own still goes, alone.
pub fn bundles(messages: &[OscMessage], max_size: usize) -> Vec<Vec<u8>> {
    let mut datagrams = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut element = Vec::new();
    for message in messages {
        element.clear();
        encode_message(message, &mut element);
        if !current.is_empty() && current.len() + 4 + element.len() > max_size {
            datagrams.push(std::mem::take(&mut current));
        }
        if current.is_empty() {
            current.extend_from_slice(b"#bundle\0");
            current.extend_from_slice(&1u64.to_be_bytes());
        }
        current.extend_from_slice(&(element.len() as i32).to_be_bytes());
        current.extend_from_slice(&element);
    }
    if !current.is_empty() {
        datagrams.push(current);
    }
    datagrams
}

fn write_str(text: &str, out: &mut Vec<u8>) {
    // A NUL inside would end the string early: cut there.
    let text = text.split('\0').next().unwrap_or_default();
    out.extend_from_slice(text.as_bytes());
    let pad = 4 - text.len() % 4;
    out.extend(std::iter::repeat_n(0u8, pad));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(address: &str, args: Vec<OscArg>) -> OscMessage {
        OscMessage {
            address: address.to_string(),
            args,
        }
    }

    #[test]
    fn a_message_reads_as_the_spec_lays_it_out() {
        // The OSC 1.0 spec's own example: "/oscillator/4/frequency" ,f 440.0
        let mut bytes = b"/oscillator/4/frequency\0,f\0\0".to_vec();
        bytes.extend_from_slice(&[0x43, 0xdc, 0x00, 0x00]);
        assert_eq!(
            decode(&bytes).unwrap(),
            OscPacket::Message(message(
                "/oscillator/4/frequency",
                vec![OscArg::Float(440.0)]
            ))
        );
        // And "/foo" ,iisff 1000 -1 "hello" 1.234 5.678
        let mut bytes = b"/foo\0\0\0\0,iisff\0\0".to_vec();
        bytes.extend_from_slice(&1000i32.to_be_bytes());
        bytes.extend_from_slice(&(-1i32).to_be_bytes());
        bytes.extend_from_slice(b"hello\0\0\0");
        bytes.extend_from_slice(&1.234f32.to_be_bytes());
        bytes.extend_from_slice(&5.678f32.to_be_bytes());
        let OscPacket::Message(m) = decode(&bytes).unwrap() else {
            panic!("a message");
        };
        assert_eq!(m.address, "/foo");
        assert_eq!(
            m.args,
            vec![
                OscArg::Int(1000),
                OscArg::Int(-1),
                OscArg::Str("hello".into()),
                OscArg::Float(1.234),
                OscArg::Float(5.678)
            ]
        );
    }

    #[test]
    fn what_is_written_reads_back() {
        for m in [
            message("/ch/1/fader", vec![OscArg::Float(0.75)]),
            message("/ch/12/mute", vec![OscArg::Int(1)]),
            message("/ch/3/name", vec![OscArg::Str("Kick In".into())]),
            message("/abc", vec![OscArg::Str(String::new())]),
            message(
                "/t",
                vec![OscArg::Bool(true), OscArg::Bool(false), OscArg::Nil],
            ),
            message("/livestage/subscribe", vec![]),
        ] {
            let bytes = message_bytes(&m);
            assert_eq!(bytes.len() % 4, 0, "{m:?}");
            assert_eq!(decode(&bytes).unwrap(), OscPacket::Message(m));
        }
    }

    #[test]
    fn bundles_flatten_in_order_and_split_by_size() {
        let messages: Vec<OscMessage> = (1..=40)
            .map(|n| message(&format!("/ch/{n}/fader"), vec![OscArg::Float(0.5)]))
            .collect();
        let datagrams = bundles(&messages, 256);
        assert!(datagrams.len() > 1);
        let mut back = Vec::new();
        for datagram in &datagrams {
            assert!(datagram.len() <= 256);
            back.extend(decode(datagram).unwrap().into_messages());
        }
        assert_eq!(back, messages);

        // A bundle inside a bundle.
        let inner = bundles(&messages[..2], 1024).remove(0);
        let mut outer = b"#bundle\0".to_vec();
        outer.extend_from_slice(&1u64.to_be_bytes());
        outer.extend_from_slice(&(inner.len() as i32).to_be_bytes());
        outer.extend_from_slice(&inner);
        let single = message_bytes(&messages[2]);
        outer.extend_from_slice(&(single.len() as i32).to_be_bytes());
        outer.extend_from_slice(&single);
        assert_eq!(decode(&outer).unwrap().into_messages(), messages[..3]);
    }

    #[test]
    fn broken_packets_are_refused_not_panicked_on() {
        let good = message_bytes(&message("/ch/1/fader", vec![OscArg::Float(0.5)]));
        for cut in 0..good.len() {
            let _ = decode(&good[..cut]);
        }
        assert!(decode(b"").is_err());
        assert!(decode(b"abc\0").is_err());
        assert!(decode(b"/a\0\0,d\0\0\0\0\0\0\0\0\0\0").is_err(), "double");
        assert!(decode(b"/a\0\0,i\0\0").is_err(), "missing int");
        assert!(decode(b"/a\0\0,s\0\0abcd").is_err(), "unterminated");
        let mut bundle = b"#bundle\0".to_vec();
        bundle.extend_from_slice(&[0; 8]);
        bundle.extend_from_slice(&64i32.to_be_bytes());
        assert!(decode(&bundle).is_err(), "element past the end");
        // No type tags: no arguments.
        assert_eq!(
            decode(b"/scene/next\0").unwrap(),
            OscPacket::Message(message("/scene/next", vec![]))
        );
        // Too deep.
        let mut nested = message_bytes(&message("/x", vec![]));
        for _ in 0..6 {
            let mut outer = b"#bundle\0".to_vec();
            outer.extend_from_slice(&[0; 8]);
            outer.extend_from_slice(&(nested.len() as i32).to_be_bytes());
            outer.extend_from_slice(&nested);
            nested = outer;
        }
        assert!(decode(&nested).is_err());
    }
}
