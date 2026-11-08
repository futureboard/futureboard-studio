//! The plug-in kit's types, as far as `plate.rs` names them.
//!
//! `plate.rs` also carries the header and preset menu of the KitModel
//! editors (BurnLimit, the EQs, Zapp). Rodhareist draws neither: it is not a
//! KitModel. Mounting Studio's real kit would pull in its live overlay and
//! every family's telemetry, so these stand-ins only give `plate.rs` the
//! types it is written against. No value of them can exist — each holds an
//! uninhabited field — so none of these methods can ever run.

use std::convert::Infallible;
use std::sync::Arc;

use gpui::{App, Context, Window};

pub(crate) type Cx<'a, M> = Context<'a, KitEditor<M>>;

pub struct KitPreset<P> {
    pub name: &'static str,
    pub params: P,
}

pub trait KitModel: Copy + PartialEq + 'static {
    type Params: Clone + 'static;
    fn title(self) -> &'static str;
    fn key(self) -> &'static str;
    fn presets(self) -> Arc<Vec<KitPreset<Self::Params>>>;
}

pub struct Compare {
    pub on_b: bool,
}

pub struct KitEditor<M: KitModel> {
    pub model: M,
    pub compare: Compare,
    pub preset_open: bool,
    pub preset: Option<usize>,
    never: Infallible,
}

impl<M: KitModel> KitEditor<M> {
    pub fn click_cb(
        &self,
        _cx: &Context<Self>,
        _action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        match self.never {}
        #[allow(unreachable_code)]
        |_: &gpui::ClickEvent, _: &mut Window, _: &mut App| {}
    }

    pub fn step_preset(&mut self, _delta: isize, _cx: &mut Context<Self>) {
        match self.never {}
    }

    pub fn swap_compare(&mut self, _cx: &mut Context<Self>) {
        match self.never {}
    }

    pub fn undo(&mut self, _cx: &mut Context<Self>) {
        match self.never {}
    }

    pub fn redo(&mut self, _cx: &mut Context<Self>) {
        match self.never {}
    }

    pub fn can_undo(&self) -> bool {
        match self.never {}
    }

    pub fn can_redo(&self) -> bool {
        match self.never {}
    }

    pub fn power(&self) -> bool {
        match self.never {}
    }

    pub fn toggle(&mut self, _id: &str, _cx: &mut Context<Self>) {
        match self.never {}
    }

    pub fn notice(&self) -> Option<&str> {
        match self.never {}
    }

    pub fn preset_label(&self) -> String {
        match self.never {}
    }

    pub fn toggle_preset_menu(&mut self, _cx: &mut Context<Self>) {
        match self.never {}
    }

    pub fn close_preset_menu(&mut self, _cx: &mut Context<Self>) {
        match self.never {}
    }

    pub fn load_preset(&mut self, _index: isize, _cx: &mut Context<Self>) {
        match self.never {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LiveSlot {
    Plate(usize),
}

pub struct LiveBounds(Infallible);

impl LiveBounds {
    pub fn slot(&self, _slot: LiveSlot) -> gpui::Canvas<()> {
        match self.0 {}
    }
}
