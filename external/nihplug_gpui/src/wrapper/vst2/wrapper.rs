//! The `AEffect` instance hosts call into.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::os::raw::c_char;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use atomic_float::AtomicF32;
use crossbeam::atomic::AtomicCell;
use crossbeam::queue::ArrayQueue;
use parking_lot::Mutex;

use crate::plugin::vst2::Vst2Plugin;
use crate::prelude::{
    AsyncExecutor, AudioIOLayout, AuxiliaryBuffers, BufferConfig, Editor, ParamFlags, ParamPtr,
    Params, ParentWindowHandle, PluginState, ProcessMode, ProcessStatus,
};
use crate::wrapper::state::{self, PluginState as StateObject};
use crate::wrapper::util::buffer_management::{BufferManager, ChannelPointers};
use crate::wrapper::util::{hash_param_id, process_wrapper};

use super::context::{WrapperGuiContext, WrapperInitContext, WrapperProcessContext};

const VST_MAGIC: i32 = 0x56737450;
const VST_VERSION_2_4: i32 = 2400;
const CAN_REPLACING: i32 = 1 << 4;
const PROGRAM_CHUNKS: i32 = 1 << 5;
// Steinberg `effFlagsHasEditor`. Bit 0, not bit 16.
const HAS_EDITOR: i32 = 1 << 0;
const PARAM_STR_LEN: usize = 8;
const PROGRAM_NAME_LEN: usize = 24;
const EFFECT_NAME_LEN: usize = 32;
const STRING_LEN: usize = 64;
const TASK_QUEUE_CAPACITY: usize = 64;

const EFF_OPEN: i32 = 0;
const EFF_CLOSE: i32 = 1;
const EFF_SET_PROGRAM: i32 = 2;
const EFF_GET_PROGRAM: i32 = 3;
const EFF_GET_PROGRAM_NAME: i32 = 5;
const EFF_GET_PARAM_LABEL: i32 = 6;
const EFF_GET_PARAM_DISPLAY: i32 = 7;
const EFF_GET_PARAM_NAME: i32 = 8;
const EFF_SET_SAMPLE_RATE: i32 = 10;
const EFF_SET_BLOCK_SIZE: i32 = 11;
const EFF_MAINS_CHANGED: i32 = 12;
const EFF_EDIT_GET_RECT: i32 = 13;
const EFF_EDIT_OPEN: i32 = 14;
const EFF_EDIT_CLOSE: i32 = 15;
const EFF_EDIT_IDLE: i32 = 19;
const EFF_GET_CHUNK: i32 = 23;
const EFF_SET_CHUNK: i32 = 24;
const EFF_CAN_BE_AUTOMATED: i32 = 26;
const EFF_STRING_2_PARAMETER: i32 = 27;
const EFF_GET_PROGRAM_NAME_INDEXED: i32 = 29;
const EFF_GET_PLUG_CATEGORY: i32 = 35;
const EFF_SET_SPEAKER_ARRANGEMENT: i32 = 42;
const EFF_SET_BLOCK_SIZE_AND_SAMPLE_RATE: i32 = 43;
const EFF_SET_BYPASS: i32 = 44;
const EFF_GET_EFFECT_NAME: i32 = 45;
const EFF_GET_VENDOR_STRING: i32 = 47;
const EFF_GET_PRODUCT_STRING: i32 = 48;
const EFF_GET_VENDOR_VERSION: i32 = 49;
const EFF_CAN_DO: i32 = 51;
const EFF_GET_TAIL_SIZE: i32 = 52;
const EFF_IDLE: i32 = 53;
const EFF_GET_VST_VERSION: i32 = 58;
const EFF_START_PROCESS: i32 = 71;
const EFF_STOP_PROCESS: i32 = 72;

const AUDIO_MASTER_AUTOMATE: i32 = 0;
const AUDIO_MASTER_GET_TIME: i32 = 7;
const AUDIO_MASTER_IO_CHANGED: i32 = 13;
const AUDIO_MASTER_SIZE_WINDOW: i32 = 15;
const AUDIO_MASTER_BEGIN_EDIT: i32 = 43;
const AUDIO_MASTER_END_EDIT: i32 = 44;

const TRANSPORT_PLAYING: i32 = 1 << 1;
const TRANSPORT_CYCLE_ACTIVE: i32 = 1 << 2;
const TRANSPORT_RECORDING: i32 = 1 << 3;
const PPQ_POS_VALID: i32 = 1 << 9;
const TEMPO_VALID: i32 = 1 << 10;
const BARS_VALID: i32 = 1 << 11;
const CYCLE_POS_VALID: i32 = 1 << 12;
const TIME_SIG_VALID: i32 = 1 << 13;
const PLUG_CATEG_EFFECT: i32 = 1;

