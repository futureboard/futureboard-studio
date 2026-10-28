//! The VST3 face of the ARA patch, through the real exported factory and
//! COM interfaces — the calls an ARA host makes before any document exists.

use std::ffi::{c_void, CStr};

use ara2_bridge_sys::ARAFactory;
use nih_plug::wrapper::vst3::ara::{IMainFactory, IPlugInEntryPoint, IPlugInEntryPoint2};
use nih_plug::wrapper::vst3::vst3_sys::base::{kResultOk, IPluginFactory, PClassInfo};
use nih_plug::wrapper::vst3::vst3_sys::vst::IComponent;
use nih_plug::wrapper::vst3::vst3_sys::{ComInterface, VstPtr};

fn text(raw: &[i8]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

unsafe fn plugin_name(factory: *const c_void) -> String {
    assert!(!factory.is_null());
    let f = unsafe { std::ptr::read_unaligned(factory as *const ARAFactory) };
    unsafe { CStr::from_ptr(f.plugInName) }
        .to_string_lossy()
        .into_owned()
}

#[test]
fn the_factory_exports_the_plugin_and_its_ara_main_factory() {
    unsafe {
        let factory =
            VstPtr::<dyn IPluginFactory>::owned(voxtune_vst3::GetPluginFactory() as *mut _)
                .expect("factory");
        assert_eq!(factory.count_classes(), 2);

        let mut plugin: PClassInfo = std::mem::zeroed();
        let mut ara: PClassInfo = std::mem::zeroed();
        assert_eq!(factory.get_class_info(0, &mut plugin), kResultOk);
        assert_eq!(factory.get_class_info(1, &mut ara), kResultOk);
        assert_eq!(text(&plugin.category), "Audio Module Class");
        assert_eq!(text(&ara.category), "ARA Main Factory Class");
        // ARA pairs the two classes by name.
        assert_eq!(text(&plugin.name), "Solfege Voxtune");
        assert_eq!(text(&ara.name), "Solfege Voxtune");
        assert_ne!(plugin.cid.data, ara.cid.data);

        // The main factory hands out the ARAFactory.
        let mut obj: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            factory.create_instance(&ara.cid, &<dyn IMainFactory as ComInterface>::IID, &mut obj),
            kResultOk
        );
        let main = VstPtr::<dyn IMainFactory>::owned(obj as *mut _).expect("main factory");
        assert_eq!(plugin_name(main.get_factory()), "Solfege Voxtune");

        // The component is the ARA entry point too.
        let mut obj: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            factory.create_instance(
                &plugin.cid,
                &<dyn IComponent as ComInterface>::IID,
                &mut obj
            ),
            kResultOk
        );
        let component = VstPtr::<dyn IComponent>::owned(obj as *mut _).expect("component");
        let entry = component
            .cast::<dyn IPlugInEntryPoint>()
            .expect("IPlugInEntryPoint");
        assert_eq!(entry.get_factory(), main.get_factory());
        let entry2 = component
            .cast::<dyn IPlugInEntryPoint2>()
            .expect("IPlugInEntryPoint2");
        // No document controller: the plug-in refuses rather than crash.
        assert!(entry2
            .bind_to_document_controller_with_roles(std::ptr::null_mut(), 7, 7)
            .is_null());

        // An unknown class is still refused.
        let mut bogus = ara.cid;
        bogus.data[0] ^= 0xFF;
        let mut obj: *mut c_void = std::ptr::null_mut();
        assert_ne!(
            factory.create_instance(&bogus, &<dyn IComponent as ComInterface>::IID, &mut obj),
            kResultOk
        );
    }
}
