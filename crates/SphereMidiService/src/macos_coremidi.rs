//! Thin CoreMIDI bindings for macOS MIDI port scan and hardware I/O.
//!
//! Avoids the Rust `midir`/`coremidi` crates so we do not fight gpui's pinned
//! `core-foundation` version. Links Apple's CoreMIDI + CoreFoundation frameworks
//! directly.

use crate::{
    DetectedMidiDevice, HardwareMidiInputMessage, MidiDeviceDirection,
    coalesce_detected_midi_devices, decode_midi_bytes, stable_id, stable_id_ordinal,
};
use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_ulong};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

type OSStatus = c_int;
type MIDIObjectRef = u32;
type MIDIClientRef = u32;
type MIDIPortRef = u32;
type MIDIEndpointRef = u32;
type MIDITimeStamp = u64;
type CFStringRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFIndex = isize;
type CFStringEncoding = u32;

// CoreMIDI property keys are plain CFStrings compared by content, so building
// them here is equivalent to linking Apple's `kMIDIProperty*` globals (the
// `coremidi` crate exposes both spellings for the same reason).
const K_MIDI_PROPERTY_NAME: &[u8] = b"name\0";
/// `kMIDIPropertyDisplayName` — the name Apple tells hosts to show. CoreMIDI
/// composes it from the device, entity, and endpoint names, which is why this
/// module reads it instead of walking Device → Entity → Source by hand.
const K_MIDI_PROPERTY_DISPLAY_NAME: &[u8] = b"displayName\0";
const K_MIDI_PROPERTY_MANUFACTURER: &[u8] = b"manufacturer\0";
const K_MIDI_PROPERTY_MODEL: &[u8] = b"model\0";
/// `kMIDIPropertyUniqueID` — the identifier CoreMIDI keeps stable for an
/// endpoint across replugs. Logged alongside the display name so a device that
/// resolves to the wrong endpoint can be told apart from one that never
/// resolved at all, which display names alone cannot distinguish.
const K_MIDI_PROPERTY_UNIQUE_ID: &[u8] = b"uniqueID\0";
/// Reported, never filtered on: an offline endpoint is a device CoreMIDI still
/// remembers, and hiding it is how a replugged keyboard goes missing.
const K_MIDI_PROPERTY_OFFLINE: &[u8] = b"offline\0";
const K_MIDI_PROPERTY_PRIVATE: &[u8] = b"private\0";
const K_CF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;
const MIDI_PACKET_LIST_BUF: usize = 512;
/// Largest message one `MIDIPacket` in [`MIDI_PACKET_LIST_BUF`] carries.
const MAX_PACKET_BYTES: usize = 256;

/// Maximum payload of a single `MIDIPacket`, from `MIDIServices.h`.
const PACKET_DATA_CAPACITY: usize = 256;

/// `MIDIPacket` is declared under `#pragma pack(4)` on every Apple target.
///
/// This is not a detail we can leave to Rust's natural `#[repr(C)]` rules: with
/// default packing, `{ u64, u16, [u8; 256] }` gets 8-byte alignment, which
/// pushes the first packet of a [`MIDIPacketList`] to offset 8 instead of 4.
/// Every field then reads four bytes late — `length` lands on the third data
/// byte and the payload is taken from past the end of the message — so incoming
/// MIDI decodes as garbage. Declaring the packing explicitly is what makes the
/// layout match CoreMIDI on both Apple Silicon and Intel.
#[repr(C, packed(4))]
struct MIDIPacket {
    time_stamp: MIDITimeStamp,
    length: u16,
    data: [u8; PACKET_DATA_CAPACITY],
}

#[repr(C)]
struct MIDIPacketList {
    num_packets: u32,
    packet: MIDIPacket,
}

/// Offsets used to walk a packet list without ever forming a reference to a
/// packed field. Checked against the struct declarations below so a layout
/// mistake fails the macOS build instead of silently decoding noise.
const PACKET_LIST_FIRST_PACKET: usize = 4;
const PACKET_LENGTH_OFFSET: usize = 8;
const PACKET_DATA_OFFSET: usize = 10;

const _: () = {
    assert!(std::mem::align_of::<MIDIPacket>() == 4);
    assert!(std::mem::size_of::<MIDIPacket>() == 268);
    assert!(std::mem::align_of::<MIDIPacketList>() == 4);
    assert!(std::mem::size_of::<MIDIPacketList>() == 272);
    assert!(std::mem::offset_of!(MIDIPacketList, packet) == PACKET_LIST_FIRST_PACKET);
    assert!(std::mem::offset_of!(MIDIPacket, length) == PACKET_LENGTH_OFFSET);
    assert!(std::mem::offset_of!(MIDIPacket, data) == PACKET_DATA_OFFSET);
};

/// Start of the packet that follows one of `length` bytes.
///
/// Mirrors `MIDIPacketNext` in `MIDIServices.h`, which is architecture
/// dependent: ARM aligns each packet up to four bytes, Intel packs them end to
/// end. Getting this wrong only shows up on multi-message packet lists, so it
/// has to come from the header rather than from what happens to work locally.
#[inline]
unsafe fn next_packet(packet: *const u8, length: usize) -> *const u8 {
    // SAFETY: the caller walks at most `num_packets` entries of a live list, so
    // the end of this packet is still inside it.
    let end = unsafe { packet.add(PACKET_DATA_OFFSET + length) };
    if cfg!(any(target_arch = "aarch64", target_arch = "arm")) {
        ((end as usize + 3) & !3) as *const u8
    } else {
        end
    }
}