/// Host callback. `extern "C"` matches the VST2 cdecl calling convention.
pub type HostCallback = unsafe extern "C" fn(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize;

pub type ProcessFn = unsafe extern "C" fn(
    effect: *mut AEffect,
    inputs: *mut *mut f32,
    outputs: *mut *mut f32,
    sample_frames: i32,
);

pub type ProcessDoubleFn = unsafe extern "C" fn(
    effect: *mut AEffect,
    inputs: *mut *mut f64,
    outputs: *mut *mut f64,
    sample_frames: i32,
);

/// The VST 2.4 `AEffect` struct. Field order and `repr(C)` match the host ABI.
#[repr(C)]
pub struct AEffect {
    pub magic: i32,
    pub dispatcher: HostCallback,
    pub process: ProcessFn,
    pub set_parameter: unsafe extern "C" fn(*mut AEffect, i32, f32),
    pub get_parameter: unsafe extern "C" fn(*mut AEffect, i32) -> f32,
    pub num_programs: i32,
    pub num_params: i32,
    pub num_inputs: i32,
    pub num_outputs: i32,
    pub flags: i32,
    pub reserved_1: isize,
    pub reserved_2: isize,
    pub initial_delay: i32,
    pub real_qualities: i32,
    pub off_qualities: i32,
    pub io_ratio: f32,
    pub object: *mut c_void,
    pub user: *mut c_void,
    pub unique_id: i32,
    pub version: i32,
    pub process_replacing: ProcessFn,
    pub process_double_replacing: ProcessDoubleFn,
    pub future: [u8; 56],
}

#[repr(C)]
struct ERect {
    top: i16,
    left: i16,
    bottom: i16,
    right: i16,
}

#[repr(C)]
struct SpeakerArrangement {
    kind: i32,
    num_channels: i32,
}

#[repr(C)]
struct VstTimeInfo {
    sample_pos: f64,
    sample_rate: f64,
    nano_seconds: f64,
    ppq_pos: f64,
    tempo: f64,
    bar_start_pos: f64,
    cycle_start_pos: f64,
    cycle_end_pos: f64,
    time_sig_numerator: i32,
    time_sig_denominator: i32,
    smpte_offset: i32,
    smpte_frame_rate: i32,
    samples_to_next_clock: i32,
    flags: i32,
}

/// `effect` is the first field so a host `*mut AEffect` is also a `*mut Wrapper<P>`.
#[repr(C)]
pub struct Wrapper<P: Vst2Plugin> {
    effect: AEffect,
    pub(super) inner: Inner<P>,
}

pub(super) struct Inner<P: Vst2Plugin> {
    pub(super) plugin: Mutex<P>,
    pub(super) task_executor: Mutex<crate::prelude::TaskExecutor<P>>,
    params: Arc<dyn Params>,
    param_ptrs: Vec<ParamPtr>,
    param_by_hash: HashMap<u32, ParamPtr>,
    param_id_to_hash: HashMap<String, u32>,
    param_ptr_to_index: HashMap<ParamPtr, i32>,
    bypass_index: Option<usize>,
    host: HostCallback,
    editor: Mutex<Option<Box<dyn Editor>>>,
    editor_handle: Mutex<Option<Box<dyn std::any::Any + Send>>>,
    scale: AtomicF32,
    rect_bits: AtomicU64,
    rect_slot: UnsafeCell<ERect>,
    buffer_config: AtomicCell<Option<BufferConfig>>,
    layout: AtomicCell<AudioIOLayout>,
    buffer_manager: Mutex<BufferManager>,
    sample_rate: AtomicF32,
    max_block: AtomicU32,
    latency: AtomicU32,
    latency_dirty: AtomicBool,
    flushing: AtomicBool,
    tail: AtomicI32,
    active: AtomicBool,
    initialized: AtomicBool,
    closed: AtomicBool,
    chunk: Mutex<Vec<u8>>,
    tasks: ArrayQueue<P::BackgroundTask>,
}

// `rect_slot` is written only on the host thread inside `effEditGetRect`. Every other field is
// synchronized with atomics or mutexes.
unsafe impl<P: Vst2Plugin> Send for Wrapper<P> {}
unsafe impl<P: Vst2Plugin> Sync for Wrapper<P> {}

/// Raw instance pointer shared with callbacks. The host keeps the instance alive for the editor's
/// lifetime, and the editor is dropped before the instance.
#[derive(Clone, Copy)]
struct WrapperPtr<P: Vst2Plugin>(*const Wrapper<P>);

unsafe impl<P: Vst2Plugin> Send for WrapperPtr<P> {}
unsafe impl<P: Vst2Plugin> Sync for WrapperPtr<P> {}

impl<P: Vst2Plugin> WrapperPtr<P> {
    fn enqueue(&self, task: P::BackgroundTask) {
        unsafe { (*self.0).enqueue_task(task) }
    }
}

impl<P: Vst2Plugin> Wrapper<P> {
    pub fn create(callback: HostCallback) -> *mut AEffect {
        if callback as usize == 0 {
            return std::ptr::null_mut();
        }
        debug_assert_eq!(std::mem::offset_of!(Self, effect), 0);

        let wrapper = Box::new(Self::new(callback));
        let instance = Box::into_raw(wrapper);
        unsafe {
            (*instance).finish_init();
            instance as *mut AEffect
        }
    }

    fn new(callback: HostCallback) -> Self {
        let mut plugin = P::default();
        let task_executor = plugin.task_executor();
        let params = plugin.params();
        let param_map = params.param_map();

        let mut param_ptrs = Vec::with_capacity(param_map.len());
        let mut param_by_hash = HashMap::with_capacity(param_map.len());
        let mut param_id_to_hash = HashMap::with_capacity(param_map.len());
        let mut param_ptr_to_index = HashMap::with_capacity(param_map.len());
        let mut bypass_index = None;
        for (index, (id, ptr, _group)) in param_map.into_iter().enumerate() {
            let hash = hash_param_id(&id);
            if bypass_index.is_none() && unsafe { ptr.flags() }.contains(ParamFlags::BYPASS) {
                bypass_index = Some(index);
            }
            param_ptr_to_index.insert(ptr, index as i32);
            param_by_hash.insert(hash, ptr);
            param_id_to_hash.insert(id, hash);
            param_ptrs.push(ptr);
        }

        #[cfg(debug_assertions)]
        {
            let mut seen = std::collections::HashSet::new();
            for id in param_id_to_hash.keys() {
                nih_debug_assert!(
                    seen.insert(id.clone()),
                    "Duplicate parameter ID \"{}\". VST2 indices follow parameter declaration order.",
                    id
                );
            }
        }

        let layout = P::AUDIO_IO_LAYOUTS
            .first()
            .copied()
            .unwrap_or_else(AudioIOLayout::const_default);
        let num_inputs = channels(layout.main_input_channels);
        let num_outputs = channels(layout.main_output_channels);
        let max_block = 1024u32;

        Self {
            effect: AEffect {
                magic: VST_MAGIC,
                dispatcher: dispatcher::<P>,
                process: process_unused::<P>,
                set_parameter: set_parameter::<P>,
                get_parameter: get_parameter::<P>,
                num_programs: 1,
                num_params: param_ptrs.len() as i32,
                num_inputs,
                num_outputs,
                flags: CAN_REPLACING | PROGRAM_CHUNKS,
                reserved_1: 0,
                reserved_2: 0,
                initial_delay: 0,
                real_qualities: 0,
                off_qualities: 0,
                io_ratio: 1.0,
                object: std::ptr::null_mut(),
                user: std::ptr::null_mut(),
                unique_id: i32::from_be_bytes(P::VST2_UID),
                version: version_number(P::VERSION),
                process_replacing: process_replacing::<P>,
                process_double_replacing: process_double_unused::<P>,
                future: [0; 56],
            },
            inner: Inner {
                plugin: Mutex::new(plugin),
                task_executor: Mutex::new(task_executor),
                params,
                param_ptrs,
                param_by_hash,
                param_id_to_hash,
                param_ptr_to_index,
                bypass_index,
                host: callback,
                editor: Mutex::new(None),
                editor_handle: Mutex::new(None),
                scale: AtomicF32::new(1.0),
                rect_bits: AtomicU64::new(0),
                rect_slot: UnsafeCell::new(ERect {
                    top: 0,
                    left: 0,
                    bottom: 0,
                    right: 0,
                }),
                buffer_config: AtomicCell::new(None),
                layout: AtomicCell::new(layout),
                buffer_manager: Mutex::new(BufferManager::for_audio_io_layout(
                    max_block as usize,
                    layout,
                )),
                sample_rate: AtomicF32::new(44100.0),
                max_block: AtomicU32::new(max_block),
                latency: AtomicU32::new(0),
                latency_dirty: AtomicBool::new(false),
                flushing: AtomicBool::new(false),
                tail: AtomicI32::new(1),
                active: AtomicBool::new(false),
                initialized: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                chunk: Mutex::new(Vec::new()),
                tasks: ArrayQueue::new(TASK_QUEUE_CAPACITY),
            },
        }
    }

    fn finish_init(&mut self) {
        let wrapper_ptr = self as *const Self;
        unsafe {
            (*self.effect_ptr()).object = wrapper_ptr as *mut c_void;
        }

        let background = WrapperPtr(wrapper_ptr);
        let gui = WrapperPtr(wrapper_ptr);
        let executor = AsyncExecutor {
            execute_background: Arc::new(move |task| background.enqueue(task)),
            execute_gui: Arc::new(move |task| gui.enqueue(task)),
        };
        let editor = self.inner.plugin.lock().editor(executor);
        let has_editor = editor.is_some();
        *self.inner.editor.lock() = editor;
        if !has_editor {
            return;
        }

        unsafe {
            (*self.effect_ptr()).flags |= HAS_EDITOR;
        }
        let scale = system_scale();
        self.inner.scale.store(scale, Ordering::Relaxed);
        if let Some(editor) = self.inner.editor.lock().as_ref() {
            editor.set_scale_factor(scale);
        }
        if let Some((width, height)) = self.logical_size() {
            self.store_rect(width, height);
        }
    }

    fn effect_ptr(&self) -> *mut AEffect {
        self as *const Self as *mut AEffect
    }

    pub(super) fn enqueue_task(&self, task: P::BackgroundTask) {
        let _ = self.inner.tasks.push(task);
    }

    fn drain_tasks(&self) {
        while let Some(task) = self.inner.tasks.pop() {
            (self.inner.task_executor.lock())(task);
        }
    }

    pub(super) fn set_latency_samples(&self, samples: u32) {
        let previous = self.inner.latency.swap(samples, Ordering::Relaxed);
        if previous == samples {
            return;
        }
        unsafe {
            (*self.effect_ptr()).initial_delay = samples as i32;
        }
        self.inner.latency_dirty.store(true, Ordering::Release);
    }

    fn flush_latency(&self) {
        if self.inner.flushing.swap(true, Ordering::AcqRel) {
            return;
        }
        for _ in 0..4 {
            if !self.inner.latency_dirty.swap(false, Ordering::AcqRel) {
                break;
            }
            let samples = self.inner.latency.load(Ordering::Relaxed);
            unsafe {
                let effect = self.effect_ptr();
                (*effect).initial_delay = samples as i32;
                (self.inner.host)(
                    effect,
                    AUDIO_MASTER_IO_CHANGED,
                    0,
                    0,
                    std::ptr::null_mut(),
                    0.0,
                );
            }
        }
        self.inner.flushing.store(false, Ordering::Release);
    }

    fn sample_rate(&self) -> f32 {
        let rate = self.inner.sample_rate.load(Ordering::Relaxed);
        if rate.is_finite() && rate > 0.0 {
            rate
        } else {
            44100.0
        }
    }

    fn configure(&self, rate: Option<f32>, block: Option<i32>) {
        let mut changed = false;
        if let Some(rate) = rate {
            if rate.is_finite() && rate > 0.0 {
                let previous = self.inner.sample_rate.load(Ordering::Relaxed);
                if (previous - rate).abs() > 0.01 {
                    self.inner.sample_rate.store(rate, Ordering::Relaxed);
                    changed = true;
                }
            }
        }
        if let Some(block) = block {
            if block > 0 {
                let block = block as u32;
                let previous = self.inner.max_block.swap(block, Ordering::Relaxed);
                changed |= previous != block;
            }
        }
        if changed && self.inner.active.load(Ordering::Acquire) {
            self.initialize_plugin();
        }
    }

    fn initialize_plugin(&self) {
        let sample_rate = self.sample_rate();
        let max_block = self.inner.max_block.load(Ordering::Relaxed).max(1);
        let layout = self.inner.layout.load();
        {
            let mut manager = self.inner.buffer_manager.lock();
            *manager = BufferManager::for_audio_io_layout(max_block as usize, layout);
        }
        let config = BufferConfig {
            sample_rate,
            min_buffer_size: None,
            max_buffer_size: max_block,
            process_mode: ProcessMode::Realtime,
        };
        self.inner.buffer_config.store(Some(config));

        let mut plugin = self.inner.plugin.lock();
        let mut context = WrapperInitContext {
            wrapper: self as *const Self,
        };
        let _ = plugin.initialize(&layout, &config, &mut context);
        plugin.reset();
        drop(plugin);
        self.inner.initialized.store(true, Ordering::Release);
        self.flush_latency();
    }

    fn resume(&self) {
        self.initialize_plugin();
        self.inner.active.store(true, Ordering::Release);
    }

    fn suspend(&self) {
        self.inner.active.store(false, Ordering::Release);
        if self.inner.initialized.swap(false, Ordering::AcqRel) {
            self.inner.plugin.lock().deactivate();
        }
    }

    fn select_layout(&self, inputs: i32, outputs: i32) -> bool {
        if inputs < 0 || outputs < 0 {
            return false;
        }
        let inputs = inputs as u32;
        let outputs = outputs as u32;
        let Some(layout) = P::AUDIO_IO_LAYOUTS.iter().copied().find(|layout| {
            channels(layout.main_input_channels) as u32 == inputs
                && channels(layout.main_output_channels) as u32 == outputs
        }) else {
            return false;
        };
        if self.inner.layout.load() == layout {
            return true;
        }
        self.inner.layout.store(layout);
        unsafe {
            let effect = self.effect_ptr();
            (*effect).num_inputs = inputs as i32;
            (*effect).num_outputs = outputs as i32;
        }
        if self.inner.active.load(Ordering::Acquire) {
            self.initialize_plugin();
        }
        true
    }

    fn dispatch(&self, opcode: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
        if self.inner.closed.load(Ordering::Acquire) {
            return 0;
        }
        let result = match opcode {
            EFF_OPEN | EFF_IDLE | EFF_EDIT_IDLE => {
                self.drain_tasks();
                1
            }
            EFF_SET_PROGRAM | EFF_GET_PROGRAM => 0,
            EFF_GET_PROGRAM_NAME => {
                write_cstring(ptr, "Default", PROGRAM_NAME_LEN);
                1
            }
            EFF_GET_PROGRAM_NAME_INDEXED => {
                if index == 0 {
                    write_cstring(ptr, "Default", PROGRAM_NAME_LEN);
                    1
                } else {
                    0
                }
            }
            EFF_GET_PARAM_LABEL => {
                self.write_param_string(index, ptr, PARAM_STR_LEN, |param| unsafe {
                    param.unit().to_string()
                });
                1
            }
            EFF_GET_PARAM_DISPLAY => {
                self.write_param_string(index, ptr, PARAM_STR_LEN, |param| unsafe {
                    param.normalized_value_to_string(param.unmodulated_normalized_value(), false)
                });
                1
            }
            EFF_GET_PARAM_NAME => {
                self.write_param_string(index, ptr, PARAM_STR_LEN, |param| unsafe {
                    param.name().to_string()
                });
                1
            }
            EFF_SET_SAMPLE_RATE => {
                self.configure(Some(opt), None);
                1
            }
            EFF_SET_BLOCK_SIZE => {
                self.configure(None, Some(value as i32));
                1
            }
            EFF_SET_BLOCK_SIZE_AND_SAMPLE_RATE => {
                self.configure(Some(opt), Some(value as i32));
                1
            }
            EFF_MAINS_CHANGED => {
                if value == 0 {
                    self.suspend();
                } else {
                    self.resume();
                }
                self.drain_tasks();
                1
            }
            EFF_START_PROCESS => {
                if self.inner.initialized.load(Ordering::Acquire) {
                    self.inner.active.store(true, Ordering::Release);
                } else {
                    self.resume();
                }
                1
            }
            EFF_STOP_PROCESS => {
                self.inner.active.store(false, Ordering::Release);
                1
            }
            EFF_EDIT_GET_RECT => self.edit_get_rect(ptr),
            EFF_EDIT_OPEN => {
                if self.open_editor(ptr) {
                    1
                } else {
                    0
                }
            }
            EFF_EDIT_CLOSE => {
                self.close_editor();
                1
            }
            EFF_GET_CHUNK => self.save_chunk(ptr),
            EFF_SET_CHUNK => {
                if self.load_chunk(ptr, value) {
                    1
                } else {
                    0
                }
            }
            EFF_CAN_BE_AUTOMATED => {
                if self.can_automate(index) {
                    1
                } else {
                    0
                }
            }
            EFF_STRING_2_PARAMETER => {
                if self.string_to_parameter(index, ptr) {
                    1
                } else {
                    0
                }
            }
            EFF_SET_SPEAKER_ARRANGEMENT => {
                let current_inputs = unsafe { (*self.effect_ptr()).num_inputs };
                let current_outputs = unsafe { (*self.effect_ptr()).num_outputs };
                let input = value as *const SpeakerArrangement;
                let output = ptr as *const SpeakerArrangement;
                if input.is_null() && output.is_null() {
                    0
                } else if self.select_layout(
                    speaker_channels(input, current_inputs),
                    speaker_channels(output, current_outputs),
                ) {
                    1
                } else {
                    0
                }
            }
            EFF_SET_BYPASS => {
                if self.set_bypass(value != 0) {
                    1
                } else {
                    0
                }
            }
            EFF_GET_EFFECT_NAME | EFF_GET_PRODUCT_STRING => {
                write_cstring(
                    ptr,
                    P::NAME,
                    if opcode == EFF_GET_EFFECT_NAME {
                        EFFECT_NAME_LEN
                    } else {
                        STRING_LEN
                    },
                );
                1
            }
            EFF_GET_VENDOR_STRING => {
                write_cstring(ptr, P::VENDOR, STRING_LEN);
                1
            }
            EFF_GET_VENDOR_VERSION => version_number(P::VERSION) as isize,
            EFF_CAN_DO => can_do(ptr, self.inner.bypass_index.is_some()),
            EFF_GET_PLUG_CATEGORY => PLUG_CATEG_EFFECT as isize,
            EFF_GET_TAIL_SIZE => self.inner.tail.load(Ordering::Relaxed) as isize,
            EFF_GET_VST_VERSION => VST_VERSION_2_4 as isize,
            _ => 0,
        };
        self.flush_latency();
        result
    }

    fn shutdown(&self) {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.close_editor();
        self.suspend();
    }

    fn edit_get_rect(&self, ptr: *mut c_void) -> isize {
        if ptr.is_null() || self.inner.editor.lock().is_none() {
            return 0;
        }
        if let Some((width, height)) = self.logical_size() {
            self.store_rect(width, height);
        }
        let bits = self.inner.rect_bits.load(Ordering::Acquire);
        unsafe {
            *self.inner.rect_slot.get() = unpack_rect(bits);
            *(ptr as *mut *mut ERect) = self.inner.rect_slot.get();
        }
        1
    }

    fn open_editor(&self, parent: *mut c_void) -> bool {
        if parent.is_null() {
            return false;
        }
        let scale = window_scale(parent);
        self.inner.scale.store(scale, Ordering::Relaxed);
        let context = Arc::new(WrapperGuiContext {
            wrapper: self as *const Self,
        });
        let handle = {
            let editor = self.inner.editor.lock();
            let Some(editor) = editor.as_ref() else {
                return false;
            };
            editor.set_scale_factor(scale);
            editor.spawn(parent_window(parent), context)
        };
        *self.inner.editor_handle.lock() = Some(handle);
        let _ = self.request_resize();
        true
    }

    fn close_editor(&self) {
        *self.inner.editor_handle.lock() = None;
    }

    pub(super) fn request_resize(&self) -> bool {
        let Some((width, height)) = self.logical_size() else {
            return false;
        };
        self.store_rect(width, height);
        let (width, height) = scaled_size(width, height, self.inner.scale.load(Ordering::Relaxed));
        let accepted = unsafe {
            (self.inner.host)(
                self.effect_ptr(),
                AUDIO_MASTER_SIZE_WINDOW,
                width,
                height as isize,
                std::ptr::null_mut(),
                0.0,
            )
        };
        accepted != 0
    }

    fn logical_size(&self) -> Option<(u32, u32)> {
        self.inner
            .editor
            .lock()
            .as_ref()
            .map(|editor| editor.size())
    }

    fn store_rect(&self, width: u32, height: u32) {
        let (width, height) = scaled_size(width, height, self.inner.scale.load(Ordering::Relaxed));
        let rect = ERect {
            top: 0,
            left: 0,
            bottom: height.min(i16::MAX as i32) as i16,
            right: width.min(i16::MAX as i32) as i16,
        };
        self.inner
            .rect_bits
            .store(pack_rect(rect), Ordering::Release);
    }

    fn save_chunk(&self, ptr: *mut c_void) -> isize {
        let bytes = unsafe {
            state::serialize_json::<P>(
                self.inner.params.clone(),
                state::make_params_iter(&self.inner.param_by_hash, &self.inner.param_id_to_hash),
            )
        };
        let Ok(bytes) = bytes else {
            return 0;
        };
        if bytes.is_empty() || ptr.is_null() {
            return 0;
        }
        let mut chunk = self.inner.chunk.lock();
        *chunk = bytes;
        let length = chunk.len() as isize;
        unsafe {
            *(ptr as *mut *mut u8) = chunk.as_mut_ptr();
        }
        length
    }

    fn load_chunk(&self, ptr: *mut c_void, length: isize) -> bool {
        if ptr.is_null() || length <= 0 {
            return false;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, length as usize) };
        let Some(plugin_state) = (unsafe { state::deserialize_json(bytes) }) else {
            return false;
        };
        self.apply_state_object(plugin_state)
    }

    pub(super) fn state_object(&self) -> StateObject {
        unsafe {
            state::serialize_object::<P>(
                self.inner.params.clone(),
                state::make_params_iter(&self.inner.param_by_hash, &self.inner.param_id_to_hash),
            )
        }
    }

    pub(super) fn apply_state_object(&self, mut plugin_state: PluginState) -> bool {
        let config = self.inner.buffer_config.load();
        let restored = unsafe {
            state::deserialize_object::<P>(
                &mut plugin_state,
                self.inner.params.clone(),
                state::make_params_getter(&self.inner.param_by_hash, &self.inner.param_id_to_hash),
                config.as_ref(),
            )
        };
        if self.inner.active.load(Ordering::Acquire) {
            self.initialize_plugin();
        }
        if let Some(editor) = self.inner.editor.lock().as_ref() {
            editor.param_values_changed();
        }
        restored
    }

    fn can_automate(&self, index: i32) -> bool {
        let Some(&param) = self.inner.param_ptrs.get(index as usize) else {
            return false;
        };
        let flags = unsafe { param.flags() };
        !flags.contains(ParamFlags::NON_AUTOMATABLE) && !flags.contains(ParamFlags::HIDDEN)
    }

    pub(super) fn set_param_normalized(&self, param: ParamPtr, normalized: f32) {
        unsafe {
            param.set_normalized_value(normalized);
            param.update_smoother(self.sample_rate(), false);
        }
    }

    fn set_parameter_at(&self, index: i32, normalized: f32) {
        let Some(&param) = self.inner.param_ptrs.get(index as usize) else {
            return;
        };
        self.set_param_normalized(param, normalized);
    }

    fn parameter_at(&self, index: i32) -> f32 {
        let Some(&param) = self.inner.param_ptrs.get(index as usize) else {
            return 0.0;
        };
        unsafe { param.unmodulated_normalized_value() }
    }

    fn string_to_parameter(&self, index: i32, ptr: *mut c_void) -> bool {
        if ptr.is_null() {
            return false;
        }
        let Ok(text) = (unsafe { CStr::from_ptr(ptr as *const c_char) }).to_str() else {
            return false;
        };
        let Some(&param) = self.inner.param_ptrs.get(index as usize) else {
            return false;
        };
        let Some(normalized) = (unsafe { param.string_to_normalized_value(text) }) else {
            return false;
        };
        self.set_param_normalized(param, normalized);
        true
    }

    fn set_bypass(&self, bypassed: bool) -> bool {
        let Some(index) = self.inner.bypass_index else {
            return false;
        };
        self.set_parameter_at(index as i32, if bypassed { 1.0 } else { 0.0 });
        true
    }

    fn param_index(&self, param: ParamPtr) -> Option<i32> {
        self.inner.param_ptr_to_index.get(&param).copied()
    }

    pub(super) fn notify_begin_edit(&self, param: ParamPtr) {
        let Some(index) = self.param_index(param) else {
            return;
        };
        self.host_callback(AUDIO_MASTER_BEGIN_EDIT, index, 0, 0.0);
    }

    pub(super) fn notify_end_edit(&self, param: ParamPtr) {
        let Some(index) = self.param_index(param) else {
            return;
        };
        self.host_callback(AUDIO_MASTER_END_EDIT, index, 0, 0.0);
    }

    pub(super) fn notify_automate(&self, param: ParamPtr, normalized: f32) {
        let Some(index) = self.param_index(param) else {
            return;
        };
        self.host_callback(AUDIO_MASTER_AUTOMATE, index, 0, normalized);
    }

    fn host_callback(&self, opcode: i32, index: i32, value: isize, opt: f32) -> isize {
        unsafe {
            (self.inner.host)(
                self.effect_ptr(),
                opcode,
                index,
                value,
                std::ptr::null_mut(),
                opt,
            )
        }
    }

    fn write_param_string(
        &self,
        index: i32,
        ptr: *mut c_void,
        max_len: usize,
        text: impl FnOnce(ParamPtr) -> String,
    ) {
        let Some(&param) = self.inner.param_ptrs.get(index as usize) else {
            return;
        };
        write_cstring(ptr, &text(param), max_len);
    }

    fn process_audio(&self, inputs: *mut *mut f32, outputs: *mut *mut f32, frames: i32) {
        let frames = frames.max(0) as usize;
        let layout = self.inner.layout.load();
        let input_channels = channels(layout.main_input_channels) as usize;
        let output_channels = channels(layout.main_output_channels) as usize;
        if frames == 0 || output_channels == 0 {
            return;
        }
        let ready = self.inner.active.load(Ordering::Acquire)
            && pointers_valid(outputs, output_channels)
            && (input_channels == 0 || pointers_valid(inputs, input_channels));
        if !ready {
            silence(outputs, output_channels, frames);
            return;
        }

        let transport = self.read_transport();
        let status = process_wrapper(|| {
            let wrapper = self as *const Self;
            let mut manager = self.inner.buffer_manager.lock();
            let buffers = unsafe {
                manager.create_buffers(0, frames, |source| {
                    if input_channels > 0 {
                        if let Some(ptrs) = NonNull::new(inputs) {
                            *source.main_input_channel_pointers = Some(ChannelPointers {
                                ptrs,
                                num_channels: input_channels,
                            });
                        }
                    }
                    if let Some(ptrs) = NonNull::new(outputs) {
                        *source.main_output_channel_pointers = Some(ChannelPointers {
                            ptrs,
                            num_channels: output_channels,
                        });
                    }
                })
            };
            let mut aux = AuxiliaryBuffers {
                inputs: buffers.aux_inputs,
                outputs: buffers.aux_outputs,
            };
            let mut context = WrapperProcessContext::new(wrapper, transport);
            let mut plugin = self.inner.plugin.lock();
            plugin.process(buffers.main_buffer, &mut aux, &mut context)
        });
        self.inner.tail.store(
            match status {
                ProcessStatus::Tail(samples) => samples.max(1) as i32,
                _ => 1,
            },
            Ordering::Relaxed,
        );
        self.flush_latency();
    }

    fn read_transport(&self) -> crate::prelude::Transport {
        let mut transport = crate::prelude::Transport::new(self.sample_rate());
        let mask = TRANSPORT_PLAYING
            | TRANSPORT_CYCLE_ACTIVE
            | TRANSPORT_RECORDING
            | PPQ_POS_VALID
            | TEMPO_VALID
            | BARS_VALID
            | CYCLE_POS_VALID
            | TIME_SIG_VALID;
        let pointer = self.host_callback(AUDIO_MASTER_GET_TIME, 0, mask as isize, 0.0);
        if pointer == 0 {
            return transport;
        }
        let info = unsafe { &*(pointer as *const VstTimeInfo) };
        transport.playing = info.flags & TRANSPORT_PLAYING != 0;
        transport.recording = info.flags & TRANSPORT_RECORDING != 0;
        if info.flags & TEMPO_VALID != 0 {
            transport.tempo = Some(info.tempo);
        }
        if info.flags & TIME_SIG_VALID != 0 {
            transport.time_sig_numerator = Some(info.time_sig_numerator);
            transport.time_sig_denominator = Some(info.time_sig_denominator);
        }
        if info.sample_rate > 0.0 {
            transport.pos_samples = Some(info.sample_pos.round() as i64);
            transport.pos_seconds = Some(info.sample_pos / info.sample_rate);
        }
        if info.flags & PPQ_POS_VALID != 0 {
            transport.pos_beats = Some(info.ppq_pos);
        }
        if info.flags & BARS_VALID != 0 {
            transport.bar_start_pos_beats = Some(info.bar_start_pos);
        }
        if info.flags & TRANSPORT_CYCLE_ACTIVE != 0 && info.flags & CYCLE_POS_VALID != 0 {
            transport.loop_range_beats = Some((info.cycle_start_pos, info.cycle_end_pos));
        }
        transport
    }
}

