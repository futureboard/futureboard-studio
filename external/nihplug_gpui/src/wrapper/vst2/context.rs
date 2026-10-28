//! Host callbacks for the VST2 wrapper.

use std::marker::PhantomData;

use crate::prelude::{
    GuiContext, InitContext, ParamPtr, PluginApi, PluginNoteEvent, ProcessContext, Transport,
};
use crate::wrapper::state;

use crate::plugin::vst2::Vst2Plugin;

use super::wrapper::Wrapper;

pub(super) struct WrapperInitContext<P: Vst2Plugin> {
    pub(super) wrapper: *const Wrapper<P>,
}

pub(super) struct WrapperProcessContext<P: Vst2Plugin> {
    pub(super) wrapper: *const Wrapper<P>,
    pub(super) transport: Transport,
    pub(super) _marker: PhantomData<*const ()>,
}

pub(super) struct WrapperGuiContext<P: Vst2Plugin> {
    pub(super) wrapper: *const Wrapper<P>,
}

unsafe impl<P: Vst2Plugin> Send for WrapperInitContext<P> {}
unsafe impl<P: Vst2Plugin> Sync for WrapperInitContext<P> {}
unsafe impl<P: Vst2Plugin> Send for WrapperProcessContext<P> {}
unsafe impl<P: Vst2Plugin> Sync for WrapperProcessContext<P> {}
unsafe impl<P: Vst2Plugin> Send for WrapperGuiContext<P> {}
unsafe impl<P: Vst2Plugin> Sync for WrapperGuiContext<P> {}

impl<P: Vst2Plugin> InitContext<P> for WrapperInitContext<P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Vst2
    }

    fn execute(&self, task: P::BackgroundTask) {
        let wrapper = unsafe { &*self.wrapper };
        (wrapper.inner.task_executor.lock())(task);
    }

    fn set_latency_samples(&self, samples: u32) {
        unsafe { (*self.wrapper).set_latency_samples(samples) };
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

impl<P: Vst2Plugin> ProcessContext<P> for WrapperProcessContext<P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Vst2
    }

    fn execute_background(&self, task: P::BackgroundTask) {
        unsafe { (*self.wrapper).enqueue_task(task) };
    }

    fn execute_gui(&self, task: P::BackgroundTask) {
        unsafe { (*self.wrapper).enqueue_task(task) };
    }

    #[inline]
    fn transport(&self) -> &Transport {
        &self.transport
    }

    fn next_event(&mut self) -> Option<PluginNoteEvent<P>> {
        None
    }

    fn send_event(&mut self, _event: PluginNoteEvent<P>) {}

    fn set_latency_samples(&self, samples: u32) {
        unsafe { (*self.wrapper).set_latency_samples(samples) };
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

impl<P: Vst2Plugin> GuiContext for WrapperGuiContext<P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Vst2
    }

    fn request_resize(&self) -> bool {
        unsafe { (*self.wrapper).request_resize() }
    }

    unsafe fn raw_begin_set_parameter(&self, param: ParamPtr) {
        unsafe { (*self.wrapper).notify_begin_edit(param) };
    }

    unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
        let wrapper = unsafe { &*self.wrapper };
        wrapper.set_param_normalized(param, normalized);
        wrapper.notify_automate(param, normalized);
    }

    unsafe fn raw_end_set_parameter(&self, param: ParamPtr) {
        unsafe { (*self.wrapper).notify_end_edit(param) };
    }

    fn get_state(&self) -> state::PluginState {
        unsafe { (*self.wrapper).state_object() }
    }

    fn set_state(&self, state: state::PluginState) {
        unsafe {
            let _ = (*self.wrapper).apply_state_object(state);
        }
    }
}

impl<P: Vst2Plugin> WrapperProcessContext<P> {
    pub(super) fn new(wrapper: *const Wrapper<P>, transport: Transport) -> Self {
        Self {
            wrapper,
            transport,
            _marker: PhantomData,
        }
    }
}
