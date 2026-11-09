//! Each format through its exported entry point, the way a host drives it:
//! find the plug-in, instantiate it, prepare it at 48 kHz, and run a guitar-
//! like tone through the default rig. The debug build aborts on any
//! allocation in `process` (`assert_process_allocs`), so these also hold the
//! audio path to Studio's realtime rule.
//!
//! Unit tests rather than integration tests: the VST2 export also defines a
//! Windows `main` outside `cfg(test)`, which an integration-test harness's own
//! `main` would collide with at link time.

use std::ffi::{c_char, c_void, CStr};

use nih_plug::wrapper::vst2::AEffect;

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;
const BLOCKS: usize = 40;

/// A decaying 110 Hz tone with some harmonics, in both channels.
fn tone(block: usize, n: usize) -> f32 {
    let t = (block * BLOCK + n) as f32 / RATE as f32;
    let env = (-t * 1.5).exp();
    0.3 * env
        * ((2.0 * std::f32::consts::PI * 110.0 * t).sin()
            + 0.4 * (2.0 * std::f32::consts::PI * 220.0 * t).sin())
}

/// Runs `process` over `BLOCKS` blocks of [`tone`] and returns the output
/// energy, checking every sample is finite.
fn drive(mut process: impl FnMut(&mut [f32], &mut [f32], &mut [f32], &mut [f32])) -> f64 {
    let mut energy = 0.0f64;
    for block in 0..BLOCKS {
        let mut in_l: Vec<f32> = (0..BLOCK).map(|n| tone(block, n)).collect();
        let mut in_r = in_l.clone();
        let mut out_l = vec![0.0f32; BLOCK];
        let mut out_r = vec![0.0f32; BLOCK];
        process(&mut in_l, &mut in_r, &mut out_l, &mut out_r);
        for s in out_l.iter().chain(&out_r) {
            assert!(s.is_finite(), "non-finite output");
            energy += f64::from(*s) * f64::from(*s);
        }
    }
    energy
}

fn text(raw: &[i8]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn vst3_factory_instantiates_and_processes() {
    use nih_plug::wrapper::vst3::vst3_sys::base::{
        kResultOk, IPluginBase, IPluginFactory, PClassInfo,
    };
    use nih_plug::wrapper::vst3::vst3_sys::vst::{
        AudioBusBuffers, IAudioProcessor, IComponent, ProcessData, ProcessSetup,
    };
    use nih_plug::wrapper::vst3::vst3_sys::{ComInterface, VstPtr};

    unsafe {
        let factory = VstPtr::<dyn IPluginFactory>::owned(crate::GetPluginFactory() as *mut _)
            .expect("factory");
        assert_eq!(factory.count_classes(), 1);
        let mut info: PClassInfo = std::mem::zeroed();
        assert_eq!(factory.get_class_info(0, &mut info), kResultOk);
        assert_eq!(text(&info.name), "Rodhareist");
        assert_eq!(text(&info.category), "Audio Module Class");

        let mut obj: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            factory.create_instance(&info.cid, &<dyn IComponent as ComInterface>::IID, &mut obj),
            kResultOk
        );
        let component = VstPtr::<dyn IComponent>::owned(obj as *mut _).expect("component");
        assert_eq!(component.initialize(std::ptr::null_mut()), kResultOk);
        let processor = component
            .cast::<dyn IAudioProcessor>()
            .expect("IAudioProcessor");

        let setup = ProcessSetup {
            process_mode: 0,         // realtime
            symbolic_sample_size: 0, // 32-bit
            max_samples_per_block: BLOCK as i32,
            sample_rate: RATE,
        };
        assert_eq!(processor.setup_processing(&setup), kResultOk);
        assert_eq!(component.set_active(1), kResultOk);
        assert_eq!(processor.set_processing(1), kResultOk);

        let energy = drive(|in_l, in_r, out_l, out_r| {
            let mut ins = [
                in_l.as_mut_ptr() as *mut c_void,
                in_r.as_mut_ptr() as *mut c_void,
            ];
            let mut outs = [
                out_l.as_mut_ptr() as *mut c_void,
                out_r.as_mut_ptr() as *mut c_void,
            ];
            let mut input = AudioBusBuffers {
                num_channels: 2,
                silence_flags: 0,
                buffers: ins.as_mut_ptr(),
            };
            let mut output = AudioBusBuffers {
                num_channels: 2,
                silence_flags: 0,
                buffers: outs.as_mut_ptr(),
            };
            let mut data: ProcessData = std::mem::zeroed();
            data.process_mode = 0;
            data.symbolic_sample_size = 0;
            data.num_samples = BLOCK as i32;
            data.num_inputs = 1;
            data.num_outputs = 1;
            data.inputs = &mut input;
            data.outputs = &mut output;
            assert_eq!(processor.process(&mut data), kResultOk);
        });
        assert!(energy > 1e-3, "the rig is silent (energy {energy})");

        assert_eq!(processor.set_processing(0), kResultOk);
        assert_eq!(component.set_active(0), kResultOk);
        assert_eq!(component.terminate(), kResultOk);
    }
}