unsafe extern "C" fn dispatcher<P: Vst2Plugin>(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize {
    if effect.is_null() {
        return 0;
    }
    if opcode == EFF_CLOSE {
        let wrapper = Box::from_raw(effect as *mut Wrapper<P>);
        wrapper.shutdown();
        return 1;
    }
    let wrapper = unsafe { &*(effect as *const Wrapper<P>) };
    wrapper.dispatch(opcode, index, value, ptr, opt)
}

unsafe extern "C" fn set_parameter<P: Vst2Plugin>(effect: *mut AEffect, index: i32, value: f32) {
    if effect.is_null() {
        return;
    }
    let wrapper = unsafe { &*(effect as *const Wrapper<P>) };
    if wrapper.inner.closed.load(Ordering::Acquire) {
        return;
    }
    wrapper.set_parameter_at(index, value);
}

unsafe extern "C" fn get_parameter<P: Vst2Plugin>(effect: *mut AEffect, index: i32) -> f32 {
    if effect.is_null() {
        return 0.0;
    }
    let wrapper = unsafe { &*(effect as *const Wrapper<P>) };
    if wrapper.inner.closed.load(Ordering::Acquire) {
        return 0.0;
    }
    wrapper.parameter_at(index)
}

unsafe extern "C" fn process_replacing<P: Vst2Plugin>(
    effect: *mut AEffect,
    inputs: *mut *mut f32,
    outputs: *mut *mut f32,
    sample_frames: i32,
) {
    if effect.is_null() {
        return;
    }
    let wrapper = unsafe { &*(effect as *const Wrapper<P>) };
    if wrapper.inner.closed.load(Ordering::Acquire) {
        return;
    }
    wrapper.process_audio(inputs, outputs, sample_frames);
}

unsafe extern "C" fn process_unused<P: Vst2Plugin>(
    _effect: *mut AEffect,
    _inputs: *mut *mut f32,
    _outputs: *mut *mut f32,
    _sample_frames: i32,
) {
}

unsafe extern "C" fn process_double_unused<P: Vst2Plugin>(
    _effect: *mut AEffect,
    _inputs: *mut *mut f64,
    _outputs: *mut *mut f64,
    _sample_frames: i32,
) {
}

fn channels(value: Option<std::num::NonZeroU32>) -> i32 {
    value.map(std::num::NonZeroU32::get).unwrap_or(0) as i32
}

fn speaker_channels(arrangement: *const SpeakerArrangement, fallback: i32) -> i32 {
    if arrangement.is_null() {
        fallback
    } else {
        unsafe { (*arrangement).num_channels }
    }
}

fn version_number(version: &str) -> i32 {
    let mut parts = version.split(['.', '-', '+']);
    let major = parts
        .next()
        .and_then(|part| part.parse::<i32>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|part| part.parse::<i32>().ok())
        .unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|part| part.parse::<i32>().ok())
        .unwrap_or(0);
    major
        .saturating_mul(10_000)
        .saturating_add(minor.saturating_mul(100))
        .saturating_add(patch)
}

