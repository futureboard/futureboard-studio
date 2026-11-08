// Whether a build of `sphere_ui_components` may compile the TONE3000 partner
// key in. Shared verbatim by `build.rs` (as a `#[path]` module) and by the
// crate's unit tests (through `include!`), so the decision the build script
// makes is the one the tests exercise.
//
// A key compiled into a binary can be read back out of it, and release
// binaries (nightlies included) are handed out, so only a developer build may
// carry one. Every signal has to agree it is a developer build; anything else,
// including a missing or unexpected value, fails closed.

/// `cargo:rustc-env` name `build.rs` hands a developer build its key under.
///
/// Deliberately not `FUTUREBOARD_TONE3000_API_KEY`: `option_env!` reads
/// rustc's whole environment, so reading the user-facing name at compile time
/// would pick up whatever the build shell exported. `tone3000.rs` spells this
/// name out literally (`option_env!` needs a literal) behind
/// `cfg(all(debug_assertions, futureboard_tone3000_dev_key))`.
pub const DEV_KEY_RUSTC_ENV: &str = "FUTUREBOARD_TONE3000_DEV_BUILD_KEY";

/// `cargo:rustc-cfg` `build.rs` sets only when [`may_embed_dev_key`] allows.
pub const DEV_KEY_CFG: &str = "futureboard_tone3000_dev_key";

/// `profile` is Cargo's `PROFILE` ("debug" for dev/test and profiles inheriting
/// them, "release" otherwise), `opt_level` is `OPT_LEVEL`, and
/// `debug_assertions` is whether `CARGO_CFG_DEBUG_ASSERTIONS` is present.
///
/// All three are required because profiles can be customised: a release
/// profile can turn debug assertions on, and a dev-derived profile can be
/// optimised for distribution.
pub fn may_embed_dev_key(
    profile: Option<&str>,
    opt_level: Option<&str>,
    debug_assertions: bool,
) -> bool {
    debug_assertions && profile == Some("debug") && opt_level == Some("0")
}
