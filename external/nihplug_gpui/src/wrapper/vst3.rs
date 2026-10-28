#[macro_use]
mod util;

#[cfg(feature = "ara")]
pub mod ara;
mod context;
mod factory;
mod inner;
mod note_expressions;
mod param_units;
pub mod subcategories;
mod view;
mod wrapper;

/// Re-export for the wrapper.
pub use factory::PluginInfo;
pub use vst3_sys;
pub use wrapper::Wrapper;

/// Factory classes beyond the plug-in itself that `P` exports: its ARA main
/// factory when it has one (`ara` feature).
pub fn extra_class_infos<P: crate::prelude::Vst3Plugin>() -> Vec<PluginInfo> {
    #[cfg(feature = "ara")]
    {
        ara::class_infos::<P>()
    }
    #[cfg(not(feature = "ara"))]
    {
        Vec::new()
    }
}

/// Creates one of `P`'s extra factory classes if `cid` names one; None
/// otherwise.
///
/// # Safety
///
/// As `IPluginFactory::createInstance`: `cid`, `iid` and `obj` are valid.
pub unsafe fn create_extra_instance<P: crate::prelude::Vst3Plugin>(
    cid: *const vst3_sys::IID,
    iid: *const vst3_sys::IID,
    obj: *mut *mut vst3_sys::c_void,
) -> Option<vst3_sys::base::tresult> {
    #[cfg(feature = "ara")]
    {
        ara::create_instance::<P>(cid, iid, obj)
    }
    #[cfg(not(feature = "ara"))]
    {
        let _ = (cid, iid, obj);
        None
    }
}

