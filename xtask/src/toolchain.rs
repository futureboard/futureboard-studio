//! Bridge to the Professional Edition build toolchain.
//!
//! Provisioning the Steinberg ASIO SDK — the download URL, the licence
//! acceptance gate, the archive-layout validation, and the libclang lookup that
//! `asio-sys` needs for bindgen — is Professional Edition build tooling and
//! lives in the Git-ignored `crates/ExclusiveEdition/xtask/toolchain.rs`.
//!
//! `build.rs` stages that file when the private checkout is present and sets
//! `professional_toolchain`. A public Community checkout compiles the stub
//! below instead; `cargo xtask package` / `build-all` then build the package
//! without the Professional code and never call it.
//!
//! The bridge deliberately uses `include!` rather than a `#[path]` module, for
//! the same reason `apps/native/studio/src/professional_edition.rs` does:
//! rustfmt resolves `#[path]` modules even when they are cfg'd out, which would
//! make formatting the public workspace require the private source tree.

#[cfg(professional_toolchain)]
mod private {
    include!(concat!(
        env!("OUT_DIR"),
        "/futureboard-professional/toolchain.rs"
    ));
}

// The type is re-exported alongside the function so this module presents the
// same surface in both configurations. Callers only ever reach it through
// `prepare_professional(..)?.apply(..)`, so the name itself is never written —
// that is what the lint sees, not a sign the export is wrong.
#[cfg(professional_toolchain)]
#[allow(unused_imports)]
pub use private::{ProfessionalToolchain, prepare_professional};

#[cfg(not(professional_toolchain))]
#[allow(unused_imports)]
pub use stub::{ProfessionalToolchain, prepare_professional};

/// What a public checkout gets: no ASIO tooling, and a clear reason why.
#[cfg(not(professional_toolchain))]
mod stub {
    use std::path::Path;
    use std::process::Command;

    use anyhow::{Result, bail};

    /// Uninhabited: [`prepare_professional`] never returns one here, so the
    /// `apply` below is statically unreachable rather than a silent no-op that
    /// would let a Professional build run without `CPAL_ASIO_DIR`.
    pub enum ProfessionalToolchain {}

    impl ProfessionalToolchain {
        pub fn apply(&self, _command: &mut Command) {
            match *self {}
        }
    }

    pub fn prepare_professional(_workspace_root: &Path) -> Result<ProfessionalToolchain> {
        bail!(
            "this checkout has no Professional Edition build toolchain.\n\
             Building with ASIO needs the authorized `crates/ExclusiveEdition` \
             source, which provisions the Steinberg ASIO SDK and libclang.\n\
             Without it, `cargo xtask package` builds the package without the \
             Professional code."
        )
    }
}
