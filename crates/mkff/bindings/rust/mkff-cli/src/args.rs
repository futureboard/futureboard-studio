//! FFmpeg-style command line: `mkfx [global] {[input options] -i input}… {[output options] output}…`.
//!
//! Options apply to the next `-i` (input options) or the next output file name (output
//! options); global options (`-y`, `-hide_banner`, `-v`, …) may appear anywhere.

use crate::units::{parse_bitrate, parse_time};

/// Log verbosity (`-v` / `-loglevel`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Quiet,
    Error,
    Warning,
    Info,
    Verbose,
}

/// Something to list instead of converting (`-codecs`, `-hwaccels`, …).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Query {
    Version,
    Help,
    Codecs,
    Encoders,
    Decoders,
    Formats,
    HwAccels,
}

#[derive(Clone, Debug)]
pub struct Global {
    /// `Some(true)`: `-y`; `Some(false)`: `-n`; `None`: refuse to overwrite.
    pub overwrite: Option<bool>,
    pub banner: bool,
    pub level: Level,
    pub stats: bool,
    pub query: Option<Query>,
}

impl Default for Global {
    fn default() -> Self {
        Global {
            overwrite: None,
            banner: true,
            level: Level::Info,
            stats: true,
            query: None,
        }
    }
}

/// `-ss` / `-t` / `-to`, in seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Window {
    pub start: Option<f64>,
    pub duration: Option<f64>,
    pub end: Option<f64>,
}

