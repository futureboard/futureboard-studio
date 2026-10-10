//! Scene recall as plain data: what a recall makes of the current mix, and
//! whether the mix still matches a scene. The engine turns the recalled mix
//! into audio changes (see `LiveEngine::reconcile`).
//!
//! A recall matches strips by id. A strip present now and in the scene is
//! set, section by section as the scene's [`RecallScope`] allows, unless it
//! is recall safe; strips missing from either side are left alone, so a
//! recall never adds or removes a strip. Solo, solo safe, record arm and
//! recall safe are never recalled, nor is the bus setup (a bus's role and
//! width, a matrix's width): like a console's bus setup, it is not a scene's.
//! A matrix's sources recall with the sends.

use crate::session::{
    BusStrip, ChannelStrip, Dca, Id, InsertPlugin, InsertSlot, MasterStrip, MatrixFeed,
    MatrixSource, MatrixStrip, MixState, MuteGroup, OutputPatch, PatchSource, RecallScope,
    SendSlot, StripCore, StripOutput,
};

/// A borrowed view of a mix: the session's own fields, without copying them.
#[derive(Clone, Copy)]
pub struct MixParts<'a> {
    pub channels: &'a [ChannelStrip],
    pub buses: &'a [BusStrip],
    pub master: &'a MasterStrip,
    pub matrices: &'a [MatrixStrip],
    pub outputs: &'a [OutputPatch],
    pub dcas: &'a [Dca],
    pub mute_groups: &'a [MuteGroup],
}

impl MixState {
    pub fn parts(&self) -> MixParts<'_> {
        MixParts {
            channels: &self.channels,
            buses: &self.buses,
            master: &self.master,
            matrices: &self.matrices,
            outputs: &self.outputs,
            dcas: &self.dcas,
            mute_groups: &self.mute_groups,
        }
    }
}

/// What a recall could not do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecallReport {
    /// Inserts added or removed since the scene was stored (on strips the
    /// recall touched), left as they are.
    pub inserts_differ: usize,
    /// Third-party inserts whose saved state differs from the scene's: a
    /// plug-in's state is not recalled (it would mean reloading it).
    pub external_kept: usize,
}

