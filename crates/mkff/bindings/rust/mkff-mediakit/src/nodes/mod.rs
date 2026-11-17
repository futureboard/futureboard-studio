//! Ready-made nodes: a tone producer (the classic BeOS sample node), a decoder node that wraps
//! whatever decoder flavor the roster picks, and a callback consumer for applications and tests.

use std::time::Duration;

use crate::addon::{AudioDecoderPlugin, Kinds};
use crate::buffer::{Buffer, BufferGroup};
use crate::format::{MediaFormat, RawAudioFormat, SampleFormat};
use crate::node::{
    MediaDestination, MediaEvent, MediaInput, MediaNode, MediaOutput, MediaSource, NodeContext,
    RunState,
};
use crate::roster::{AccelPolicy, MediaRoster};
use crate::{Error, Result};

const TICK: u32 = 0x5449_434B; // 'TICK'

fn interleaved_f32(rate: u32, channels: u32, frames: u32) -> MediaFormat {
    MediaFormat::RawAudio(RawAudioFormat {
        frame_rate: rate,
        channels,
        format: SampleFormat::F32,
        buffer_frames: frames,
    })
}

/// Produces a sine tone as interleaved f32 buffers on output 0, on schedule.
pub struct ToneProducer {
    frequency: f32,
    gain: f32,
    format: RawAudioFormat,
    group: Option<BufferGroup>,
    connected: bool,
    phase: f64,
    /// Performance time of the next buffer.
    next: i64,
    /// Media time of the next buffer (µs).
    media: i64,
}

impl ToneProducer {
    pub fn new(frequency: f32, gain: f32) -> Self {
        Self {
            frequency,
            gain,
            format: RawAudioFormat {
                frame_rate: 48_000,
                channels: 2,
                format: SampleFormat::F32,
                buffer_frames: 1024,
            },
            group: None,
            connected: false,
            phase: 0.0,
            next: 0,
            media: 0,
        }
    }

    fn buffer_duration(&self) -> i64 {
        i64::from(self.format.buffer_frames) * 1_000_000 / i64::from(self.format.frame_rate.max(1))
    }

    fn make_buffer(&mut self) -> Result<Buffer> {
        let group = self
            .group
            .as_ref()
            .ok_or_else(|| Error::BadNode("not connected".into()))?;
        let (n, ch) = (
            self.format.buffer_frames as usize,
            self.format.channels as usize,
        );
        let mut b = group.request_buffer(n * ch * 4, Some(Duration::from_millis(100)))?;
        let step =
            std::f64::consts::TAU * f64::from(self.frequency) / f64::from(self.format.frame_rate);
        let mut samples = Vec::with_capacity(n * ch);
        for _ in 0..n {
            let s = (self.phase.sin() as f32) * self.gain;
            self.phase = (self.phase + step) % std::f64::consts::TAU;
            samples.extend(std::iter::repeat_n(s, ch));
        }
        b.write_f32(&samples)?;
        b.header.start_time = self.next;
        b.header.media_time = self.media;
        b.header.frames = n as u32;
        Ok(b)
    }
}

impl MediaNode for ToneProducer {
    fn name(&self) -> String {
        format!("tone {} Hz", self.frequency)
    }

    fn kinds(&self) -> Kinds {
        Kinds::BUFFER_PRODUCER
    }

    fn outputs(&self) -> Vec<MediaOutput> {
        vec![MediaOutput {
            source: MediaSource { node: 0, port: 0 },
            destination: None,
            format: MediaFormat::RawAudio(self.format),
            name: "tone out".into(),
        }]
    }

    fn format_proposal(&mut self, port: u32, request: &MediaFormat) -> Result<MediaFormat> {
        if port != 0 {
            return Err(Error::BadNode(format!("no output {port}")));
        }
        let mut f = request.clone();
        if !f.matches(&MediaFormat::RawAudio(RawAudioFormat {
            format: SampleFormat::F32,
            ..Default::default()
        })) {
            return Err(Error::BadFormat(format!(
                "tone produces f32 audio, not {request}"
            )));
        }
        f.specialize(&MediaFormat::RawAudio(self.format));
        Ok(f)
    }

    fn connect(
        &mut self,
        _port: u32,
        _dest: MediaDestination,
        format: &MediaFormat,
        _ctx: &mut NodeContext,
    ) -> Result<()> {
        let MediaFormat::RawAudio(a) = format else {
            return Err(Error::BadFormat(format.to_string()));
        };
        self.format = *a;
        let size = a.buffer_frames as usize * a.channels as usize * 4;
        self.group = Some(BufferGroup::new(size, 8)?);
        self.connected = true;
        Ok(())
    }

    fn disconnect_output(&mut self, _port: u32, _dest: MediaDestination, _ctx: &mut NodeContext) {
        self.connected = false;
        self.group = None;
    }