impl Window {
    /// Length of the window starting at `start`, if limited.
    pub fn limit(&self, start: f64) -> Option<f64> {
        let by_to = self.end.map(|e| (e - start).max(0.0));
        match (self.duration, by_to) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

#[derive(Clone, Debug)]
pub struct InputSpec {
    pub path: String,
    pub window: Window,
    /// `-f` before `-i`: accepted for compatibility (formats are detected from content).
    #[allow(dead_code)]
    pub format: Option<String>,
}

/// Which input stream goes to the output (`-map`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapSpec {
    pub input: usize,
    /// `None`: both kinds.
    pub video: Option<bool>,
    /// Index among the input's streams of that kind.
    pub index: Option<usize>,
}

/// An audio filter (`-af`).
#[derive(Clone, Debug, PartialEq)]
pub enum AudioFilter {
    /// Linear gain.
    Volume(f32),
}

#[derive(Clone, Debug, Default)]
pub struct OutputSpec {
    pub path: String,
    pub format: Option<String>,
    pub acodec: Option<String>,
    pub vcodec: Option<String>,
    pub audio_bitrate: Option<u32>,
    pub video_bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub no_audio: bool,
    pub no_video: bool,
    pub window: Window,
    pub frame_rate: Option<(u32, u32)>,
    /// Target size; a negative dimension keeps the aspect ratio (`scale=1280:-2`).
    pub size: Option<(i32, i32)>,
    pub crf: Option<f32>,
    pub preset: Option<String>,
    pub gop: Option<u32>,
    pub bframes: Option<u8>,
    pub video_profile: Option<String>,
    pub compression_level: Option<u8>,
    pub audio_quality: Option<u8>,
    pub audio_filters: Vec<AudioFilter>,
    pub maps: Vec<MapSpec>,
    /// `-movflags frag_keyframe` / `+frag_keyframe`: fragmented MP4.
    pub fragmented: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Command {
    pub global: Global,
    pub inputs: Vec<InputSpec>,
    pub outputs: Vec<OutputSpec>,
}

fn level(s: &str) -> Result<Level, String> {
    // "level+flags" and numeric levels as FFmpeg accepts them
    let s = s.split('+').next().unwrap_or(s);
    Ok(match s {
        "quiet" | "-8" | "panic" | "0" | "fatal" | "8" => Level::Quiet,
        "error" | "16" => Level::Error,
        "warning" | "24" => Level::Warning,
        "info" | "32" => Level::Info,
        "verbose" | "40" | "debug" | "48" | "trace" | "56" => Level::Verbose,
        other => return Err(format!("invalid log level '{other}'")),
    })
}

fn rate(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("invalid frame rate '{s}'");
    match s {
        "ntsc" => return Ok((30_000, 1001)),
        "pal" => return Ok((25, 1)),
        "film" => return Ok((24, 1)),
        "ntsc-film" => return Ok((24_000, 1001)),
        _ => {}
    }
    if let Some((n, d)) = s.split_once('/') {
        let (n, d) = (
            n.parse::<u32>().map_err(|_| bad())?,
            d.parse::<u32>().map_err(|_| bad())?,
        );
        return if n > 0 && d > 0 {
            Ok((n, d))
        } else {
            Err(bad())
        };
    }
    let f: f64 = s.parse().map_err(|_| bad())?;
    if !(f > 0.0 && f <= 1000.0) {
        return Err(bad());
    }
    // integral rates exactly; others as /1000 (29.97 → 29970/1000)
    if (f - f.round()).abs() < 1e-9 {
        Ok((f.round() as u32, 1))
    } else {
        Ok(((f * 1000.0).round() as u32, 1000))
    }
}

fn size(s: &str) -> Result<(i32, i32), String> {
    let named = match s {
        "hd480" => Some((852, 480)),
        "hd720" => Some((1280, 720)),
        "hd1080" => Some((1920, 1080)),
        "2k" => Some((2048, 1080)),
        "4k" | "uhd2160" => Some((3840, 2160)),
        "vga" => Some((640, 480)),
        _ => None,
    };
    if let Some(n) = named {
        return Ok(n);
    }
    let (w, h) = s
        .split_once(['x', ':'])
        .ok_or_else(|| format!("invalid size '{s}'"))?;
    let p = |v: &str| {
        v.trim()
            .parse::<i32>()
            .map_err(|_| format!("invalid size '{s}'"))
    };
    let (w, h) = (p(w)?, p(h)?);
    if w == 0 || h == 0 || (w < 0 && h < 0) {
        return Err(format!("invalid size '{s}'"));
    }
    Ok((w, h))
}

fn audio_filters(s: &str) -> Result<Vec<AudioFilter>, String> {
    let mut v = Vec::new();
    for f in s.split(',').map(str::trim).filter(|f| !f.is_empty()) {
        let (name, arg) = f.split_once('=').unwrap_or((f, ""));
        match name {
            "volume" => {
                let arg = arg.strip_prefix("volume=").unwrap_or(arg);
                let gain =
                    if let Some(db) = arg.strip_suffix("dB").or_else(|| arg.strip_suffix("db")) {
                        let db: f32 = db
                            .trim()
                            .parse()
                            .map_err(|_| format!("invalid volume '{arg}'"))?;
                        10f32.powf(db / 20.0)
                    } else {
                        arg.trim()
                            .parse()
                            .map_err(|_| format!("invalid volume '{arg}'"))?
                    };
                v.push(AudioFilter::Volume(gain));
            }
            other => {
                return Err(format!(
                    "audio filter '{other}' is not supported (available: volume)"
                ))
            }
        }
    }
    Ok(v)
}

/// `scale=W:H` (or `scale=w=W:h=H`) from a `-vf` chain; other filters are refused.
fn video_filters(s: &str) -> Result<Option<(i32, i32)>, String> {
    let mut out = None;
    for f in s.split(',').map(str::trim).filter(|f| !f.is_empty()) {
        let (name, arg) = f.split_once('=').unwrap_or((f, ""));
        match name {
            "scale" => {
                let parts: Vec<&str> = arg.split(':').collect();
                let get = |i: usize, key: &str| -> Option<&str> {
                    parts
                        .iter()
                        .find_map(|p| p.strip_prefix(key))
                        .or_else(|| parts.get(i).copied().filter(|p| !p.contains('=')))
                };
                let w = get(0, "w=").ok_or("scale needs a width")?;
                let h = get(1, "h=").ok_or("scale needs a height")?;
                out = Some(size(&format!("{w}x{h}"))?);
            }
            other => {
                return Err(format!(
                    "video filter '{other}' is not supported (available: scale)"
                ))
            }
        }
    }
    Ok(out)
}

fn map(s: &str) -> Result<MapSpec, String> {
    let bad = || format!("invalid -map '{s}' (use IN, IN:a, IN:v, IN:a:N)");
    let mut it = s.split(':');
    let input = it.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let mut m = MapSpec {
        input,
        video: None,
        index: None,
    };
    if let Some(k) = it.next() {
        match k {
            "a" => m.video = Some(false),
            "v" | "V" => m.video = Some(true),
            n => {
                m.index = Some(n.parse().map_err(|_| bad())?);
            }
        }
    }
    if let Some(n) = it.next() {
        m.index = Some(n.parse().map_err(|_| bad())?);
    }
    Ok(m)
}

/// Parse `argv[1..]`.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut cmd = Command::default();
    let mut pending = OutputSpec::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        let mut value = || -> Result<&str, String> {
            let v = args
                .get(i)
                .map(String::as_str)
                .ok_or_else(|| format!("missing argument for option '{a}'"))?;
            i += 1;
            Ok(v)
        };
        if !a.starts_with('-') || a == "-" {
            // an output file
            pending.path = a.to_owned();
            cmd.outputs.push(std::mem::take(&mut pending));
            continue;
        }
        match a {
            // global
            "-y" => cmd.global.overwrite = Some(true),
            "-n" => cmd.global.overwrite = Some(false),
            "-hide_banner" => cmd.global.banner = false,
            "-v" | "-loglevel" => cmd.global.level = level(value()?)?,
            "-stats" => cmd.global.stats = true,
            "-nostats" => cmd.global.stats = false,
            "-nostdin" => {}
            "-version" => cmd.global.query = Some(Query::Version),
            "-h" | "-help" | "--help" | "-?" => cmd.global.query = Some(Query::Help),
            "-codecs" => cmd.global.query = Some(Query::Codecs),
            "-encoders" => cmd.global.query = Some(Query::Encoders),
            "-decoders" => cmd.global.query = Some(Query::Decoders),
            "-formats" | "-muxers" | "-demuxers" => cmd.global.query = Some(Query::Formats),
            "-hwaccels" => cmd.global.query = Some(Query::HwAccels),
            // input
            "-i" => {
                let path = value()?.to_owned();
                // -ss / -t / -to / -f given since the last file belong to this input
                let window = std::mem::take(&mut pending.window);
                let format = pending.format.take();
                cmd.inputs.push(InputSpec {
                    path,
                    window,
                    format,
                });
            }
            // input options before -i, output options otherwise
            "-ss" => pending.window.start = Some(parse_time(value()?)?),
            "-t" => pending.window.duration = Some(parse_time(value()?)?),
            "-to" => pending.window.end = Some(parse_time(value()?)?),
            "-f" => pending.format = Some(value()?.to_owned()),
            // output: codecs
            "-c" | "-codec" => {
                let c = value()?.to_owned();
                pending.acodec = Some(c.clone());
                pending.vcodec = Some(c);
            }
            "-c:a" | "-codec:a" | "-acodec" => pending.acodec = Some(value()?.to_owned()),
            "-c:v" | "-codec:v" | "-vcodec" => pending.vcodec = Some(value()?.to_owned()),
            "-b:a" | "-ab" => pending.audio_bitrate = Some(parse_bitrate(value()?)?),
            "-b:v" | "-vb" | "-b" => pending.video_bitrate = Some(parse_bitrate(value()?)?),
            "-ar" | "-ar:a" => {
                let v = value()?;
                pending.sample_rate = Some(
                    v.parse()
                        .ok()
                        .filter(|r: &u32| (1000..=768_000).contains(r))
                        .ok_or_else(|| format!("invalid sample rate '{v}'"))?,
                );
            }
            "-ac" | "-ac:a" => {
                let v = value()?;
                pending.channels = Some(
                    v.parse()
                        .ok()
                        .filter(|c: &u32| (1..=8).contains(c))
                        .ok_or_else(|| format!("invalid channel count '{v}' (1-8)"))?,
                );
            }
            "-an" => pending.no_audio = true,
            "-vn" => pending.no_video = true,
            "-sn" | "-dn" => {}
            "-r" | "-r:v" => pending.frame_rate = Some(rate(value()?)?),
            "-s" | "-s:v" => pending.size = Some(size(value()?)?),
            "-vf" | "-filter:v" => {
                if let Some(s) = video_filters(value()?)? {
                    pending.size = Some(s);
                }
            }
            "-af" | "-filter:a" => pending.audio_filters.extend(audio_filters(value()?)?),
            "-crf" => {
                let v = value()?;
                pending.crf = Some(
                    v.parse()
                        .ok()
                        .filter(|q: &f32| (0.0..=51.0).contains(q))
                        .ok_or_else(|| format!("invalid crf '{v}' (0-51)"))?,
                );
            }
            "-preset" => pending.preset = Some(value()?.to_owned()),
            "-g" => pending.gop = Some(value()?.parse().map_err(|_| "invalid -g")?),
            "-bf" => pending.bframes = Some(value()?.parse().map_err(|_| "invalid -bf")?),
            "-profile:v" | "-profile" => pending.video_profile = Some(value()?.to_owned()),
            "-compression_level" => {
                pending.compression_level =
                    Some(value()?.parse().map_err(|_| "invalid -compression_level")?)
            }
            "-q:a" | "-aq" => {
                pending.audio_quality = Some(value()?.parse().map_err(|_| "invalid -q:a (0-9)")?)
            }
            "-map" => pending.maps.push(map(value()?)?),
            "-movflags" => {
                let v = value()?;
                if v.contains("frag_keyframe") {
                    pending.fragmented = true;
                }
            }
            "-threads" | "-metadata" | "-map_metadata" | "-pix_fmt" | "-strict" => {
                // accepted for script compatibility; MKFF picks these itself
                value()?;
            }
            other => {
                return Err(format!(
                    "Unrecognized option '{}'.",
                    other.trim_start_matches('-')
                ))
            }
        }
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Command {
        parse(&s.split_whitespace().map(str::to_owned).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn ffmpeg_style() {
        let c = p(
            "-y -hide_banner -ss 1.5 -i in.flac -c:a mp3 -b:a 192k -ar 44100 -ac 2 -t 10 out.mp3",
        );
        assert_eq!(c.global.overwrite, Some(true));
        assert!(!c.global.banner);
        assert_eq!(c.inputs.len(), 1);
        assert_eq!(c.inputs[0].window.start, Some(1.5));
        let o = &c.outputs[0];
        assert_eq!(
            o.window.start, None,
            "input -ss must not also seek the output"
        );
        assert_eq!(o.path, "out.mp3");
        assert_eq!(o.acodec.as_deref(), Some("mp3"));
        assert_eq!(o.audio_bitrate, Some(192_000));
        assert_eq!((o.sample_rate, o.channels), (Some(44100), Some(2)));
        assert_eq!(o.window.duration, Some(10.0));
    }

    #[test]
    fn filters_maps_and_rates() {
        let c = p("-i a.mov -map 0:v -map 0:a:1 -vf scale=1280:-2 -r 29.97 -af volume=-6dB -c:v h264_hw out.mp4");
        let o = &c.outputs[0];
        assert_eq!(o.size, Some((1280, -2)));
        assert_eq!(o.frame_rate, Some((29_970, 1000)));
        assert_eq!(
            o.maps,
            vec![
                MapSpec {
                    input: 0,
                    video: Some(true),
                    index: None
                },
                MapSpec {
                    input: 0,
                    video: Some(false),
                    index: Some(1)
                }
            ]
        );
        let AudioFilter::Volume(g) = o.audio_filters[0];
        assert!((g - 0.501).abs() < 0.001);
    }

    #[test]
    fn errors() {
        let e = |s: &str| {
            parse(&s.split_whitespace().map(str::to_owned).collect::<Vec<_>>()).unwrap_err()
        };
        assert!(e("-i").contains("missing argument"));
        assert!(e("-frobnicate 1").contains("Unrecognized option"));
        assert!(e("-i a -ac 99 b.wav").contains("channel"));
        assert!(e("-i a -af echo b.wav").contains("not supported"));
    }
}
