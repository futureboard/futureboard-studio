//! MIDI ports for the remote: inputs and outputs opened by name through
//! `midir` (WinMM on Windows, the ALSA sequencer on Linux), a port that went
//! away retried every 2 s, and what is wrong with each said in the status.
//!
//! An input's callback runs on midir's thread: it only decodes the bytes
//! into a fixed-size event and pushes it into the server's bounded inbound
//! queue ([`crate::remote::RemoteSender`]), never blocking.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::remote::{RemoteIn, RemoteSender};

/// How often missing ports are looked for again.
pub const RETRY: Duration = Duration::from_secs(2);
const RETRY_NO_SYSTEM: Duration = Duration::from_secs(30);
const CLIENT: &str = "LiveStage";

/// Every port the system offers now: `(inputs, outputs)`.
pub fn list() -> Result<(Vec<String>, Vec<String>), String> {
    let input = midir::MidiInput::new(CLIENT).map_err(|e| no_midi(&e))?;
    let output = midir::MidiOutput::new(CLIENT).map_err(|e| no_midi(&e))?;
    let inputs = input
        .ports()
        .iter()
        .filter_map(|p| input.port_name(p).ok())
        .collect();
    let outputs = output
        .ports()
        .iter()
        .filter_map(|p| output.port_name(p).ok())
        .collect();
    Ok((inputs, outputs))
}

fn no_midi(error: &dyn std::fmt::Display) -> String {
    format!("no MIDI system: {error}")
}

/// Whether the port the system calls `actual` is the one saved as `wanted`.
/// ALSA names end in the client:port numbers (`"nanoKONTROL2:nanoKONTROL2
/// _SLIDER/KNOB 24:0"`), which change when the device is plugged in again:
/// without them the names must match.
pub fn same_port(wanted: &str, actual: &str) -> bool {
    wanted == actual || strip_numbers(wanted) == strip_numbers(actual)
}

fn strip_numbers(name: &str) -> &str {
    match name.rsplit_once(' ') {
        Some((rest, tail))
            if tail.split_once(':').is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && a.bytes().all(|c| c.is_ascii_digit())
                    && b.bytes().all(|c| c.is_ascii_digit())
            }) =>
        {
            rest
        }
        _ => name,
    }
}

struct InPort {
    name: String,
    /// The connection, the id its events carry, and the name it was opened
    /// by (the numbers in it change when the device comes back).
    open: Option<(u64, String, midir::MidiInputConnection<()>)>,
    error: Option<String>,
}

struct OutPort {
    name: String,
    open: Option<(String, midir::MidiOutputConnection)>,
    error: Option<String>,
}

#[derive(Default)]
pub struct Midi {
    inputs: Vec<InPort>,
    outputs: Vec<OutPort>,
    next_id: u64,
    last_look: Option<Instant>,
    /// The MIDI system itself is missing (no ALSA sequencer).
    error: Option<String>,
}

impl Midi {
    /// Open these ports from now on (the ones already open by these names
    /// stay open).
    pub fn set_ports(
        &mut self,
        inputs: &[String],
        outputs: &[String],
        sender: Option<&RemoteSender>,
    ) {
        self.inputs.retain(|p| inputs.contains(&p.name));
        for name in inputs {
            if !self.inputs.iter().any(|p| &p.name == name) {
                self.inputs.push(InPort {
                    name: name.clone(),
                    open: None,
                    error: None,
                });
            }
        }
        self.outputs.retain(|p| outputs.contains(&p.name));
        for name in outputs {
            if !self.outputs.iter().any(|p| &p.name == name) {
                self.outputs.push(OutPort {
                    name: name.clone(),
                    open: None,
                    error: None,
                });
            }
        }
        if self.inputs.is_empty() && self.outputs.is_empty() {
            self.error = None;
        }
        self.look(Instant::now(), sender);
    }

    /// Every [`RETRY`]: close ports that went away, open the missing ones.
    /// Whether the status changed.
    pub fn tick(&mut self, now: Instant, sender: Option<&RemoteSender>) -> bool {
        if self.inputs.is_empty() && self.outputs.is_empty() {
            return false;
        }
        // No MIDI system at all (no ALSA sequencer): looked for again, but
        // not so often that its complaints fill the log.
        let period = if self.error.is_some() {
            RETRY_NO_SYSTEM
        } else {
            RETRY
        };
        if self
            .last_look
            .is_some_and(|last| now.saturating_duration_since(last) < period)
        {
            return false;
        }
        let before = self.status();
        self.look(now, sender);
        self.status() != before
    }