    fn handle_event(
        &mut self,
        event: MediaEvent,
        time: i64,
        _lateness: i64,
        ctx: &mut NodeContext,
    ) {
        match event {
            MediaEvent::Start => {
                self.next = if time == crate::IMMEDIATE {
                    ctx.now()
                } else {
                    time
                };
                let _ = ctx.post_self(
                    self.next - self.buffer_duration(),
                    MediaEvent::User {
                        what: TICK,
                        data: Box::new(()),
                    },
                );
            }
            MediaEvent::Seek { media_time } => self.media = media_time,
            MediaEvent::User { what: TICK, .. }
                if ctx.run_state() == RunState::Started && self.connected =>
            {
                if let Ok(b) = self.make_buffer() {
                    let _ = ctx.send_buffer(0, b);
                }
                let d = self.buffer_duration();
                self.next += d;
                self.media += d;
                let _ = ctx.post_self(
                    self.next - d,
                    MediaEvent::User {
                        what: TICK,
                        data: Box::new(()),
                    },
                );
            }
            _ => {}
        }
    }
}

/// What a [`CallbackConsumer`] calls for each buffer: the buffer, the connection's format and
/// how late (µs) it is being handled.
pub type BufferCallback = Box<dyn FnMut(&Buffer, &MediaFormat, i64) + Send>;

/// Consumes any format on input 0 and hands each buffer to a closure at its performance time.
pub struct CallbackConsumer {
    name: String,
    accept: MediaFormat,
    format: MediaFormat,
    callback: BufferCallback,
}

impl CallbackConsumer {
    /// `accept`: what input 0 takes ([`MediaFormat::Wildcard`] for anything).
    pub fn new(name: &str, accept: MediaFormat, callback: BufferCallback) -> Self {
        Self {
            name: name.to_owned(),
            format: accept.clone(),
            accept,
            callback,
        }
    }
}

impl MediaNode for CallbackConsumer {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn kinds(&self) -> Kinds {
        Kinds::BUFFER_CONSUMER
    }

    fn inputs(&self) -> Vec<MediaInput> {
        vec![MediaInput {
            destination: MediaDestination { node: 0, port: 0 },
            source: None,
            format: self.accept.clone(),
            name: format!("{} in", self.name),
        }]
    }

    fn accept_format(&mut self, port: u32, format: &MediaFormat) -> Result<MediaFormat> {
        if port != 0 || !self.accept.matches(format) {
            return Err(Error::BadFormat(format!(
                "{} does not take {format}",
                self.name
            )));
        }
        let mut f = format.clone();
        f.specialize(&self.accept);
        Ok(f)
    }

    fn connected(
        &mut self,
        _port: u32,
        _source: MediaSource,
        format: &MediaFormat,
        _ctx: &mut NodeContext,
    ) -> Result<()> {
        self.format = format.clone();
        Ok(())
    }

    fn handle_event(
        &mut self,
        event: MediaEvent,
        _time: i64,
        lateness: i64,
        _ctx: &mut NodeContext,
    ) {
        if let MediaEvent::Buffer { buffer, .. } = event {
            (self.callback)(&buffer, &self.format, lateness);
        }
    }
}

/// Decodes encoded audio buffers (one packet each, `media_time` = packet pts) from input 0 into
/// interleaved f32 buffers on output 0, with the decoder the roster picks for the stream.
pub struct AudioDecoderNode {
    roster: MediaRoster,
    policy: AccelPolicy,
    input: MediaFormat,
    decoder: Option<Box<dyn AudioDecoderPlugin>>,
    out: RawAudioFormat,
    out_connected: bool,
}

impl AudioDecoderNode {
    /// `input`: the encoded format to accept (its codec at least; the configuration arrives with
    /// the connection).
    pub fn new(roster: MediaRoster, input: MediaFormat, policy: AccelPolicy) -> Self {
        let (rate, ch) = match &input {
            MediaFormat::EncodedAudio(e) => (e.a, e.b),
            _ => (0, 0),
        };
        Self {
            roster,
            policy,
            input,
            decoder: None,
            out: RawAudioFormat {
                frame_rate: rate,
                channels: ch,
                format: SampleFormat::F32,
                buffer_frames: 0,
            },
            out_connected: false,
        }
    }
}

impl MediaNode for AudioDecoderNode {
    fn name(&self) -> String {
        format!(
            "{} decoder",
            self.input.codec().map_or("audio", |c| c.name())
        )
    }

    fn kinds(&self) -> Kinds {
        Kinds::BUFFER_CONSUMER | Kinds::BUFFER_PRODUCER
    }

    fn inputs(&self) -> Vec<MediaInput> {
        vec![MediaInput {
            destination: MediaDestination { node: 0, port: 0 },
            source: None,
            format: self.input.clone(),
            name: "encoded in".into(),
        }]
    }

    fn outputs(&self) -> Vec<MediaOutput> {
        vec![MediaOutput {
            source: MediaSource { node: 0, port: 0 },
            destination: None,
            format: MediaFormat::RawAudio(self.out),
            name: "decoded out".into(),
        }]
    }

