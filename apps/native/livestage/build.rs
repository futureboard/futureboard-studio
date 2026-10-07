//! Embeds the web UI (`webui/dist`, built by Vite) into the server binary.
//!
//! A tree without a built page still compiles: the table comes out empty and
//! the server answers the browser with how to build it.

use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("webui/dist");
    let report = builtin_ui_embed::generate::generate(
        &builtin_ui_embed::generate::GenerateOptions::from_out_dir(dist, out_dir),
    )
    .expect("embed the LiveStage web UI");
    if !report.dist_present {
        println!(
            "cargo:warning=LiveStage web UI not built; the server will say so. \
             Build it with `bun run --cwd apps/native/livestage/webui build`."
        );
    }
}
