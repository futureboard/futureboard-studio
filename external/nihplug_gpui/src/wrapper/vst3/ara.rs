//! ARA 2 for VST3 plug-ins (the `ara` feature).
//!
//! ARA rides on two VST3 interfaces from `ARA_API/ARAVST3.h`:
//!
//! - the audio-effect component implements `ARA::IPlugInEntryPoint` and
//!   `ARA::IPlugInEntryPoint2`, through which the host fetches the plug-in's
//!   `ARAFactory` and binds this instance to a document controller;
//! - the factory exports one more class, category `"ARA Main Factory Class"`,
//!   whose object implements `ARA::IMainFactory` (the same `ARAFactory`), so a
//!   host can create a document controller before any plug-in instance.
//!
//! NIH-plug knows nothing about ARA's C API beyond pointers: the plug-in
//! supplies the `ARAFactory` ([`Vst3Plugin::ara_factory`]) and the binding
//! ([`Vst3Plugin::ara_bind_to_document_controller`]). Both are plain
//! pointers here, so this module needs no ARA headers or bindings.

use std::ffi::c_void;

use vst3_sys::base::{kInvalidArgument, kResultOk, tresult};
use vst3_sys::{com_interface, interfaces::iunknown::IUnknown};
use vst3_sys::{IID, VST3};

use super::factory::PluginInfo;
use crate::prelude::Vst3Plugin;

// Alias needed for the VST3 attribute macros.
use vst3_sys as vst3_com;

/// Category of the factory class that implements [`IMainFactory`].
pub const ARA_MAIN_FACTORY_CLASS: &str = "ARA Main Factory Class";

/// `ARA::IMainFactory` — `DECLARE_CLASS_IID (IMainFactory, 0xDB2A1669,
/// 0xFAFD42A5, 0xA82F864F, 0x7B6872EA)`.
#[com_interface("DB2A1669-FAFD-42A5-A82F-864F7B6872EA")]
pub trait IMainFactory: IUnknown {
    /// `const ARAFactory* getFactory ()`.
    unsafe fn get_factory(&self) -> *const c_void;
}

/// `ARA::IPlugInEntryPoint` — `DECLARE_CLASS_IID (IPlugInEntryPoint,
/// 0x12814E54, 0xA1CE4076, 0x82B96813, 0x16950BD6)`.
#[com_interface("12814E54-A1CE-4076-82B9-681316950BD6")]
pub trait IPlugInEntryPoint: IUnknown {
    /// `const ARAFactory* getFactory ()`.
    unsafe fn get_factory(&self) -> *const c_void;
    /// ARA 1 binding, deprecated since ARA 2: binds with every role.
    unsafe fn bind_to_document_controller(
        &self,
        document_controller_ref: *mut c_void,
    ) -> *const c_void;
}

/// `ARA::IPlugInEntryPoint2` — `DECLARE_CLASS_IID (IPlugInEntryPoint2,
/// 0xCD9A5913, 0xC9EB46D7, 0x96CA53AD, 0xD1DB89F5)`.
#[com_interface("CD9A5913-C9EB-46D7-96CA-53ADD1DB89F5")]
pub trait IPlugInEntryPoint2: IUnknown {
    /// `const ARAPlugInExtensionInstance* bindToDocumentControllerWithRoles
    /// (ARADocumentControllerRef, ARAPlugInInstanceRoleFlags knownRoles,
    /// ARAPlugInInstanceRoleFlags assignedRoles)`.
    unsafe fn bind_to_document_controller_with_roles(
        &self,
        document_controller_ref: *mut c_void,
        known_roles: i32,
        assigned_roles: i32,
    ) -> *const c_void;
}

/// Every ARA role (`kARAPlaybackRendererRole | kARAEditorRendererRole |
/// kARAEditorViewRole`), for the ARA 1 binding.
pub const ALL_ROLES: i32 = 1 | 2 | 4;

/// The object behind the "ARA Main Factory Class".
#[VST3(implements(IMainFactory))]
pub struct MainFactory {
    factory: *const c_void,
}

impl MainFactory {
    pub fn new(factory: *const c_void) -> Box<Self> {
        Self::allocate(factory)
    }
}

impl IMainFactory for MainFactory {
    unsafe fn get_factory(&self) -> *const c_void {
        self.factory
    }
}

/// The factory class entry for `P`'s ARA main factory, if it has one.
pub fn class_infos<P: Vst3Plugin>() -> Vec<PluginInfo> {
    match P::PLATFORM_VST3_ARA_FACTORY_CLASS_ID {
        Some(cid) if !P::ara_factory().is_null() => {
            let mut info = PluginInfo::for_plugin::<P>();
            // `PluginInfo` holds a `'static` class ID; 16 bytes per factory.
            info.cid = Box::leak(Box::new(cid));
            info.category = ARA_MAIN_FACTORY_CLASS;
            vec![info]
        }
        _ => Vec::new(),
    }
}

/// Creates the ARA main factory object if `cid` is `P`'s. None when it isn't.
///
/// # Safety
///
/// `cid`, `iid` and `obj` must be valid pointers, as for
/// `IPluginFactory::createInstance`.
pub unsafe fn create_instance<P: Vst3Plugin>(
    cid: *const IID,
    iid: *const IID,
    obj: *mut *mut c_void,
) -> Option<tresult> {
    let class_id = P::PLATFORM_VST3_ARA_FACTORY_CLASS_ID?;
    if (*cid).data != class_id {
        return None;
    }
    let factory = P::ara_factory();
    if factory.is_null() {
        return Some(kInvalidArgument);
    }
    let main_factory = MainFactory::new(factory);
    let result = main_factory.query_interface(iid, obj);
    if result == kResultOk {
        // Same ownership dance as the plug-in wrapper: `query_interface`
        // added the caller's reference, so drop ours and leak the box.
        main_factory.release();
        Box::leak(main_factory);
    }
    Some(result)
}