/// Stack buffer for building an outgoing packet list. CoreMIDI writes a
/// `MIDIPacketList` into it, so it has to carry that type's alignment — a bare
/// `[u8; N]` is only byte-aligned.
#[repr(C, align(4))]
struct PacketListBuffer([u8; MIDI_PACKET_LIST_BUF]);

#[link(name = "CoreMIDI", kind = "framework")]
unsafe extern "C" {
    fn MIDIGetNumberOfSources() -> c_ulong;
    fn MIDIGetSource(source_index_0: c_ulong) -> MIDIEndpointRef;
    fn MIDIGetNumberOfDestinations() -> c_ulong;
    fn MIDIGetDestination(dest_index_0: c_ulong) -> MIDIEndpointRef;
    fn MIDIObjectGetStringProperty(
        obj: MIDIObjectRef,
        property_id: CFStringRef,
        str: *mut CFStringRef,
    ) -> OSStatus;
    fn MIDIObjectGetIntegerProperty(
        obj: MIDIObjectRef,
        property_id: CFStringRef,
        out_value: *mut i32,
    ) -> OSStatus;
    fn MIDIClientCreate(
        name: CFStringRef,
        notify_proc: Option<unsafe extern "C" fn(*const MIDINotification, *mut c_void)>,
        notify_ref_con: *mut c_void,
        out_client: *mut MIDIClientRef,
    ) -> OSStatus;
    fn MIDIClientDispose(client: MIDIClientRef) -> OSStatus;
    fn MIDIInputPortCreate(
        client: MIDIClientRef,
        port_name: CFStringRef,
        read_proc: Option<unsafe extern "C" fn(*const MIDIPacketList, *mut c_void, *mut c_void)>,
        ref_con: *mut c_void,
        out_port: *mut MIDIPortRef,
    ) -> OSStatus;
    fn MIDIOutputPortCreate(
        client: MIDIClientRef,
        port_name: CFStringRef,
        out_port: *mut MIDIPortRef,
    ) -> OSStatus;
    fn MIDIPortConnectSource(
        port: MIDIPortRef,
        source: MIDIEndpointRef,
        conn_ref_con: *mut c_void,
    ) -> OSStatus;
    fn MIDIPortDisconnectSource(port: MIDIPortRef, source: MIDIEndpointRef) -> OSStatus;
    fn MIDIPortDispose(port: MIDIPortRef) -> OSStatus;
    fn MIDISend(
        port: MIDIPortRef,
        dest: MIDIEndpointRef,
        pktlist: *const MIDIPacketList,
    ) -> OSStatus;
    fn MIDIPacketListInit(pktlist: *mut MIDIPacketList) -> *mut MIDIPacket;
    fn MIDIPacketListAdd(
        pktlist: *mut MIDIPacketList,
        list_size: c_ulong,
        cur_packet: *mut MIDIPacket,
        time: MIDITimeStamp,
        n_data: c_ulong,
        data: *const u8,
    ) -> *mut MIDIPacket;
}

// Separate block: these live in CoreFoundation, not CoreMIDI. Stacking both
// `#[link]` attributes on one block also trips `clippy::duplicated_attributes`,
// which the macOS CI job denies.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: CFStringEncoding,
    ) -> CFStringRef;
    fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: CFStringEncoding,
    ) -> u8;
    fn CFStringGetLength(the_string: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: CFStringEncoding) -> CFIndex;
    fn CFRelease(cf: *const c_void);
}

/// CoreMIDI notification message ids we care about. Any of these means the port
/// inventory moved, so a cached device list is stale.
const K_MIDI_MSG_SETUP_CHANGED: i32 = 1;
const K_MIDI_MSG_OBJECT_ADDED: i32 = 2;
const K_MIDI_MSG_OBJECT_REMOVED: i32 = 3;

#[repr(C)]
struct MIDINotification {
    message_id: i32,
    message_size: u32,
}

/// The process-wide CoreMIDI client, and whether the inventory changed since it
/// was last checked.
static CLIENT: AtomicU32 = AtomicU32::new(0);
/// Serializes creation attempts. Not a `Once`: see [`shared_client`].
static CLIENT_INIT_LOCK: Mutex<()> = Mutex::new(());
static CLIENT_ATTEMPTS: AtomicU32 = AtomicU32::new(0);
static PORTS_CHANGED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn midi_notify_proc(message: *const MIDINotification, _ref_con: *mut c_void) {
    if message.is_null() {
        return;
    }
    // SAFETY: CoreMIDI passes a live notification for the duration of the call.
    let message_id = unsafe { (*message).message_id };
    if matches!(
        message_id,
        K_MIDI_MSG_SETUP_CHANGED | K_MIDI_MSG_OBJECT_ADDED | K_MIDI_MSG_OBJECT_REMOVED
    ) {
        PORTS_CHANGED.store(true, Ordering::Release);
    }
}