/// Export one or more VST3 plugins from this library using the provided plugin types. The first
/// plugin's vendor information is used for the factory's information.
#[macro_export]
macro_rules! nih_export_vst3 {
    ($($plugin_ty:ty),+) => {
        // Earlier versions used a simple generic struct for this, but because we don't have
        // variadic generics (yet) we can't generate the struct for multiple plugin types without
        // macros. So instead we'll generate the implementation ad-hoc inside of this macro.
        #[doc(hidden)]
        mod vst3 {
            use ::std::collections::HashSet;

            // `vst3_sys` is imported from the VST3 wrapper module
            use $crate::wrapper::vst3::{vst3_sys, PluginInfo, Wrapper};
            use $crate::wrapper::vst3::{create_extra_instance, extra_class_infos};
            use vst3_sys::base::{kInvalidArgument, kResultOk, tresult};
            use vst3_sys::base::{
                FactoryFlags, IPluginFactory, IPluginFactory2, IPluginFactory3, IUnknown,
                PClassInfo, PClassInfo2, PClassInfoW, PFactoryInfo,
            };
            use vst3_sys::VST3;

            // This alias is needed for the VST3 attribute macro
            use vst3_sys as vst3_com;

            // Because the `$plugin_ty`s are likely defined in the enclosing scope. This works even
            // if the types are not public because this is a child module.
            use super::*;

            // Sneaky way to get the number of expanded elements
            const PLUGIN_COUNT: usize = [$(stringify!($plugin_ty)),+].len();

            #[doc(hidden)]
            #[VST3(implements(IPluginFactory, IPluginFactory2, IPluginFactory3))]
            pub struct Factory {
                // This is a type erased version of the information stored on the plugin types
                plugin_infos: [PluginInfo; PLUGIN_COUNT],
                // Further classes the plugins export (an ARA main factory), listed after the
                // plugins themselves
                extra_infos: Vec<PluginInfo>,
            }

            impl Factory {
                pub fn new() -> Box<Self> {
                    let plugin_infos = [$(PluginInfo::for_plugin::<$plugin_ty>()),+];

                    if cfg!(debug_assertions) {
                        let unique_cids: HashSet<[u8; 16]> = plugin_infos.iter().map(|d| *d.cid).collect();
                        $crate::nih_debug_assert_eq!(
                            unique_cids.len(),
                            plugin_infos.len(),
                            "Duplicate VST3 class IDs found in `nih_export_vst3!()` call"
                        );
                    }

                    let mut extra_infos = Vec::new();
                    $(extra_infos.extend(extra_class_infos::<$plugin_ty>());)+

                    Self::allocate(plugin_infos, extra_infos)
                }

                /// Class `index`: the plugins first, then the extra classes.
                fn class(&self, index: i32) -> Option<&PluginInfo> {
                    let index = usize::try_from(index).ok()?;
                    self.plugin_infos.get(index).or_else(|| {
                        self.extra_infos.get(index.checked_sub(self.plugin_infos.len())?)
                    })
                }
            }

            impl IPluginFactory for Factory {
                unsafe fn get_factory_info(&self, info: *mut PFactoryInfo) -> tresult {
                    if info.is_null() {
                        return kInvalidArgument;
                    }

                    // We'll use the first plugin's info for this
                    *info = self.plugin_infos[0].create_factory_info();

                    kResultOk
                }

                unsafe fn count_classes(&self) -> i32 {
                    (self.plugin_infos.len() + self.extra_infos.len()) as i32
                }

                unsafe fn get_class_info(&self, index: i32, info: *mut PClassInfo) -> tresult {
                    let Some(class) = self.class(index) else {
                        return kInvalidArgument;
                    };

                    *info = class.create_class_info();

                    kResultOk
                }

                unsafe fn create_instance(
                    &self,
                    cid: *const vst3_sys::IID,
                    iid: *const vst3_sys::IID,
                    obj: *mut *mut vst3_sys::c_void,
                ) -> tresult {
                    // Can't use `check_null_ptr!()` here without polluting NIH-plug's general
                    // exports
                    if cid.is_null() || obj.is_null() {
                        return kInvalidArgument;
                    }

                    // This is a poor man's way of treating `$plugin_ty` like an indexable array.
                    // Assuming `self.plugin_infos` is in the same order, we can simply check all of
                    // the registered plugin CIDs for matches using an unrolled loop.
                    let mut plugin_idx = 0;
                    $({
                        let plugin_info = &self.plugin_infos[plugin_idx];
                        if (*cid).data == *plugin_info.cid {
                            let wrapper = Wrapper::<$plugin_ty>::new();

                            // 99.999% of the times `iid` will be that of `IComponent`, but the
                            // caller is technically allowed to create an object for any support
                            // interface. We don't have a way to check whether our plugin supports
                            // the interface without creating it, but since the odds that a caller
                            // will create an object with an interface we don't support are
                            // basically zero this is not a problem.
                            let result = wrapper.query_interface(iid, obj);
                            if result == kResultOk {
                                // This is a bit awkward now but if the cast succeeds we need to get
                                // rid of the reference from the `wrapper` binding. The VST3 query
                                // interface always increments the reference count and returns an
                                // owned reference, so we need to explicitly release the reference
                                // from `wrapper` and leak the `Box` so the wrapper doesn't
                                // automatically get deallocated when this function returns (`Box`
                                // is an incorrect choice on vst3-sys' part, it should have used a
                                // `VstPtr` instead).
                                wrapper.release();
                                Box::leak(wrapper);

                                return kResultOk;
                            }
                        }

                        plugin_idx += 1;
                    })+

                    $(
                        if let Some(result) = create_extra_instance::<$plugin_ty>(cid, iid, obj) {
                            return result;
                        }
                    )+

                    kInvalidArgument
                }
            }

            impl IPluginFactory2 for Factory {
                unsafe fn get_class_info2(&self, index: i32, info: *mut PClassInfo2) -> tresult {
                    let Some(class) = self.class(index) else {
                        return kInvalidArgument;
                    };

                    *info = class.create_class_info_2();

                    kResultOk
                }
            }

            impl IPluginFactory3 for Factory {
                unsafe fn get_class_info_unicode(
                    &self,
                    index: i32,
                    info: *mut PClassInfoW,
                ) -> tresult {
                    let Some(class) = self.class(index) else {
                        return kInvalidArgument;
                    };

                    *info = class.create_class_info_unicode();

                    kResultOk
                }

                unsafe fn set_host_context(&self, _context: *mut vst3_sys::c_void) -> tresult {
                    // We don't need to do anything with this
                    kResultOk
                }
            }
        }

        /// The VST3 plugin factory entry point.
        #[no_mangle]
        pub extern "system" fn GetPluginFactory() -> *mut ::std::ffi::c_void {
            let factory = self::vst3::Factory::new();

            Box::into_raw(factory) as *mut ::std::ffi::c_void
        }

        // These two entry points are used on Linux, and they would theoretically also be used on
        // the BSDs:
        // https://github.com/steinbergmedia/vst3_public_sdk/blob/c3948deb407bdbff89de8fb6ab8500ea4df9d6d9/source/main/linuxmain.cpp#L47-L52
        #[allow(missing_docs)]
        #[no_mangle]
        #[cfg(all(target_family = "unix", not(target_os = "macos")))]
        pub extern "C" fn ModuleEntry(_lib_handle: *mut ::std::ffi::c_void) -> bool {
            $crate::wrapper::setup_logger();
            true
        }

        #[allow(missing_docs)]
        #[no_mangle]
        #[cfg(all(target_family = "unix", not(target_os = "macos")))]
        pub extern "C" fn ModuleExit() -> bool {
            true
        }

        // These two entry points are used on macOS:
        // https://github.com/steinbergmedia/vst3_public_sdk/blob/bc459feee68803346737901471441fd4829ec3f9/source/main/macmain.cpp#L60-L61
        #[allow(missing_docs)]
        #[no_mangle]
        #[cfg(target_os = "macos")]
        pub extern "C" fn bundleEntry(_lib_handle: *mut ::std::ffi::c_void) -> bool {
            $crate::wrapper::setup_logger();
            true
        }

        #[allow(missing_docs)]
        #[no_mangle]
        #[cfg(target_os = "macos")]
        pub extern "C" fn bundleExit() -> bool {
            true
        }

        // And these two entry points are used on Windows:
        // https://github.com/steinbergmedia/vst3_public_sdk/blob/bc459feee68803346737901471441fd4829ec3f9/source/main/dllmain.cpp#L59-L60
        #[allow(missing_docs)]
        #[no_mangle]
        #[cfg(target_os = "windows")]
        pub extern "system" fn InitDll() -> bool {
            $crate::wrapper::setup_logger();
            true
        }

        #[allow(missing_docs)]
        #[no_mangle]
        #[cfg(target_os = "windows")]
        pub extern "system" fn ExitDll() -> bool {
            true
        }
    };
}