#[test]
fn clap_entry_instantiates_and_processes() {
    use clap_sys::audio_buffer::clap_audio_buffer;
    use clap_sys::events::{clap_event_header, clap_input_events, clap_output_events};
    use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
    use clap_sys::host::clap_host;
    use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};
    use clap_sys::version::CLAP_VERSION;

    unsafe extern "C" fn no_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
        std::ptr::null()
    }
    unsafe extern "C" fn no_request(_: *const clap_host) {}
    unsafe extern "C" fn no_events(_: *const clap_input_events) -> u32 {
        0
    }
    unsafe extern "C" fn no_event(_: *const clap_input_events, _: u32) -> *const clap_event_header {
        std::ptr::null()
    }
    unsafe extern "C" fn drop_event(
        _: *const clap_output_events,
        _: *const clap_event_header,
    ) -> bool {
        true
    }

    unsafe {
        let entry = &crate::clap_entry;
        assert!((entry.init.unwrap())(c"rodhareist-test".as_ptr()));
        let factory = (entry.get_factory.unwrap())(CLAP_PLUGIN_FACTORY_ID.as_ptr())
            as *const clap_plugin_factory;
        assert!(!factory.is_null());
        assert_eq!(((*factory).get_plugin_count.unwrap())(factory), 1);
        let desc = ((*factory).get_plugin_descriptor.unwrap())(factory, 0);
        let id = CStr::from_ptr((*desc).id);
        assert_eq!(id.to_str().unwrap(), "com.futureboard.rodhareist");
        assert_eq!(CStr::from_ptr((*desc).name).to_str().unwrap(), "Rodhareist");

        let host = clap_host {
            clap_version: CLAP_VERSION,
            host_data: std::ptr::null_mut(),
            name: c"rodhareist-test".as_ptr(),
            vendor: c"".as_ptr(),
            url: c"".as_ptr(),
            version: c"1".as_ptr(),
            get_extension: Some(no_extension),
            request_restart: Some(no_request),
            request_process: Some(no_request),
            request_callback: Some(no_request),
        };
        let plugin = ((*factory).create_plugin.unwrap())(factory, &host, (*desc).id);
        assert!(!plugin.is_null());
        assert!(((*plugin).init.unwrap())(plugin));
        assert!(((*plugin).activate.unwrap())(plugin, RATE, 1, BLOCK as u32));
        assert!(((*plugin).start_processing.unwrap())(plugin));

        let in_events = clap_input_events {
            ctx: std::ptr::null_mut(),
            size: Some(no_events),
            get: Some(no_event),
        };
        let out_events = clap_output_events {
            ctx: std::ptr::null_mut(),
            try_push: Some(drop_event),
        };
        let energy = drive(|in_l, in_r, out_l, out_r| {
            let mut ins = [in_l.as_mut_ptr(), in_r.as_mut_ptr()];
            let mut outs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
            let input = clap_audio_buffer {
                data32: ins.as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let mut output = clap_audio_buffer {
                data32: outs.as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let process = clap_process {
                steady_time: -1,
                frames_count: BLOCK as u32,
                transport: std::ptr::null(),
                audio_inputs: &input,
                audio_outputs: &mut output,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: &in_events,
                out_events: &out_events,
            };
            assert_ne!(
                ((*plugin).process.unwrap())(plugin, &process),
                CLAP_PROCESS_ERROR
            );
        });
        assert!(energy > 1e-3, "the rig is silent (energy {energy})");

        ((*plugin).stop_processing.unwrap())(plugin);
        ((*plugin).deactivate.unwrap())(plugin);
        ((*plugin).destroy.unwrap())(plugin);
        (entry.deinit.unwrap())();
    }
}

#[test]
fn vst2_main_instantiates_and_processes() {
    const MAGIC: i32 = 0x5673_7450; // 'VstP'
    const EFF_OPEN: i32 = 0;
    const EFF_CLOSE: i32 = 1;
    const EFF_SET_SAMPLE_RATE: i32 = 10;
    const EFF_SET_BLOCK_SIZE: i32 = 11;
    const EFF_MAINS_CHANGED: i32 = 12;
    const EFF_START_PROCESS: i32 = 71;
    const EFF_STOP_PROCESS: i32 = 72;
    const AUDIO_MASTER_VERSION: i32 = 1;

    unsafe extern "C" fn host(
        _effect: *mut AEffect,
        opcode: i32,
        _index: i32,
        _value: isize,
        _ptr: *mut c_void,
        _opt: f32,
    ) -> isize {
        if opcode == AUDIO_MASTER_VERSION {
            2400
        } else {
            0
        }
    }

    unsafe {
        let effect = crate::vst2_export::VSTPluginMain(host);
        assert!(!effect.is_null());
        let fx = &mut *effect;
        assert_eq!(fx.magic, MAGIC);
        assert_eq!(fx.unique_id, i32::from_be_bytes(*b"FbRh"));
        assert_eq!((fx.num_inputs, fx.num_outputs), (2, 2));
        // Every wire id but the clear-clip action.
        assert_eq!(fx.num_params as usize, rodharerist::UI_PARAM_IDS.len() - 1);

        let dispatch = fx.dispatcher;
        let call = |opcode: i32, value: isize, opt: f32| {
            dispatch(effect, opcode, 0, value, std::ptr::null_mut(), opt)
        };
        call(EFF_OPEN, 0, 0.0);
        call(EFF_SET_SAMPLE_RATE, 0, RATE as f32);
        call(EFF_SET_BLOCK_SIZE, BLOCK as isize, 0.0);
        call(EFF_MAINS_CHANGED, 1, 0.0);
        call(EFF_START_PROCESS, 0, 0.0);

        let process = fx.process_replacing;
        let energy = drive(|in_l, in_r, out_l, out_r| {
            let mut ins = [in_l.as_mut_ptr(), in_r.as_mut_ptr()];
            let mut outs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
            process(effect, ins.as_mut_ptr(), outs.as_mut_ptr(), BLOCK as i32);
        });
        assert!(energy > 1e-3, "the rig is silent (energy {energy})");

        call(EFF_STOP_PROCESS, 0, 0.0);
        call(EFF_MAINS_CHANGED, 0, 0.0);
        call(EFF_CLOSE, 0, 0.0);
    }
}

/// #343: a host caches parameter names. When a state or automation selects
/// another stomp, the plug-in renames that block's CC titles and tells the
/// host once (`audioMasterUpdateDisplay`); a knob move tells it nothing.
#[test]
fn vst2_tells_the_host_once_when_a_stomp_renames_its_titles() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    const EFF_OPEN: i32 = 0;
    const EFF_CLOSE: i32 = 1;
    const EFF_GET_PARAM_NAME: i32 = 8;
    const EFF_SET_SAMPLE_RATE: i32 = 10;
    const EFF_SET_BLOCK_SIZE: i32 = 11;
    const EFF_MAINS_CHANGED: i32 = 12;
    const AUDIO_MASTER_UPDATE_DISPLAY: i32 = 42;

    static UPDATE_DISPLAY: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn host(
        _effect: *mut AEffect,
        opcode: i32,
        _index: i32,
        _value: isize,
        _ptr: *mut c_void,
        _opt: f32,
    ) -> isize {
        if opcode == AUDIO_MASTER_UPDATE_DISPLAY {
            UPDATE_DISPLAY.fetch_add(1, Ordering::SeqCst);
        }
        0
    }

    let index_of = |id: &str| {
        rodharerist::UI_PARAM_IDS
            .iter()
            .filter(|known| **known != crate::params::CLEAR_CLIP)
            .position(|known| *known == id)
            .unwrap() as i32
    };
    let drive_model = index_of("drive_model");
    let drive_gain = index_of("drive_gain");
    let models = rodharerist::DriveModel::ALL;
    let rat = models
        .iter()
        .position(|m| *m == rodharerist::DriveModel::Rat)
        .unwrap();

    unsafe {
        let effect = crate::vst2_export::VSTPluginMain(host);
        assert!(!effect.is_null());
        let fx = &mut *effect;
        let dispatch = fx.dispatcher;
        let set_parameter = fx.set_parameter;
        let call = |opcode: i32, value: isize, opt: f32| {
            dispatch(effect, opcode, 0, value, std::ptr::null_mut(), opt)
        };
        // Suspend and resume: the host's way to have a VST2 take new values
        // into `initialize`, as a state load does.
        let reinitialize = || {
            call(EFF_MAINS_CHANGED, 0, 0.0);
            call(EFF_MAINS_CHANGED, 1, 0.0);
        };
        let name_of = |index: i32| {
            let mut buf = [0 as c_char; 64];
            dispatch(
                effect,
                EFF_GET_PARAM_NAME,
                index,
                0,
                buf.as_mut_ptr() as *mut c_void,
                0.0,
            );
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        };

        call(EFF_OPEN, 0, 0.0);
        call(EFF_SET_SAMPLE_RATE, 0, RATE as f32);
        call(EFF_SET_BLOCK_SIZE, BLOCK as isize, 0.0);
        call(EFF_MAINS_CHANGED, 1, 0.0);
        assert_eq!(UPDATE_DISPLAY.load(Ordering::SeqCst), 0, "defaults");
        assert!(name_of(drive_gain).starts_with("Green"));

        set_parameter(effect, drive_gain, 0.9);
        reinitialize();
        assert_eq!(UPDATE_DISPLAY.load(Ordering::SeqCst), 0, "a knob move");

        // Model steps sit on the fixed catalogue range, not the list's (#342).
        set_parameter(
            effect,
            drive_model,
            rat as f32 / (crate::params::MODEL_SLOTS - 1) as f32,
        );
        reinitialize();
        assert_eq!(UPDATE_DISPLAY.load(Ordering::SeqCst), 1, "a new stomp");
        assert!(
            name_of(drive_gain).starts_with("Rats"),
            "{}",
            name_of(drive_gain)
        );

        reinitialize();
        assert_eq!(UPDATE_DISPLAY.load(Ordering::SeqCst), 1, "nothing renamed");

        call(EFF_MAINS_CHANGED, 0, 0.0);
        call(EFF_CLOSE, 0, 0.0);
    }
}
