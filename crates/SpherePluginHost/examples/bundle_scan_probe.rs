//! Manual probe: discover plug-in bundles the way a Studio rescan does and run
//! the isolated scan on the ones whose path contains a filter, without writing
//! the catalog or any preset.
//!
//! ```text
//! cargo build -p sphere-plugin-host --bins
//! cargo run -p sphere-plugin-host --example bundle_scan_probe -- Roland
//! ```
//!
//! Each matching bundle is listed with the plug-ins the isolated scanner
//! reported for it (name, vendor, version, class id) or the failure, so a
//! plug-in missing from Studio's list can be traced to discovery or to the
//! scan itself.

use SpherePluginHost::discover_plugin_bundles;
use SpherePluginHost::registry::default_scan_paths;
use SpherePluginHost::scan::isolation::run_isolated_bundle_scan;

fn main() {
    let filter = std::env::args().nth(1).unwrap_or_default().to_lowercase();
    let roots = default_scan_paths();
    let bundles: Vec<_> = discover_plugin_bundles(&roots)
        .into_iter()
        .filter(|bundle| {
            filter.is_empty() || bundle.to_string_lossy().to_lowercase().contains(&filter)
        })
        .collect();
    println!("{} bundle(s) matching {filter:?}", bundles.len());
    let mut found = 0usize;
    for bundle in &bundles {
        println!("\n== {}", bundle.display());
        match run_isolated_bundle_scan(bundle) {
            Ok(outcome) => {
                for plugin in &outcome.plugins {
                    found += 1;
                    println!(
                        "  ok   {} | {} | {} | {}",
                        plugin.name,
                        plugin.vendor,
                        plugin.version.as_deref().unwrap_or("-"),
                        plugin.class_id.as_deref().unwrap_or("-"),
                    );
                }
                for failure in &outcome.failures {
                    println!("  FAIL {} — {}", failure.path, failure.error);
                }
                if outcome.plugins.is_empty() && outcome.failures.is_empty() {
                    println!("  (no plug-ins reported)");
                }
            }
            Err(error) => println!("  ERROR {error}"),
        }
    }
    println!("\n{found} plug-in(s) from {} bundle(s)", bundles.len());
}
