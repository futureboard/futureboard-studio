//! Manual probe: instantiate a real VST3 and print every parameter with its
//! normalized value and the plug-in's own text for it.
//!
//! Needs real plug-ins, so it is an example rather than a unit test:
//! `cargo run -p sphere_directaudioengine --example vst3_param_probe -- <bundle.vst3> [class-id] [filter]`

use DirectAudio::vst3_processor::Vst3RuntimeProcessor;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: vst3_param_probe <bundle.vst3> [class-id] [filter]");
        return;
    };
    let class_id = args.next().unwrap_or_default();
    let filter = args.next().unwrap_or_default().to_lowercase();
    let Some(plugin) = Vst3RuntimeProcessor::new(&path, &class_id, 48_000) else {
        println!("FAIL  processor could not be created for {path}");
        return;
    };
    let Some(parameters) = plugin.list_parameters() else {
        println!("FAIL  no parameter list");
        return;
    };
    println!("{} parameters", parameters.len());
    for parameter in parameters {
        if !filter.is_empty() && !parameter.title.to_lowercase().contains(&filter) {
            continue;
        }
        println!(
            "{:>10}  {:<40} {:<8} {:.6}  {:?}",
            parameter.id,
            parameter.title,
            parameter.unit,
            parameter.value_normalized,
            parameter.value_text
        );
    }
}