/// The shared CoreMIDI client, created on demand and **retried until it
/// succeeds**.
///
/// **This is what makes enumeration work at all.** CoreMIDI connects a process
/// to `MIDIServer` lazily, when it first creates a client; until then
/// `MIDIGetNumberOfSources` / `MIDIGetNumberOfDestinations` report **zero** even
/// with hardware attached. Every other CoreMIDI implementation (RtMidi, midir's
/// CoreMIDI backend) creates a client before enumerating for this reason.
///
/// Retrying is the whole point of this function's shape. This used to be a
/// `std::sync::Once`, but `call_once` marks the `Once` completed even when the
/// closure bails out early — so one failed `MIDIClientCreate` latched the client
/// at 0 for the rest of the process, and *every* later attempt short-circuited:
/// the resilient scan's retries, the two-second startup rescan, the Preferences
/// Refresh button, and the periodic device sync all silently returned zero
/// devices until the app was restarted. The first attempt happens on a
/// background scan thread during splash, which is exactly when `MIDIServer` is
/// most likely to still be unreachable (`kMIDIServerStartErr`, -10844), so the
/// failure that gets latched is a transient one.
///
/// The client is deliberately never disposed: it is the process's connection to
/// the MIDI server, and tearing it down would take the port inventory with it.
/// Returns 0 when creation failed, which callers treat as "enumerate anyway" —
/// a failed bootstrap must degrade to an empty list, not to a panic.
fn shared_client() -> MIDIClientRef {
    let existing = CLIENT.load(Ordering::Acquire);
    if existing != 0 {
        return existing;
    }

    let _guard = CLIENT_INIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Another thread may have created it while this one waited for the lock.
    let existing = CLIENT.load(Ordering::Acquire);
    if existing != 0 {
        return existing;
    }

    let attempt = CLIENT_ATTEMPTS.fetch_add(1, Ordering::Relaxed) + 1;
    let name = cfstr("Futureboard");
    if name.is_null() {
        eprintln!("[MIDI scan] CoreMIDI client name allocation failed (attempt {attempt})");
        return 0;
    }
    let mut client: MIDIClientRef = 0;
    let status =
        unsafe { MIDIClientCreate(name, Some(midi_notify_proc), ptr::null_mut(), &mut client) };
    unsafe { CFRelease(name) };
    if status != 0 || client == 0 {
        // Throttled: this is retried from the 500 ms device sync, so logging
        // every failure would flood the log while still saying nothing new.
        if attempt == 1 || attempt.is_multiple_of(20) {
            eprintln!(
                "[MIDI scan] MIDIClientCreate failed (status {status}, attempt {attempt}); \
                 no MIDI devices can be listed until it succeeds. \
                 -10844 is kMIDIServerStartErr — the process could not reach MIDIServer."
            );
        }
        return 0;
    }
    CLIENT.store(client, Ordering::Release);
    // Logged unconditionally and exactly once: this single line is what
    // separates "CoreMIDI never came up" from "CoreMIDI is up but reports no
    // sources" when a user says no devices appear.
    eprintln!("[MIDI scan] CoreMIDI client created (ref {client}, attempt {attempt})");
    client
}

/// Endpoint count observed on the previous [`take_ports_changed`] call.
/// `u32::MAX` means "not sampled yet", so the first call never reports a change.
static LAST_ENDPOINT_COUNT: AtomicU32 = AtomicU32::new(u32::MAX);

/// True when the CoreMIDI port inventory changed since the last call, and
/// clears the flag.
///
/// Two signals, because neither is sufficient alone. The notify proc is exact
/// but only fires on the run loop of the thread that created the client — and
/// that is whichever thread scanned first, often a background scan thread whose
/// run loop never runs, in which case notifications never arrive at all.
/// Comparing the endpoint count is a cheap allocation-free safety net so a
/// controller plugged in after launch is still picked up by the periodic device
/// sync instead of waiting for a manual Rescan.
pub fn take_ports_changed() -> bool {
    let notified = PORTS_CHANGED.swap(false, Ordering::AcqRel);
    if shared_client() == 0 {
        return notified;
    }
    let count =
        unsafe { MIDIGetNumberOfSources().saturating_add(MIDIGetNumberOfDestinations()) as u32 };
    let previous = LAST_ENDPOINT_COUNT.swap(count, Ordering::AcqRel);
    notified || (previous != u32::MAX && previous != count)
}

fn cfstr(s: &str) -> CFStringRef {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    unsafe {
        CFStringCreateWithCString(
            ptr::null(),
            bytes.as_ptr() as *const c_char,
            K_CF_STRING_ENCODING_UTF8,
        )
    }
}