    fn look(&mut self, now: Instant, sender: Option<&RemoteSender>) {
        self.last_look = Some(now);
        if self.inputs.is_empty() && self.outputs.is_empty() {
            return;
        }
        let (inputs, outputs) = match list() {
            Ok(ports) => {
                self.error = None;
                ports
            }
            Err(error) => {
                for port in &mut self.inputs {
                    port.open = None;
                    port.error = Some(error.clone());
                }
                for port in &mut self.outputs {
                    port.open = None;
                    port.error = Some(error.clone());
                }
                self.error = Some(error);
                return;
            }
        };
        for port in &mut self.inputs {
            if let Some((_, opened, _)) = &port.open {
                if inputs.contains(opened) {
                    continue;
                }
                port.open = None;
            }
            match inputs.iter().find(|actual| same_port(&port.name, actual)) {
                None => port.error = Some("not connected".to_string()),
                Some(actual) => {
                    let Some(sender) = sender else {
                        port.error = Some("not listening (no command loop)".to_string());
                        continue;
                    };
                    let id = self.next_id;
                    self.next_id += 1;
                    match open_input(actual, id, sender.clone()) {
                        Ok(connection) => {
                            port.open = Some((id, actual.clone(), connection));
                            port.error = None;
                        }
                        Err(error) => port.error = Some(error),
                    }
                }
            }
        }
        for port in &mut self.outputs {
            if let Some((opened, _)) = &port.open {
                if outputs.contains(opened) {
                    continue;
                }
                port.open = None;
            }
            match outputs.iter().find(|actual| same_port(&port.name, actual)) {
                None => port.error = Some("not connected".to_string()),
                Some(actual) => match open_output(actual) {
                    Ok(connection) => {
                        port.open = Some((actual.clone(), connection));
                        port.error = None;
                    }
                    Err(error) => port.error = Some(error),
                },
            }
        }
    }

    /// Which saved input an event's connection id belongs to.
    pub fn input_name(&self, id: u64) -> Option<&str> {
        self.inputs
            .iter()
            .find(|p| p.open.as_ref().is_some_and(|(open, _, _)| *open == id))
            .map(|p| p.name.as_str())
    }

    pub fn has_outputs(&self) -> bool {
        self.outputs.iter().any(|p| p.open.is_some())
    }

    /// `bytes` to every open output. A port that refuses is closed and
    /// looked for again on the next retry.
    pub fn send(&mut self, bytes: &[u8]) {
        for port in &mut self.outputs {
            if let Some((_, connection)) = &mut port.open {
                if let Err(error) = connection.send(bytes) {
                    port.open = None;
                    port.error = Some(error.to_string());
                }
            }
        }
    }

    /// `{"inputs":[{"name","open","error"}],"outputs":[…],"error"}`.
    pub fn status(&self) -> Value {
        let port = |name: &str, open: bool, error: &Option<String>| json!({"name": name, "open": open, "error": error});
        json!({
            "inputs": self.inputs.iter().map(|p| port(&p.name, p.open.is_some(), &p.error)).collect::<Vec<_>>(),
            "outputs": self.outputs.iter().map(|p| port(&p.name, p.open.is_some(), &p.error)).collect::<Vec<_>>(),
            "error": self.error,
        })
    }
}

fn open_input(
    name: &str,
    id: u64,
    sender: RemoteSender,
) -> Result<midir::MidiInputConnection<()>, String> {
    let mut input = midir::MidiInput::new(CLIENT).map_err(|e| no_midi(&e))?;
    // Clock, active sensing and SysEx are never mapped.
    input.ignore(midir::Ignore::All);
    let port = input
        .ports()
        .into_iter()
        .find(|p| input.port_name(p).is_ok_and(|n| n == name))
        .ok_or("not connected")?;
    input
        .connect(
            &port,
            "livestage-remote-in",
            move |_stamp, bytes, _| {
                // Fixed size: nothing is allocated for a dropped event.
                let mut message = [0u8; 3];
                let len = bytes.len().min(3);
                message[..len].copy_from_slice(&bytes[..len]);
                sender.push(RemoteIn::Midi {
                    port: id,
                    message,
                    len: len as u8,
                });
            },
            (),
        )
        .map_err(|e| e.to_string())
}

fn open_output(name: &str) -> Result<midir::MidiOutputConnection, String> {
    let output = midir::MidiOutput::new(CLIENT).map_err(|e| no_midi(&e))?;
    let port = output
        .ports()
        .into_iter()
        .find(|p| output.port_name(p).is_ok_and(|n| n == name))
        .ok_or("not connected")?;
    output
        .connect(&port, "livestage-remote-out")
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alsa_port_numbers_do_not_matter_when_matching_names() {
        assert!(same_port(
            "Microsoft GS Wavetable Synth",
            "Microsoft GS Wavetable Synth"
        ));
        assert!(same_port(
            "nanoKONTROL2:nanoKONTROL2 _ CTRL 20:0",
            "nanoKONTROL2:nanoKONTROL2 _ CTRL 24:0"
        ));
        assert!(!same_port(
            "nanoKONTROL2:nanoKONTROL2 _ CTRL 20:0",
            "X-Touch:X-Touch MIDI 1 24:0"
        ));
        assert!(!same_port("Port 1", "Port 2"));
        assert_eq!(strip_numbers("A B 1:2"), "A B");
        assert_eq!(strip_numbers("A B:C"), "A B:C");
    }
}