fn write_cstring(dest: *mut c_void, src: &str, max_len: usize) {
    if dest.is_null() || max_len == 0 {
        return;
    }
    let max_len = max_len.min(128);
    let mut buffer = [0 as c_char; 128];
    crate::wrapper::util::strlcpy(&mut buffer[..max_len], src);
    unsafe {
        std::ptr::copy_nonoverlapping(buffer.as_ptr(), dest as *mut c_char, max_len);
    }
}

fn can_do(ptr: *mut c_void, has_bypass: bool) -> isize {
    if ptr.is_null() {
        return 0;
    }
    let Ok(text) = (unsafe { CStr::from_ptr(ptr as *const c_char) }).to_str() else {
        return 0;
    };
    match text {
        "receiveVstTimeInfo" | "plugAsChannelInsert" | "plugAsSend" => 1,
        "bypass" if has_bypass => 1,
        "bypass"
        | "receiveVstEvents"
        | "receiveVstMidiEvent"
        | "sendVstEvents"
        | "sendVstMidiEvent" => 0,
        _ => -1,
    }
}

fn pack_rect(rect: ERect) -> u64 {
    u64::from(rect.top as u16)
        | (u64::from(rect.left as u16) << 16)
        | (u64::from(rect.bottom as u16) << 32)
        | (u64::from(rect.right as u16) << 48)
}

