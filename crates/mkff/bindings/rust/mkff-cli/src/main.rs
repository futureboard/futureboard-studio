//! `mkfx`: an FFmpeg-style command-line media tool built on MKFF — probe, transcode and remux
//! with MKFF's own audio codecs, the mkff-codecs video codecs and the Media Kit's GPU
//! accelerants. See `mkfx -h`.

mod args;
mod info;
mod progress;
mod transcode;
mod units;

use std::process::ExitCode;

use args::{Level, Query};
use transcode::{Ctx, Input};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match args::parse(&argv) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let g = &cmd.global;
    let say = |s: &str| {
        if g.level >= Level::Info {
            eprint!("{s}");
        }
    };
    if g.banner || g.query == Some(Query::Version) {
        say(&info::banner());
    }
    if let Some(q) = g.query {
        print!(
            "{}",
            match q {
                Query::Version => String::new(),
                Query::Help => info::help(),
                Query::Codecs => info::codecs(),
                Query::Encoders => info::encoders(),
                Query::Decoders => info::decoders(),
                Query::Formats => info::formats(),
                Query::HwAccels => info::hwaccels(),
            }
        );
        return ExitCode::SUCCESS;
    }
    if cmd.inputs.is_empty() {
        if cmd.outputs.is_empty() {
            eprintln!("Universal media converter\nusage: mkfx [options] [[infile options] -i infile]... {{[outfile options] outfile}}...\n\nUse -h to get full help.");
        } else {
            eprintln!(
                "Output file '{}' has no input: specify one with -i",
                cmd.outputs[0].path
            );
        }
        return ExitCode::from(1);
    }
    let mut inputs = Vec::new();
    for (k, spec) in cmd.inputs.iter().enumerate() {
        match mkff_codecs::MediaFile::open(&spec.path) {
            Ok(media) => {
                say(&info::describe_input(k, &spec.path, &media));
                inputs.push(Input {
                    spec: spec.clone(),
                    media,
                });
            }
            Err(e) => {
                if g.level >= Level::Error {
                    eprintln!("{}: {e}", spec.path);
                }
                return ExitCode::from(1);
            }
        }
    }
    if cmd.outputs.is_empty() {
        // like FFmpeg: `-i file` alone describes the input
        if g.level >= Level::Error {
            eprintln!("At least one output file must be specified");
        }
        return ExitCode::from(1);
    }
    let ctx = Ctx { global: g };
    for (k, o) in cmd.outputs.iter().enumerate() {
        if let Err(e) = transcode::run_output(&ctx, k, o, &inputs) {
            if g.level >= Level::Error {
                eprintln!("{e}");
            }
            // don't leave a half-written file behind
            if !e.contains("already exists") {
                let _ = std::fs::remove_file(&o.path);
            }
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}
