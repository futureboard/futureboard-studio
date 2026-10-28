//! VST2 wrapper. The ABI is the public C layout from VST 2.4; the Steinberg SDK is not linked.

mod context;
mod wrapper;

pub use wrapper::{AEffect, HostCallback, Wrapper};

/// Export a VST2 plugin from this library. The type must implement [`Vst2Plugin`][crate::prelude::Vst2Plugin].
///
/// This exports `VSTPluginMain`. On Windows it also exports `main`, which older hosts still look up.
#[macro_export]
macro_rules! nih_export_vst2 {
    ($plugin_ty:ty) => {
        #[doc(hidden)]
        mod vst2_export {
            use $crate::wrapper::setup_logger;
            use $crate::wrapper::vst2::{AEffect, HostCallback, Wrapper};

            use super::*;

            #[no_mangle]
            #[allow(non_snake_case, clippy::missing_safety_doc)]
            pub unsafe extern "C" fn VSTPluginMain(callback: HostCallback) -> *mut AEffect {
                setup_logger();
                Wrapper::<$plugin_ty>::create(callback)
            }

            // The integration-test harness also defines `main`. The cdylib build is the one hosts load.
            #[cfg(all(target_os = "windows", not(test)))]
            #[no_mangle]
            #[allow(non_snake_case, clippy::missing_safety_doc)]
            pub unsafe extern "C" fn main(callback: HostCallback) -> *mut AEffect {
                VSTPluginMain(callback)
            }
        }
    };
}