    fn accept_format(&mut self, _port: u32, format: &MediaFormat) -> Result<MediaFormat> {
        if !self.input.matches(format) {
            return Err(Error::BadFormat(format!(
                "decoder for {} given {format}",
                self.input
            )));
        }
        let mut f = format.clone();
        f.specialize(&self.input);
        Ok(f)
    }

    fn connected(
        &mut self,
        _port: u32,
        _source: MediaSource,
        format: &MediaFormat,
        _ctx: &mut NodeContext,
    ) -> Result<()> {
        let (_, dec) = self.roster.audio_decoder(format, self.policy)?;
        let o = dec.output_format();
        if o.frame_rate != 0 {
            self.out.frame_rate = o.frame_rate;
        }
        if o.channels != 0 {
            self.out.channels = o.channels;
        }
        self.input = format.clone();
        self.decoder = Some(dec);
        Ok(())
    }

    fn format_proposal(&mut self, _port: u32, request: &MediaFormat) -> Result<MediaFormat> {
        let mut f = request.clone();
        f.specialize(&MediaFormat::RawAudio(self.out));
        Ok(f)
    }

    fn connect(
        &mut self,
        _port: u32,
        _dest: MediaDestination,
        _format: &MediaFormat,
        _ctx: &mut NodeContext,
    ) -> Result<()> {
        self.out_connected = true;
        Ok(())
    }

    fn disconnect_output(&mut self, _port: u32, _dest: MediaDestination, _ctx: &mut NodeContext) {
        self.out_connected = false;
    }

    fn handle_event(
        &mut self,
        event: MediaEvent,
        _time: i64,
        _lateness: i64,
        ctx: &mut NodeContext,
    ) {
        match event {
            MediaEvent::Buffer { buffer, .. } => {
                let Some(dec) = self.decoder.as_mut() else {
                    return;
                };
                let chunk = match dec.decode(buffer.payload(), buffer.header.media_time) {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("{}: {e}", self.input);
                        return;
                    }
                };
                if !self.out_connected || chunk.frames() == 0 {
                    return;
                }
                let n = chunk.frames();
                let mut inter = Vec::with_capacity(n * chunk.channels.len());
                for i in 0..n {
                    for c in &chunk.channels {
                        inter.push(c[i]);
                    }
                }
                let mut out = Buffer::from_vec(vec![0; inter.len() * 4]);
                if out.write_f32(&inter).is_ok() {
                    out.header.start_time = buffer.header.start_time;
                    out.header.media_time = chunk.pts;
                    out.header.frames = n as u32;
                    out.header.flags = buffer.header.flags;
                    let _ = ctx.send_buffer(0, out);
                }
            }
            MediaEvent::Seek { .. } => {
                if let Some(d) = self.decoder.as_mut() {
                    d.reset();
                }
            }
            _ => {}
        }
    }
}

/// An interleaved f32 raw audio format (helper for connecting the nodes above).
pub fn f32_audio(rate: u32, channels: u32, buffer_frames: u32) -> MediaFormat {
    interleaved_f32(rate, channels, buffer_frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn tone_graph_runs_on_schedule() {
        let roster = MediaRoster::new();
        let got: Arc<Mutex<Vec<(i64, usize, i64)>>> = Arc::default();
        let g = Arc::clone(&got);
        let tone = roster
            .register_node(Box::new(ToneProducer::new(440.0, 0.5)))
            .unwrap();
        let sink = roster
            .register_node(Box::new(CallbackConsumer::new(
                "sink",
                MediaFormat::raw_audio(0, 0, SampleFormat::F32),
                Box::new(move |b, _f, late| {
                    g.lock()
                        .unwrap()
                        .push((b.header.start_time, b.payload().len(), late))
                }),
            )))
            .unwrap();
        let c = roster.connect_nodes(tone.id, sink.id).unwrap();
        assert_eq!(c.format, f32_audio(48_000, 2, 1024));
        let start = roster
            .start_all(&[tone.id, sink.id], Duration::from_millis(20))
            .unwrap();
        std::thread::sleep(Duration::from_millis(250));
        roster.stop_node(tone.id, crate::IMMEDIATE).unwrap();
        let v = got.lock().unwrap().clone();
        // 1024 frames at 48 kHz = 21.333 ms per buffer: ~10 in 230 ms
        assert!(v.len() >= 6, "only {} buffers", v.len());
        assert_eq!(v[0].0, start);
        for w in v.windows(2) {
            assert_eq!(w[1].0 - w[0].0, 21_333);
        }
        assert!(v.iter().all(|x| x.1 == 1024 * 2 * 4));
        // consumers see buffers at (not long after) their performance time (loose for shared CI runners)
        assert!(v.iter().all(|x| x.2 < 50_000), "{v:?}");
        roster.release_node(tone.id).unwrap();
        roster.release_node(sink.id).unwrap();
        assert!(roster.connections().is_empty());
    }
}