fn unpack_rect(bits: u64) -> ERect {
    ERect {
        top: bits as u16 as i16,
        left: (bits >> 16) as u16 as i16,
        bottom: (bits >> 32) as u16 as i16,
        right: (bits >> 48) as u16 as i16,
    }
}

fn scaled_size(width: u32, height: u32, scale: f32) -> (i32, i32) {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let width = ((width as f32) * scale).round().max(1.0) as i32;
    let height = ((height as f32) * scale).round().max(1.0) as i32;
    (width, height)
}

fn pointers_valid(ptrs: *mut *mut f32, channels: usize) -> bool {
    if channels == 0 {
        return true;
    }
    if ptrs.is_null() {
        return false;
    }
    for channel in 0..channels {
        unsafe {
            if (*ptrs.add(channel)).is_null() {
                return false;
            }
        }
    }
    true
}

fn silence(outputs: *mut *mut f32, channels: usize, frames: usize) {
    if outputs.is_null() || frames == 0 {
        return;
    }
    for channel in 0..channels {
        unsafe {
            let samples = *outputs.add(channel);
            if !samples.is_null() {
                std::ptr::write_bytes(samples, 0, frames);
            }
        }
    }
}

fn parent_window(ptr: *mut c_void) -> ParentWindowHandle {
    #[cfg(windows)]
    {
        ParentWindowHandle::Win32Hwnd(ptr)
    }
    #[cfg(target_os = "macos")]
    {
        ParentWindowHandle::AppKitNsView(ptr)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        ParentWindowHandle::X11Window(ptr as usize as u32)
    }
}

fn system_scale() -> f32 {
    #[cfg(windows)]
    {
        scale_from_dpi(unsafe { GetDpiForSystem() })
    }
    #[cfg(not(windows))]
    {
        1.0
    }
}

fn window_scale(hwnd: *mut c_void) -> f32 {
    #[cfg(windows)]
    {
        let dpi = if hwnd.is_null() {
            0
        } else {
            unsafe { GetDpiForWindow(hwnd) }
        };
        if dpi == 0 {
            system_scale()
        } else {
            scale_from_dpi(dpi)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        1.0
    }
}

fn scale_from_dpi(dpi: u32) -> f32 {
    if dpi == 0 {
        1.0
    } else {
        dpi as f32 / 96.0
    }
}

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn GetDpiForWindow(hwnd: *mut c_void) -> u32;
    fn GetDpiForSystem() -> u32;
}
