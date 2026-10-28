use super::Plugin;
use crate::prelude::Vst3SubCategory;

/// Provides auxiliary metadata needed for a VST3 plugin.
pub trait Vst3Plugin: Plugin {
    /// The unique class ID that identifies this particular plugin. You can use the
    /// `*b"fooofooofooofooo"` syntax for this.
    ///
    /// This will be shuffled into a different byte order on Windows for project-compatibility.
    const VST3_CLASS_ID: [u8; 16];
    /// One or more subcategories. The host may use these to categorize the plugin. Internally this
    /// slice will be converted to a string where each character is separated by a pipe character
    /// (`|`). This string has a limit of 127 characters, and anything longer than that will be
    /// truncated.
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory];

    /// [`VST3_CLASS_ID`][Self::VST3_CLASS_ID`] in the correct order for the current platform so
    /// projects and presets can be shared between platforms. This should not be overridden.
    const PLATFORM_VST3_CLASS_ID: [u8; 16] = swap_vst3_uid_byte_order(Self::VST3_CLASS_ID);

    /// ARA 2: the class ID of the factory class that implements `ARA::IMainFactory`. `Some`
    /// (together with [`ara_factory()`][Self::ara_factory()] returning a factory) exports that
    /// class next to the plug-in and makes the plug-in instances answer `IPlugInEntryPoint` and
    /// `IPlugInEntryPoint2`. Needs the `ara` feature to have any effect.
    const VST3_ARA_FACTORY_CLASS_ID: Option<[u8; 16]> = None;

    /// [`VST3_ARA_FACTORY_CLASS_ID`][Self::VST3_ARA_FACTORY_CLASS_ID] in the current platform's
    /// byte order. This should not be overridden.
    const PLATFORM_VST3_ARA_FACTORY_CLASS_ID: Option<[u8; 16]> =
        match Self::VST3_ARA_FACTORY_CLASS_ID {
            Some(uid) => Some(swap_vst3_uid_byte_order(uid)),
            None => None,
        };

    /// ARA 2: this plug-in's `const ARAFactory*`, valid for the life of the library, or null when
    /// it doesn't support ARA. NIH-plug treats it as an opaque pointer.
    fn ara_factory() -> *const std::ffi::c_void {
        std::ptr::null()
    }

    /// ARA 2: binds this instance to the document controller `document_controller_ref` (an
    /// `ARADocumentControllerRef`) in the given `ARAPlugInInstanceRoleFlags`, returning its
    /// `const ARAPlugInExtensionInstance*` (null to refuse). Called on the host's main thread,
    /// once, before the plug-in is activated.
    fn ara_bind_to_document_controller(
        &mut self,
        document_controller_ref: *mut std::ffi::c_void,
        known_roles: i32,
        assigned_roles: i32,
    ) -> *const std::ffi::c_void {
        let _ = (document_controller_ref, known_roles, assigned_roles);
        std::ptr::null()
    }
}

#[cfg(not(target_os = "windows"))]
const fn swap_vst3_uid_byte_order(uid: [u8; 16]) -> [u8; 16] {
    uid
}

#[cfg(target_os = "windows")]
const fn swap_vst3_uid_byte_order(mut uid: [u8; 16]) -> [u8; 16] {
    // No mutable references in const functions, so we can't use `uid.swap()`
    let original_uid = uid;

    uid[0] = original_uid[3];
    uid[1] = original_uid[2];
    uid[2] = original_uid[1];
    uid[3] = original_uid[0];

    uid[4] = original_uid[5];
    uid[5] = original_uid[4];
    uid[6] = original_uid[7];
    uid[7] = original_uid[6];

    uid
}
