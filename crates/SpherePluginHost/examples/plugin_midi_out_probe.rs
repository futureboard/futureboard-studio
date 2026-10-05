//! Load plug-ins in process and print the MIDI they produce.
//!
//! For each `.vst3` / `.clap` path (or folder of them): scans it for its class
//! id with `FutureboardPluginScanner`, creates it through the same native
//! bridge the plug-in host uses, plays a C major chord into it for a second
//! with the transport running, and prints every event its
//! `take_output_midi` hands back. The bridge also logs the plug-in's event
//! output bus count on stderr (`eventOutputBusCount`).
//!
//! ```text
//! cargo run -p sphere-plugin-host --example plugin_midi_out_probe -- <plugin>...
//! ```
//!
//! Needs `cargo build -p sphere-plugin-host --bins` for the scanner.

use std::path::{Path, PathBuf};

use DirectAudio::vst3_processor::{
    PluginMidiOutEvent, RuntimeTransportContext, Vst3MidiEvent, Vst3RuntimeProcessor,
};

const SAMPLE_RATE: u32 = 48_000;
const BLOCK: usize = 256;
/// About two seconds of blocks: notes on for the first half, off after.
const BLOCKS: usize = 375;
const CHORD: [u8; 3] = [60, 64, 67];

fn main() {
    let paths: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        eprintln!("usage: plugin_midi_out_probe <plugin.vst3|plugin.clap|folder>...");
        std::process::exit(2);
    }
    let plugins: Vec<PathBuf> = paths
        .into_iter()
        .flat_map(|path| {
            let is_bundle = path
                .extension()
                .is_some_and(|ext| ext == "vst3" || ext == "clap");
            if path.is_dir() && !is_bundle {
                let mut inside: Vec<PathBuf> = std::fs::read_dir(&path)
                    .map(|dir| {
                        dir.filter_map(Result::ok)
                            .map(|entry| entry.path())
                            .filter(|p| {
                                p.extension()
                                    .is_some_and(|ext| ext == "vst3" || ext == "clap")
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                inside.sort();
                inside
            } else {
                vec![path]
            }
        })
        .collect();

    let host =
        SpherePluginHost::plugin_host_client::locate_plugin_host_binary().unwrap_or_else(|error| {
            eprintln!("{error} — build it with `cargo build -p sphere-plugin-host --bins`");
            std::process::exit(2);
        });
    let scanner = host.with_file_name(if cfg!(windows) {
        "FutureboardPluginScanner.exe"
    } else {
        "FutureboardPluginScanner"
    });

    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    for plugin in &plugins {
        println!("\n=== {}", plugin.display());
        match probe(&scanner, plugin) {
            Ok(events) => {
                println!("  RESULT   {} MIDI event(s) out", events.len());
                for (block, event) in events.iter().take(24) {
                    println!(
                        "    block {block:>3} +{:>3}  {:02X} {:02X} {:02X}",
                        event.sample_offset, event.status, event.data1, event.data2
                    );
                }
            }
            Err(error) => println!("  RESULT   FAILED {error}"),
        }
    }
    // Plug-in modules can fault in their own DLL teardown; the answers are
    // already printed.
    SpherePluginHost::plugin_host_lifecycle::exit_now(0);
}

fn scan(scanner: &Path, plugin: &Path) -> Result<(String, String), String> {
    let format = if plugin.extension().is_some_and(|ext| ext == "clap") {
        "clap"
    } else {
        "vst3"
    };
    let output = std::process::Command::new(scanner)
        .args(["--format", format, "--json", "--path"])
        .arg(plugin)
        .output()
        .map_err(|error| format!("scanner did not run: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let payload = stdout
        .lines()
        .find_map(|line| line.strip_prefix("@@FUTUREBOARD_SCAN_PAYLOAD@@"))
        .ok_or_else(|| format!("scanner gave no payload (exit {:?})", output.status.code()))?;
    let json: serde_json::Value =
        serde_json::from_str(payload).map_err(|error| format!("bad payload: {error}"))?;
    let entry = json["plugins"]
        .as_array()
        .and_then(|plugins| plugins.first())
        .ok_or_else(|| format!("scanner found no class: {}", json["failures"]))?;
    Ok((
        entry["classId"].as_str().unwrap_or_default().to_string(),
        entry["name"].as_str().unwrap_or_default().to_string(),
    ))
}

fn probe(scanner: &Path, plugin: &Path) -> Result<Vec<(usize, PluginMidiOutEvent)>, String> {
    let (class_id, name) = scan(scanner, plugin)?;
    println!("  scanned  name={name} class={class_id}");
    let path = plugin.to_string_lossy().to_string();
    let mut processor = Vst3RuntimeProcessor::new(&path, &class_id, SAMPLE_RATE)
        .ok_or_else(|| "the bridge could not create it".to_string())?;
    println!("  event inputs={}", processor.event_input_bus_count());

    let silence = [0.0f32; BLOCK];
    let mut out = vec![0.0f32; BLOCK * 32];
    let mut midi_out = [PluginMidiOutEvent::default(); 256];
    let mut seen = Vec::new();
    let tempo = 120.0;
    for block in 0..BLOCKS {
        let samples = (block * BLOCK) as i64;
        let ppq = samples as f64 / SAMPLE_RATE as f64 * tempo / 60.0;
        processor.set_process_context(&RuntimeTransportContext {
            tempo_bpm: tempo,
            time_sig_num: 4,
            time_sig_den: 4,
            project_time_samples: samples,
            ppq_position: ppq,
            bar_position_ppq: RuntimeTransportContext::bar_start_ppq(ppq, 4, 4),
            playing: true,
            recording: false,
        });
        let events: Vec<Vst3MidiEvent> = if block == 0 {
            CHORD
                .iter()
                .map(|&note| Vst3MidiEvent::note_on(0, 0, note, 0.8))
                .collect()
        } else if block == BLOCKS / 2 {
            CHORD
                .iter()
                .map(|&note| Vst3MidiEvent::note_off(0, 0, note, 0.0))
                .collect()
        } else {
            Vec::new()
        };
        let _ =
            processor.process_main_output_block_with_midi(&silence, &silence, &mut out, 2, &events);
        let n = processor.take_output_midi(&mut midi_out);
        seen.extend(midi_out[..n].iter().map(|event| (block, *event)));
    }
    Ok(seen)
}