impl RecallReport {
    /// The note a recall's reply carries, if there is anything to say.
    pub fn note(&self) -> Option<String> {
        let mut parts = Vec::new();
        match self.inserts_differ {
            0 => {}
            1 => parts.push("1 insert differs from the scene and was left as it is".to_string()),
            n => parts.push(format!(
                "{n} inserts differ from the scene and were left as they are"
            )),
        }
        match self.external_kept {
            0 => {}
            1 => parts.push("1 third-party plug-in keeps its current settings".to_string()),
            n => parts.push(format!(
                "{n} third-party plug-ins keep their current settings"
            )),
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// The same plug-in: settings can move from one to the other in place.
pub(crate) fn same_plugin(a: &InsertPlugin, b: &InsertPlugin) -> bool {
    match (a, b) {
        (InsertPlugin::Builtin { stem: a, .. }, InsertPlugin::Builtin { stem: b, .. }) => a == b,
        (
            InsertPlugin::External {
                format: fa,
                path: pa,
                class_id: ca,
                ..
            },
            InsertPlugin::External {
                format: fb,
                path: pb,
                class_id: cb,
                ..
            },
        ) => fa == fb && pa == pb && ca == cb,
        _ => false,
    }
}

/// Set `mix` from `scene` as `scope` allows. `mix` is the current mix; its
/// recall-safe flags decide which strips are left alone.
pub fn recall_into(mix: &mut MixState, scene: &MixState, scope: &RecallScope) -> RecallReport {
    let mut report = RecallReport::default();
    let buses_now: Vec<Id> = mix.buses.iter().map(|b| b.id).collect();
    let scene_buses: Vec<Id> = scene.buses.iter().map(|b| b.id).collect();

    for channel in &mut mix.channels {
        if channel.core.recall_safe {
            continue;
        }
        let Some(from) = scene.channels.iter().find(|c| c.id == channel.id) else {
            continue;
        };
        if scope.input {
            channel.trim_db = from.trim_db;
            channel.phase_invert = from.phase_invert;
            channel.input = from.input;
        }
        if scope.names {
            channel.name.clone_from(&from.name);
        }
        if scope.routing {
            channel.output = recalled_output(channel.output, from.output, &buses_now);
        }
        if scope.sends {
            channel.sends = recalled_sends(&channel.sends, &from.sends, &buses_now, &scene_buses);
        }
        recall_core(&mut channel.core, &from.core, scope, &mut report);
    }
    for bus in &mut mix.buses {
        if bus.core.recall_safe {
            continue;
        }
        let Some(from) = scene.buses.iter().find(|b| b.id == bus.id) else {
            continue;
        };
        if scope.names {
            bus.name.clone_from(&from.name);
        }
        if scope.routing && !matches!(from.output, StripOutput::Bus(_)) {
            bus.output = from.output;
        }
        recall_core(&mut bus.core, &from.core, scope, &mut report);
    }
    if !mix.master.core.recall_safe {
        recall_core(&mut mix.master.core, &scene.master.core, scope, &mut report);
    }
    for matrix in &mut mix.matrices {
        if matrix.core.recall_safe {
            continue;
        }
        let Some(from) = scene.matrices.iter().find(|m| m.id == matrix.id) else {
            continue;
        };
        if scope.names {
            matrix.name.clone_from(&from.name);
        }
        if scope.sends {
            matrix.sources =
                recalled_sources(&matrix.sources, &from.sources, &buses_now, &scene_buses);
        }
        recall_core(&mut matrix.core, &from.core, scope, &mut report);
    }

    for (dca, from) in mix.dcas.iter_mut().zip(&scene.dcas) {
        if scope.faders {
            dca.level_db = from.level_db;
        }
        if scope.mutes {
            dca.mute = from.mute;
        }
        if scope.names {
            dca.name.clone_from(&from.name);
            dca.color = from.color;
        }
    }
    for (group, from) in mix.mute_groups.iter_mut().zip(&scene.mute_groups) {
        if scope.mutes {
            group.active = from.active;
        }
        if scope.names {
            group.name.clone_from(&from.name);
        }
    }
    if scope.routing {
        mix.outputs = recalled_outputs(mix.parts(), scene);
    }
    report
}

fn recall_core(
    core: &mut StripCore,
    from: &StripCore,
    scope: &RecallScope,
    report: &mut RecallReport,
) {
    if scope.processing {
        core.processing = from.processing;
    }
    if scope.faders {
        core.fader_db = from.fader_db;
    }
    if scope.mutes {
        core.mute = from.mute;
    }
    if scope.pan {
        core.pan = from.pan;
    }
    if scope.assign {
        core.dcas.clone_from(&from.dcas);
        core.mute_groups.clone_from(&from.mute_groups);
    }
    if scope.names {
        core.color = from.color;
    }
    if scope.inserts {
        for slot in &mut core.inserts {
            match from.inserts.iter().find(|s| s.id == slot.id) {
                Some(stored) if same_plugin(&slot.plugin, &stored.plugin) => {
                    slot.bypass = stored.bypass;
                    match (&mut slot.plugin, &stored.plugin) {
                        (
                            InsertPlugin::Builtin { params, .. },
                            InsertPlugin::Builtin { params: stored, .. },
                        ) => params.clone_from(stored),
                        (
                            InsertPlugin::External { state, .. },
                            InsertPlugin::External { state: stored, .. },
                        ) => {
                            if stored.is_some() && state != stored {
                                report.external_kept += 1;
                            }
                        }
                        _ => {}
                    }
                }
                _ => report.inserts_differ += 1,
            }
        }
        report.inserts_differ += from
            .inserts
            .iter()
            .filter(|stored| !core.inserts.iter().any(|s| s.id == stored.id))
            .count();
    }
}

/// A channel's route from the scene, unless it names a bus that is gone.
fn recalled_output(now: StripOutput, stored: StripOutput, buses_now: &[Id]) -> StripOutput {
    match stored {
        StripOutput::Bus(id) if !buses_now.contains(&id) => now,
        other => other,
    }
}

/// The scene's sends to buses that still exist, and the current sends to
/// buses the scene does not know (added since).
fn recalled_sends(
    now: &[SendSlot],
    stored: &[SendSlot],
    buses_now: &[Id],
    scene_buses: &[Id],
) -> Vec<SendSlot> {
    let mut sends: Vec<SendSlot> = stored
        .iter()
        .filter(|s| buses_now.contains(&s.bus))
        .cloned()
        .collect();
    for send in now {
        if !scene_buses.contains(&send.bus) && !sends.iter().any(|s| s.bus == send.bus) {
            sends.push(send.clone());
        }
    }
    sends
}

/// Whether a matrix source still has something to take from.
fn feed_exists(feed: MatrixFeed, buses: &[Id]) -> bool {
    match feed {
        MatrixFeed::Master => true,
        MatrixFeed::Bus(id) => buses.contains(&id),
    }
}

/// Whether the scene knew this source's bus (the master it always knew).
fn feed_known(feed: MatrixFeed, scene_buses: &[Id]) -> bool {
    match feed {
        MatrixFeed::Master => true,
        MatrixFeed::Bus(id) => scene_buses.contains(&id),
    }
}

/// A matrix's sources as a recall leaves them: the scene's from buses that
/// still exist (and the master), then the current ones from buses the scene
/// does not know.
fn recalled_sources(
    now: &[MatrixSource],
    stored: &[MatrixSource],
    buses_now: &[Id],
    scene_buses: &[Id],
) -> Vec<MatrixSource> {
    let mut sources: Vec<MatrixSource> = stored
        .iter()
        .filter(|s| feed_exists(s.source, buses_now))
        .cloned()
        .collect();
    for source in now {
        if !feed_known(source.source, scene_buses)
            && !sources.iter().any(|s| s.source == source.source)
        {
            sources.push(source.clone());
        }
    }
    sources
}

/// The output patch a recall leaves: the scene's patches for sources that
/// exist now, except a recall-safe strip's (and a strip's the scene does not
/// know), which keep their current patches.
fn recalled_outputs(mix: MixParts<'_>, scene: &MixState) -> Vec<OutputPatch> {
    let channel_now = |id: Id| mix.channels.iter().find(|c| c.id == id);
    let bus_now = |id: Id| mix.buses.iter().find(|b| b.id == id);
    let matrix_now = |id: Id| mix.matrices.iter().find(|m| m.id == id);
    // A source whose current patches stay.
    let kept = |source: PatchSource| match source {
        PatchSource::Master => mix.master.core.recall_safe,
        PatchSource::Monitor => false,
        PatchSource::Channel(id) => {
            channel_now(id).is_some_and(|c| c.core.recall_safe)
                || !scene.channels.iter().any(|c| c.id == id)
        }
        PatchSource::Bus(id) => {
            bus_now(id).is_some_and(|b| b.core.recall_safe)
                || !scene.buses.iter().any(|b| b.id == id)
        }
        PatchSource::Matrix(id) => {
            matrix_now(id).is_some_and(|m| m.core.recall_safe)
                || !scene.matrices.iter().any(|m| m.id == id)
        }
    };
    let exists = |source: PatchSource| match source {
        PatchSource::Channel(id) => channel_now(id).is_some(),
        PatchSource::Bus(id) => bus_now(id).is_some(),
        PatchSource::Matrix(id) => matrix_now(id).is_some(),
        PatchSource::Master | PatchSource::Monitor => true,
    };
    scene
        .outputs
        .iter()
        .filter(|p| exists(p.source) && !kept(p.source))
        .chain(mix.outputs.iter().filter(|p| kept(p.source)))
        .copied()
        .collect()
}

/// Whether recalling `scene` with `scope` would change `mix`: the "modified"
/// light. Reads only; allocates only small id lists and the output patch.
pub fn differs(mix: &MixState, scene: &MixState, scope: &RecallScope) -> bool {
    differs_parts(mix.parts(), scene, scope)
}

/// [`differs`] on a borrowed view of the mix.
pub fn differs_parts(mix: MixParts<'_>, scene: &MixState, scope: &RecallScope) -> bool {
    let buses_now: Vec<Id> = mix.buses.iter().map(|b| b.id).collect();
    let scene_buses: Vec<Id> = scene.buses.iter().map(|b| b.id).collect();
    for channel in mix.channels.iter().filter(|c| !c.core.recall_safe) {
        let Some(from) = scene.channels.iter().find(|c| c.id == channel.id) else {
            continue;
        };
        if channel_differs(channel, from, scope, &buses_now, &scene_buses) {
            return true;
        }
    }
    for bus in mix.buses.iter().filter(|b| !b.core.recall_safe) {
        let Some(from) = scene.buses.iter().find(|b| b.id == bus.id) else {
            continue;
        };
        if bus_differs(bus, from, scope) {
            return true;
        }
    }
    if !mix.master.core.recall_safe && core_differs(&mix.master.core, &scene.master.core, scope) {
        return true;
    }
    for matrix in mix.matrices.iter().filter(|m| !m.core.recall_safe) {
        let Some(from) = scene.matrices.iter().find(|m| m.id == matrix.id) else {
            continue;
        };
        if matrix_differs(matrix, from, scope, &buses_now, &scene_buses) {
            return true;
        }
    }
    for (dca, from) in mix.dcas.iter().zip(&scene.dcas) {
        if (scope.faders && dca.level_db != from.level_db)
            || (scope.mutes && dca.mute != from.mute)
            || (scope.names && (dca.name != from.name || dca.color != from.color))
        {
            return true;
        }
    }
    for (group, from) in mix.mute_groups.iter().zip(&scene.mute_groups) {
        if (scope.mutes && group.active != from.active) || (scope.names && group.name != from.name)
        {
            return true;
        }
    }
    scope.routing && recalled_outputs(mix, scene) != mix.outputs
}

fn channel_differs(
    channel: &ChannelStrip,
    from: &ChannelStrip,
    scope: &RecallScope,
    buses_now: &[Id],
    scene_buses: &[Id],
) -> bool {
    if scope.input
        && (channel.trim_db != from.trim_db
            || channel.phase_invert != from.phase_invert
            || channel.input != from.input)
    {
        return true;
    }
    if scope.names && channel.name != from.name {
        return true;
    }
    if scope.routing && recalled_output(channel.output, from.output, buses_now) != channel.output {
        return true;
    }
    if scope.sends {
        // The sends a recall would leave, compared without building them.
        let stored = from.sends.iter().filter(|s| buses_now.contains(&s.bus));
        let extra = channel
            .sends
            .iter()
            .filter(|s| !scene_buses.contains(&s.bus));
        let expected = stored.clone().count() + extra.clone().count();
        if expected != channel.sends.len()
            || stored.chain(extra).zip(&channel.sends).any(|(a, b)| a != b)
        {
            return true;
        }
    }
    core_differs(&channel.core, &from.core, scope)
}

fn bus_differs(bus: &BusStrip, from: &BusStrip, scope: &RecallScope) -> bool {
    (scope.names && bus.name != from.name)
        || (scope.routing
            && !matches!(from.output, StripOutput::Bus(_))
            && bus.output != from.output)
        || core_differs(&bus.core, &from.core, scope)
}

fn matrix_differs(
    matrix: &MatrixStrip,
    from: &MatrixStrip,
    scope: &RecallScope,
    buses_now: &[Id],
    scene_buses: &[Id],
) -> bool {
    if scope.names && matrix.name != from.name {
        return true;
    }
    if scope.sends {
        // The sources a recall would leave, compared without building them.
        let stored = from
            .sources
            .iter()
            .filter(|s| feed_exists(s.source, buses_now));
        let extra = matrix
            .sources
            .iter()
            .filter(|s| !feed_known(s.source, scene_buses));
        let expected = stored.clone().count() + extra.clone().count();
        if expected != matrix.sources.len()
            || stored
                .chain(extra)
                .zip(&matrix.sources)
                .any(|(a, b)| a != b)
        {
            return true;
        }
    }
    core_differs(&matrix.core, &from.core, scope)
}

fn core_differs(core: &StripCore, from: &StripCore, scope: &RecallScope) -> bool {
    (scope.processing && core.processing != from.processing)
        || (scope.faders && core.fader_db != from.fader_db)
        || (scope.mutes && core.mute != from.mute)
        || (scope.pan && core.pan != from.pan)
        || (scope.assign && (core.dcas != from.dcas || core.mute_groups != from.mute_groups))
        || (scope.names && core.color != from.color)
        || (scope.inserts
            && core
                .inserts
                .iter()
                .any(|slot| insert_differs(slot, &from.inserts)))
}

/// An insert present in both whose bypass or built-in parameters differ.
/// Inserts on one side only, and third-party state, are not recalled, so
/// they do not count.
fn insert_differs(slot: &InsertSlot, stored: &[InsertSlot]) -> bool {
    let Some(stored) = stored.iter().find(|s| s.id == slot.id) else {
        return false;
    };
    if !same_plugin(&slot.plugin, &stored.plugin) {
        return false;
    }
    if slot.bypass != stored.bypass {
        return true;
    }
    match (&slot.plugin, &stored.plugin) {
        (InsertPlugin::Builtin { params: a, .. }, InsertPlugin::Builtin { params: b, .. }) => {
            a != b
        }
        _ => false,
    }
}