fn cfstring_to_rust(cf: CFStringRef) -> Option<String> {
    if cf.is_null() {
        return None;
    }
    let mut buf = [0i8; 512];
    let ok = unsafe {
        CFStringGetCString(
            cf,
            buf.as_mut_ptr(),
            buf.len() as CFIndex,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if ok != 0 {
        let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
        return Some(cstr.to_string_lossy().into_owned());
    }

    // The stack buffer was too small (`CFStringGetCString` is all-or-nothing).
    // Size the next one from the string itself rather than giving up — a name
    // we cannot read is a device the user cannot select.
    let length = unsafe { CFStringGetLength(cf) };
    if length <= 0 {
        return None;
    }
    let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, K_CF_STRING_ENCODING_UTF8) };
    if capacity <= 0 {
        return None;
    }
    let mut heap = vec![0i8; capacity as usize + 1];
    let ok = unsafe {
        CFStringGetCString(
            cf,
            heap.as_mut_ptr(),
            heap.len() as CFIndex,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if ok == 0 {
        return None;
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(heap.as_ptr()) };
    Some(cstr.to_string_lossy().into_owned())
}

/// Read a CFString property, treating blank as absent so callers fall through
/// to the next candidate instead of showing an unselectable empty row.
fn endpoint_string_property(endpoint: MIDIEndpointRef, property: &[u8]) -> Option<String> {
    if endpoint == 0 {
        return None;
    }
    let prop = unsafe {
        CFStringCreateWithCString(
            ptr::null(),
            property.as_ptr() as *const c_char,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if prop.is_null() {
        return None;
    }
    let mut value_ref: CFStringRef = ptr::null();
    let status = unsafe { MIDIObjectGetStringProperty(endpoint, prop, &mut value_ref) };
    unsafe { CFRelease(prop) };
    if status != 0 || value_ref.is_null() {
        return None;
    }
    let value = cfstring_to_rust(value_ref);
    unsafe { CFRelease(value_ref) };
    value.filter(|value| !value.trim().is_empty())
}

/// Read an integer property (`uniqueID`, `offline`, `private`, ...).
fn endpoint_int_property(endpoint: MIDIEndpointRef, property: &[u8]) -> Option<i32> {
    if endpoint == 0 {
        return None;
    }
    let prop = unsafe {
        CFStringCreateWithCString(
            ptr::null(),
            property.as_ptr() as *const c_char,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if prop.is_null() {
        return None;
    }
    let mut value: i32 = 0;
    let status = unsafe { MIDIObjectGetIntegerProperty(endpoint, prop, &mut value) };
    unsafe { CFRelease(prop) };
    (status == 0).then_some(value)
}

/// Where an endpoint's name came from, for the scan log.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NameSource {
    DisplayName,
    Name,
    ManufacturerModel,
    Synthesized,
}

impl NameSource {
    fn label(self) -> &'static str {
        match self {
            NameSource::DisplayName => "displayName",
            NameSource::Name => "name",
            NameSource::ManufacturerModel => "manufacturer/model",
            NameSource::Synthesized => "synthesized",
        }
    }
}

/// A user-facing name for an endpoint. **Never fails.**
///
/// Optional metadata being unreadable is not a reason to hide hardware: the
/// previous version returned `Option` from a single `kMIDIPropertyName` read and
/// the caller dropped the endpoint on `None`, so one unnamed endpoint became one
/// missing device with no explanation. Falls back through the properties Apple
/// documents, then to a synthetic name built from the stable unique ID.
fn endpoint_display_name(
    endpoint: MIDIEndpointRef,
    direction: MidiDeviceDirection,
    index: usize,
) -> (String, NameSource) {
    if let Some(name) = endpoint_string_property(endpoint, K_MIDI_PROPERTY_DISPLAY_NAME) {
        return (name, NameSource::DisplayName);
    }
    if let Some(name) = endpoint_string_property(endpoint, K_MIDI_PROPERTY_NAME) {
        return (name, NameSource::Name);
    }
    let manufacturer = endpoint_string_property(endpoint, K_MIDI_PROPERTY_MANUFACTURER);
    let model = endpoint_string_property(endpoint, K_MIDI_PROPERTY_MODEL);
    match (manufacturer, model) {
        (Some(manufacturer), Some(model)) => {
            return (
                format!("{manufacturer} {model}"),
                NameSource::ManufacturerModel,
            );
        }
        (Some(only), None) | (None, Some(only)) => return (only, NameSource::ManufacturerModel),
        (None, None) => {}
    }
    let label = match direction {
        MidiDeviceDirection::Output => "MIDI Destination",
        _ => "MIDI Source",
    };
    let suffix = endpoint_int_property(endpoint, K_MIDI_PROPERTY_UNIQUE_ID)
        .map(|id| id.to_string())
        .unwrap_or_else(|| format!("#{}", index + 1));
    (format!("{label} {suffix}"), NameSource::Synthesized)
}

/// Enumerate one direction's endpoints into `devices`.
///
/// Uses CoreMIDI's flat endpoint enumeration (`MIDIGetNumberOfSources` /
/// `MIDIGetSource`) rather than walking Device → Entity → Source. That is
/// deliberate: the flat list is the only one that includes endpoints with no
/// backing `MIDIDeviceRef` — IAC buses, Network Session, and virtual ports
/// published by other apps — so the topology walk would *lose* devices, not
/// find more. It is also what RtMidi and midir's CoreMIDI backend use.
///
/// Every endpoint the OS reports becomes a device. Nothing is filtered on
/// offline, private, manufacturer, model, or a failed name lookup.
fn scan_direction(
    direction: MidiDeviceDirection,
    count: usize,
    get_endpoint: impl Fn(usize) -> MIDIEndpointRef,
    devices: &mut Vec<DetectedMidiDevice>,
) {
    let debug = crate::midi_settings_debug_enabled();
    let kind = if direction == MidiDeviceDirection::Output {
        "destination"
    } else {
        "source"
    };
    for index in 0..count {
        let endpoint = get_endpoint(index);
        if endpoint == 0 {
            // The only rejection in this loop, and it is not a real endpoint.
            eprintln!(
                "[MIDI scan] {kind}[{index}] rejected: CoreMIDI returned a null endpoint ref"
            );
            continue;
        }
        let (raw_name, name_source) = endpoint_display_name(endpoint, direction, index);
        let name = crate::normalize_port_name(&raw_name);
        if debug {
            eprintln!(
                "[MIDI scan] {kind}[{index}] endpoint={endpoint} unique_id={} name={name:?} \
                 name_from={} manufacturer={:?} model={:?} offline={} private={} accepted=true",
                endpoint_int_property(endpoint, K_MIDI_PROPERTY_UNIQUE_ID)
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                name_source.label(),
                endpoint_string_property(endpoint, K_MIDI_PROPERTY_MANUFACTURER),
                endpoint_string_property(endpoint, K_MIDI_PROPERTY_MODEL),
                endpoint_int_property(endpoint, K_MIDI_PROPERTY_OFFLINE).unwrap_or(0) != 0,
                endpoint_int_property(endpoint, K_MIDI_PROPERTY_PRIVATE).unwrap_or(0) != 0,
            );
        }
        devices.push(DetectedMidiDevice {
            id: stable_id(direction, &name),
            name,
            direction,
        });
    }
}

pub fn scan_ports() -> Vec<DetectedMidiDevice> {
    // Establish the MIDIServer connection first. Without it CoreMIDI reports
    // zero endpoints regardless of what is plugged in.
    let client = shared_client();
    let mut devices = Vec::new();
    let (n_src, n_dst) = unsafe {
        (
            MIDIGetNumberOfSources() as usize,
            MIDIGetNumberOfDestinations() as usize,
        )
    };

    scan_direction(
        MidiDeviceDirection::Input,
        n_src,
        |index| unsafe { MIDIGetSource(index as c_ulong) },
        &mut devices,
    );
    scan_direction(
        MidiDeviceDirection::Output,
        n_dst,
        |index| unsafe { MIDIGetDestination(index as c_ulong) },
        &mut devices,
    );

    let enumerated = devices.len();
    let coalesced = coalesce_detected_midi_devices(devices);

    if crate::midi_settings_debug_enabled() {
        eprintln!(
            "[MIDI scan] CoreMIDI client={client} sources={n_src} destinations={n_dst} \
             enumerated={enumerated} listed={}",
            coalesced.len()
        );
    }
    // Make every "no devices" outcome say which layer produced it, without
    // needing a debug build or an env var to find out.
    if coalesced.is_empty() {
        if client == 0 {
            eprintln!(
                "[MIDI scan] no MIDI devices: CoreMIDI client unavailable (MIDIClientCreate has \
                 not succeeded yet); enumeration always reports zero without one"
            );
        } else if n_src == 0 && n_dst == 0 {
            eprintln!(
                "[MIDI scan] no MIDI devices: CoreMIDI client is up (ref {client}) but reports \
                 zero sources and zero destinations — the OS itself sees no endpoints"
            );
        } else {
            eprintln!(
                "[MIDI scan] no MIDI devices listed despite CoreMIDI reporting \
                 sources={n_src} destinations={n_dst} — endpoints were discarded after \
                 enumeration; re-run with FUTUREBOARD_MIDI_SETTINGS_DEBUG=1 for per-endpoint detail"
            );
        }
    } else if coalesced.len() < enumerated {
        // Not an error — input+output pairs of one device merge into a single
        // bidirectional row — but worth being able to see.
        if crate::midi_settings_debug_enabled() {
            eprintln!(
                "[MIDI scan] {} endpoint(s) merged into bidirectional devices",
                enumerated - coalesced.len()
            );
        }
    }
    coalesced
}

struct InputCallbackState {
    tx: crate::MidiInputSender,
    device_id: String,
    device_name: String,
    /// Running status carried across packets (0 = none). Atomic because the box
    /// is owned by the control thread but only ever touched by CoreMIDI's read
    /// thread, so a plain field would be a data race by construction.
    running_status: AtomicU8,
}

/// CoreMIDI read proc. Runs on a high-priority CoreMIDI thread — never the
/// audio render thread and never the UI thread — so it does the least it can:
/// frame the packet bytes into messages, decode them, and hand them to the
/// existing bounded queue that the control-thread poll drains. No locks, no
/// I/O, no logging, no plugin work.
unsafe extern "C" fn midi_read_proc(
    pktlist: *const MIDIPacketList,
    read_proc_ref_con: *mut c_void,
    _src_conn_ref_con: *mut c_void,
) {
    if pktlist.is_null() || read_proc_ref_con.is_null() {
        return;
    }
    // SAFETY: CoreMIDI invokes this with a live packet list and the ref_con we
    // registered, which MacMidiInputConnection keeps boxed for the port's
    // lifetime and releases only after the port is disconnected and disposed.
    unsafe {
        let state = &*(read_proc_ref_con as *const InputCallbackState);
        let received_at = std::time::Instant::now();

        let mut running = match state.running_status.load(Ordering::Relaxed) {
            0 => None,
            status => Some(status),
        };
        let (mut messages, mut events, mut ignored, mut dropped) = (0u64, 0u64, 0u64, 0u64);
        let mut malformed = 0u64;

        for_each_packet(pktlist, |data| {
            malformed += crate::split_midi_messages(data, &mut running, |message| {
                messages += 1;
                match decode_midi_bytes(message) {
                    Some(event) => {
                        events += 1;
                        if state
                            .tx
                            .send(HardwareMidiInputMessage {
                                device_id: state.device_id.clone(),
                                device_name: state.device_name.clone(),
                                event,
                                received_at,
                            })
                            .is_err()
                        {
                            dropped += 1;
                        }
                    }
                    // Clock, sysex, aftertouch, pitch bend: real MIDI the
                    // internal event model does not carry. Counted, not lost.
                    None => ignored += 1,
                }
            }) as u64;
        });

        state
            .running_status
            .store(running.unwrap_or(0), Ordering::Relaxed);
        crate::note_midi_input_callback(messages, events, ignored, malformed, dropped);
    }
}

/// Hand each packet's payload in `pktlist` to `visit`, in order.
///
/// Reads through raw offsets rather than the packed struct so no reference to
/// an unaligned field is ever formed, and so the traversal can be exercised by
/// [`tests::a_packet_list_is_walked_at_the_layout_core_midi_actually_uses`]
/// without a CoreMIDI device attached.
///
/// # Safety
/// `pktlist` must point at a live `MIDIPacketList` holding at least
/// `num_packets` well-formed packets, as CoreMIDI guarantees for a read proc.
unsafe fn for_each_packet(pktlist: *const MIDIPacketList, mut visit: impl FnMut(&[u8])) {
    unsafe {
        let list = pktlist as *const u8;
        let num_packets = (list as *const u32).read_unaligned();
        let mut packet = list.add(PACKET_LIST_FIRST_PACKET);
        for _ in 0..num_packets {
            let length = (packet.add(PACKET_LENGTH_OFFSET) as *const u16).read_unaligned() as usize;
            // A packet can never exceed its declared payload; clamping keeps the
            // slice inside the buffer even if the length field is nonsense.
            let length = length.min(PACKET_DATA_CAPACITY);
            visit(std::slice::from_raw_parts(
                packet.add(PACKET_DATA_OFFSET),
                length,
            ));
            packet = next_packet(packet, length);
        }
    }
}

/// Keeps a CoreMIDI input port connected for as long as the device is enabled.
///
/// Owns everything the read proc touches: the port it was registered on, the
/// source it is connected to, and the boxed callback state the `ref_con` points
/// at. The port hangs off the process-wide [`shared_client`], so enabling and
/// disabling a device costs a port rather than a whole MIDIServer connection.
pub struct MacMidiInputConnection {
    port: MIDIPortRef,
    source: MIDIEndpointRef,
    /// Referenced by the read proc through a raw pointer. Boxed so the address
    /// survives moving this struct into the connection list, and dropped only
    /// after the port is torn down below.
    _callback: Box<InputCallbackState>,
}

impl Drop for MacMidiInputConnection {
    fn drop(&mut self) {
        unsafe {
            if self.port != 0 {
                // Disconnect before disposing so CoreMIDI stops feeding this
                // port first; the boxed callback state is released only after
                // both calls have returned.
                if self.source != 0 {
                    let _ = MIDIPortDisconnectSource(self.port, self.source);
                }
                let _ = MIDIPortDispose(self.port);
            }
        }
    }
}

fn find_source_by_name_or_id(device_id: &str, device_name: &str) -> Option<MIDIEndpointRef> {
    // Same bootstrap as the scan: without a client there are no endpoints to
    // find, so opening would fail with a misleading "source not found".
    let _ = shared_client();
    let ordinal = stable_id_ordinal(device_id, device_name);
    let mut occurrence = 0usize;
    unsafe {
        let n = MIDIGetNumberOfSources() as usize;
        for index in 0..n {
            let src = MIDIGetSource(index as c_ulong);
            if src == 0 {
                continue;
            }
            // Must resolve names exactly as `scan_direction` did, fallbacks
            // included — a lookup that disagrees with the scan turns "enabled in
            // Preferences" into "source not found" for any endpoint whose name
            // came from a fallback.
            let (raw_name, _) = endpoint_display_name(src, MidiDeviceDirection::Input, index);
            if crate::normalize_port_name(&raw_name) == device_name {
                occurrence += 1;
                if occurrence == ordinal {
                    return Some(src);
                }
            }
        }
    }
    None
}

fn find_destination_by_name_or_id(device_id_or_name: &str) -> Option<(MIDIEndpointRef, String)> {
    let _ = shared_client();
    let mut occurrences = std::collections::HashMap::<String, usize>::new();
    unsafe {
        let n = MIDIGetNumberOfDestinations() as usize;
        for index in 0..n {
            let dst = MIDIGetDestination(index as c_ulong);
            if dst != 0 {
                let (raw_name, _) = endpoint_display_name(dst, MidiDeviceDirection::Output, index);
                let name = crate::normalize_port_name(&raw_name);
                let stable = stable_id(MidiDeviceDirection::Output, &name);
                let stable_io = stable_id(MidiDeviceDirection::InputOutput, &name);
                let occurrence = occurrences.entry(name.clone()).or_insert(0);
                *occurrence += 1;
                let ordinal = stable_id_ordinal(device_id_or_name, &name);
                if name == device_id_or_name
                    || ((stable == device_id_or_name
                        || stable_io == device_id_or_name
                        || ordinal > 1)
                        && *occurrence == ordinal)
                {
                    return Some((dst, name));
                }
            }
        }
    }
    None
}

pub fn open_inputs(
    enabled: Vec<(String, String)>,
    tx: crate::MidiInputSender,
) -> Vec<(String, MacMidiInputConnection)> {
    let mut connections = Vec::new();
    let client = shared_client();
    if client == 0 {
        eprintln!(
            "[MIDI input] no CoreMIDI client — hardware MIDI input is unavailable this session"
        );
        return connections;
    }

    for (device_id, device_name) in enabled {
        let Some(source) = find_source_by_name_or_id(&device_id, &device_name) else {
            eprintln!("[MIDI input] CoreMIDI source not found for '{device_name}'");
            continue;
        };
        let mut callback = Box::new(InputCallbackState {
            tx: tx.clone(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            running_status: AtomicU8::new(0),
        });
        let callback_ptr = (&mut *callback) as *mut InputCallbackState as *mut c_void;

        let mut port: MIDIPortRef = 0;
        let port_name = cfstr(&format!("Futureboard listen ({device_name})"));
        if port_name.is_null() {
            continue;
        }
        let status = unsafe {
            MIDIInputPortCreate(
                client,
                port_name,
                Some(midi_read_proc),
                callback_ptr,
                &mut port,
            )
        };
        unsafe { CFRelease(port_name) };
        if status != 0 || port == 0 {
            eprintln!("[MIDI input] MIDIInputPortCreate failed for '{device_name}': {status}");
            continue;
        }

        let status = unsafe { MIDIPortConnectSource(port, source, ptr::null_mut()) };
        if status != 0 {
            eprintln!("[MIDI input] MIDIPortConnectSource failed for '{device_name}': {status}");
            unsafe {
                let _ = MIDIPortDispose(port);
            }
            continue;
        }

        if crate::midi_settings_debug_enabled() {
            eprintln!(
                "[MIDI input] CoreMIDI connected '{device_name}' ({device_id}) \
                 endpoint={source} unique_id={} port={port}",
                endpoint_int_property(source, K_MIDI_PROPERTY_UNIQUE_ID)
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "?".to_string())
            );
        }
        connections.push((
            device_id,
            MacMidiInputConnection {
                port,
                source,
                _callback: callback,
            },
        ));
    }
    connections
}

/// CoreMIDI output connection (client + port + destination).
pub struct MacMidiOutputConnection {
    client: MIDIClientRef,
    port: MIDIPortRef,
    dest: MIDIEndpointRef,
}

impl Drop for MacMidiOutputConnection {
    fn drop(&mut self) {
        unsafe {
            if self.client != 0 {
                let _ = MIDIClientDispose(self.client);
            }
        }
    }
}

impl MacMidiOutputConnection {
    pub fn send(&self, message: &[u8]) -> Result<(), OSStatus> {
        if message.is_empty() {
            return Err(-1);
        }
        // A bulk dump outgrows one packet. CoreMIDI accepts a SysEx split
        // across consecutive packets, so send it in packet-sized pieces
        // rather than refusing it.
        if message.len() > MAX_PACKET_BYTES {
            if message[0] != 0xF0 {
                return Err(-1);
            }
            for chunk in message.chunks(MAX_PACKET_BYTES) {
                self.send_packet(chunk)?;
            }
            return Ok(());
        }
        self.send_packet(message)
    }

    fn send_packet(&self, message: &[u8]) -> Result<(), OSStatus> {
        let mut buf = PacketListBuffer([0u8; MIDI_PACKET_LIST_BUF]);
        let pktlist = buf.0.as_mut_ptr() as *mut MIDIPacketList;
        unsafe {
            let mut cur = MIDIPacketListInit(pktlist);
            cur = MIDIPacketListAdd(
                pktlist,
                MIDI_PACKET_LIST_BUF as c_ulong,
                cur,
                0,
                message.len() as c_ulong,
                message.as_ptr(),
            );
            if cur.is_null() {
                return Err(-1);
            }
            let status = MIDISend(self.port, self.dest, pktlist);
            if status != 0 { Err(status) } else { Ok(()) }
        }
    }
}

pub fn open_output(device_id_or_name: &str) -> Option<MacMidiOutputConnection> {
    let (dest, name) = find_destination_by_name_or_id(device_id_or_name)?;
    let mut client: MIDIClientRef = 0;
    let client_name = cfstr("Futureboard MIDI playback");
    if client_name.is_null() {
        return None;
    }
    let status = unsafe { MIDIClientCreate(client_name, None, ptr::null_mut(), &mut client) };
    unsafe { CFRelease(client_name) };
    if status != 0 || client == 0 {
        eprintln!("[MIDI output] MIDIClientCreate failed for '{name}': {status}");
        return None;
    }
    let mut port: MIDIPortRef = 0;
    let port_name = cfstr("Futureboard MIDI Out");
    if port_name.is_null() {
        unsafe {
            let _ = MIDIClientDispose(client);
        }
        return None;
    }
    let status = unsafe { MIDIOutputPortCreate(client, port_name, &mut port) };
    unsafe { CFRelease(port_name) };
    if status != 0 || port == 0 {
        eprintln!("[MIDI output] MIDIOutputPortCreate failed for '{name}': {status}");
        unsafe {
            let _ = MIDIClientDispose(client);
        }
        return None;
    }
    Some(MacMidiOutputConnection { client, port, dest })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `MIDIPacketList` byte-for-byte the way CoreMIDI does.
    ///
    /// The offsets and the stride rule are written out as literals taken from
    /// `MIDIServices.h` rather than from this module's constants, so the test
    /// fails if those constants drift: `numPackets` at 0, the first packet at
    /// **4** (not 8 — `MIDIPacket` is `pack(4)`), `timeStamp` at +0, `length` at
    /// +8, `data` at +10, and each following packet 4-byte aligned on ARM or
    /// butted directly against the previous payload on Intel.
    fn build_packet_list(packets: &[&[u8]]) -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&(packets.len() as u32).to_ne_bytes());
        for payload in packets {
            assert_eq!(buffer.len() % 4, 0, "packets start 4-byte aligned");
            buffer.extend_from_slice(&0x1234_5678_9abc_def0u64.to_ne_bytes());
            buffer.extend_from_slice(&(payload.len() as u16).to_ne_bytes());
            buffer.extend_from_slice(payload);
            if cfg!(any(target_arch = "aarch64", target_arch = "arm")) {
                while buffer.len() % 4 != 0 {
                    buffer.push(0);
                }
            }
        }
        // Room for the reader to clamp a payload against without running off
        // the allocation, mirroring CoreMIDI's oversized packet buffers.
        buffer.resize(buffer.len() + PACKET_DATA_CAPACITY, 0);
        buffer
    }

    fn collect(packets: &[&[u8]]) -> Vec<Vec<u8>> {
        let buffer = build_packet_list(packets);
        let mut seen = Vec::new();
        // SAFETY: `buffer` is a well-formed packet list built above, and it
        // outlives the walk.
        unsafe {
            for_each_packet(buffer.as_ptr() as *const MIDIPacketList, |data| {
                seen.push(data.to_vec());
            });
        }
        seen
    }

    #[test]
    fn a_packet_list_is_walked_at_the_layout_core_midi_actually_uses() {
        // The regression: reading the first packet through a naturally aligned
        // `#[repr(C)]` struct put it at offset 8, so `length` landed on the
        // third data byte and every payload came back as noise.
        assert_eq!(collect(&[&[0x90, 60, 100]]), vec![vec![0x90, 60, 100]]);
    }

    #[test]
    fn every_packet_in_a_multi_packet_list_is_read() {
        // Wrong stride arithmetic only shows up from the second packet on, and
        // the correct rule differs between Apple Silicon and Intel.
        let seen = collect(&[&[0x90, 60, 100], &[0x80, 60, 0], &[0xB0, 7, 90]]);
        assert_eq!(
            seen,
            vec![vec![0x90, 60, 100], vec![0x80, 60, 0], vec![0xB0, 7, 90]]
        );
    }

    #[test]
    fn packets_of_uneven_length_keep_the_walk_aligned() {
        let seen = collect(&[&[0xF8], &[0x90, 60, 100], &[0xC0, 5], &[0x90, 64, 100]]);
        assert_eq!(
            seen,
            vec![
                vec![0xF8],
                vec![0x90, 60, 100],
                vec![0xC0, 5],
                vec![0x90, 64, 100]
            ]
        );
    }

    #[test]
    fn a_coalesced_packet_reaches_the_splitter_intact() {
        // CoreMIDI puts messages that share a timestamp in one packet; the
        // payload has to arrive whole for `split_midi_messages` to unpack it.
        let payload: &[u8] = &[0x90, 60, 100, 0x90, 64, 100, 0x90, 67, 100];
        assert_eq!(collect(&[payload]), vec![payload.to_vec()]);

        let mut running = None;
        let mut events = Vec::new();
        crate::split_midi_messages(payload, &mut running, |message| {
            if let Some(event) = decode_midi_bytes(message) {
                events.push(event);
            }
        });
        assert_eq!(events.len(), 3, "the whole chord must decode");
    }

    #[test]
    fn an_empty_packet_list_is_a_no_op() {
        assert!(collect(&[]).is_empty());
    }
}
