//! Built-in plug-in instance manager.
//!
//! One built-in plug-in **library/type** (e.g. `builtin:rodharerist`) is loaded
//! once but may back many DSP instances — one per insert slot on any track. There
//! is at most **one shared CEF editor window per plug-in type**, whose active
//! binding can switch between instances (see the design in
//! `crates/BuiltinAudioPlugins`). This module owns that control-thread bookkeeping:
//! which instance lives in which insert slot, and which instance the shared editor
//! is currently showing.
//!
//! It is **control-thread only** — the audio callback never touches these maps; it
//! references instances by the compact [`InstanceId`] resolved before playback.

use std::collections::HashMap;

use crate::builtin::builtin_editor_url;

/// A track insert slot a built-in instance occupies. `track_id` is the host's
/// stable track identity; `insert_index` is the slot on that track's chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InsertSlot {
    pub track_id: u64,
    pub insert_index: u32,
}

impl InsertSlot {
    pub fn new(track_id: u64, insert_index: u32) -> Self {
        Self {
            track_id,
            insert_index,
        }
    }
}

/// Unique id for a live built-in DSP instance. Compact and `Copy` so the engine
/// can route MIDI/params to exactly one instance without string lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId(pub u64);

/// One live built-in instance.
#[derive(Debug, Clone)]
pub struct PluginInstance {
    pub id: InstanceId,
    /// Registry id of the plug-in type, e.g. `builtin:rodharerist`.
    pub plugin_id: String,
    /// The insert slot this instance occupies.
    pub slot: InsertSlot,
}

/// Control-thread registry of built-in instances and shared-editor bindings.
#[derive(Debug, Default)]
pub struct InstanceManager {
    instances: HashMap<InstanceId, PluginInstance>,
    by_slot: HashMap<InsertSlot, InstanceId>,
    /// One editor binding per plug-in type → the instance it currently shows.
    editor_binding: HashMap<String, InstanceId>,
    next_id: u64,
}

impl InstanceManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an instance of `plugin_id` at `slot`, returning its id. If the slot
    /// was already occupied, the previous instance is removed first (one instance
    /// per slot). The new instance does **not** auto-bind the editor.
    pub fn create_instance(
        &mut self,
        plugin_id: impl Into<String>,
        slot: InsertSlot,
    ) -> InstanceId {
        if let Some(existing) = self.by_slot.get(&slot).copied() {
            self.remove_instance(existing);
        }
        self.next_id += 1;
        let id = InstanceId(self.next_id);
        let plugin_id = plugin_id.into();
        self.instances.insert(
            id,
            PluginInstance {
                id,
                plugin_id: plugin_id.clone(),
                slot,
            },
        );
        self.by_slot.insert(slot, id);
        id
    }

    /// Remove an instance. If the shared editor for its type was bound to it, the
    /// binding falls back to another instance of the same type (or clears).
    pub fn remove_instance(&mut self, id: InstanceId) -> Option<PluginInstance> {
        let removed = self.instances.remove(&id)?;
        self.by_slot.remove(&removed.slot);
        if self.editor_binding.get(&removed.plugin_id) == Some(&id) {
            match self.instances_of(&removed.plugin_id).first() {
                Some(next) => {
                    self.editor_binding
                        .insert(removed.plugin_id.clone(), next.id);
                }
                None => {
                    self.editor_binding.remove(&removed.plugin_id);
                }
            }
        }
        Some(removed)
    }

    pub fn instance(&self, id: InstanceId) -> Option<&PluginInstance> {
        self.instances.get(&id)
    }

    pub fn instance_at(&self, slot: InsertSlot) -> Option<&PluginInstance> {
        self.by_slot
            .get(&slot)
            .and_then(|id| self.instances.get(id))
    }

    /// All instances of a plug-in type, ordered by id (creation order).
    pub fn instances_of(&self, plugin_id: &str) -> Vec<&PluginInstance> {
        let mut out: Vec<&PluginInstance> = self
            .instances
            .values()
            .filter(|i| i.plugin_id == plugin_id)
            .collect();
        out.sort_by_key(|i| i.id);
        out
    }

    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Point the shared editor for `instance`'s type at that instance. Returns
    /// `false` if the id is unknown.
    pub fn bind_editor(&mut self, id: InstanceId) -> bool {
        let Some(instance) = self.instances.get(&id) else {
            return false;
        };
        self.editor_binding.insert(instance.plugin_id.clone(), id);
        true
    }

    /// The instance the shared editor of `plugin_id` is currently showing.
    pub fn bound_instance(&self, plugin_id: &str) -> Option<&PluginInstance> {
        self.editor_binding
            .get(plugin_id)
            .and_then(|id| self.instances.get(id))
    }

    /// The `mikoplugin://` editor URL for a plug-in type (built-ins with a UI).
    pub fn editor_url(&self, plugin_id: &str) -> Option<String> {
        builtin_editor_url(plugin_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROD: &str = "builtin:rodharerist";

    #[test]
    fn creating_instances_assigns_unique_ids_and_slots() {
        let mut m = InstanceManager::new();
        let a = m.create_instance(ROD, InsertSlot::new(1, 0));
        let b = m.create_instance(ROD, InsertSlot::new(2, 0));
        assert_ne!(a, b);
        assert_eq!(m.len(), 2);
        assert_eq!(m.instance_at(InsertSlot::new(1, 0)).unwrap().id, a);
        assert_eq!(m.instances_of(ROD).len(), 2);
    }

    #[test]
    fn one_instance_per_slot_replaces_previous() {
        let mut m = InstanceManager::new();
        let slot = InsertSlot::new(1, 0);
        let a = m.create_instance(ROD, slot);
        let b = m.create_instance(ROD, slot);
        assert_ne!(a, b);
        assert_eq!(m.len(), 1);
        assert!(m.instance(a).is_none());
        assert_eq!(m.instance_at(slot).unwrap().id, b);
    }

    #[test]
    fn shared_editor_binding_switches_between_instances() {
        let mut m = InstanceManager::new();
        let a = m.create_instance(ROD, InsertSlot::new(1, 0));
        let b = m.create_instance(ROD, InsertSlot::new(2, 0));
        assert!(m.bound_instance(ROD).is_none());
        assert!(m.bind_editor(a));
        assert_eq!(m.bound_instance(ROD).unwrap().id, a);
        // The one shared editor switches its active binding.
        assert!(m.bind_editor(b));
        assert_eq!(m.bound_instance(ROD).unwrap().id, b);
    }

    #[test]
    fn removing_bound_instance_falls_back_or_clears() {
        let mut m = InstanceManager::new();
        let a = m.create_instance(ROD, InsertSlot::new(1, 0));
        let b = m.create_instance(ROD, InsertSlot::new(2, 0));
        m.bind_editor(b);
        // Removing the bound instance falls back to the remaining one.
        m.remove_instance(b);
        assert_eq!(m.bound_instance(ROD).unwrap().id, a);
        // Removing the last clears the binding.
        m.remove_instance(a);
        assert!(m.bound_instance(ROD).is_none());
        assert!(m.is_empty());
    }

    #[test]
    fn editor_url_resolves_for_builtins_with_ui() {
        let m = InstanceManager::new();
        assert_eq!(
            m.editor_url(ROD).as_deref(),
            Some("mikoplugin://rodharerist/index.html")
        );
        assert_eq!(
            m.editor_url("builtin:equz8").as_deref(),
            Some("mikoplugin://equz8/index.html")
        );
        assert!(m.editor_url("builtin:c1073").is_none());
    }
}
