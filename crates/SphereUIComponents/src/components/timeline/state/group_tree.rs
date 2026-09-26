//! Folder (group) tracks as a tree.
//!
//! Membership is stored on the member: `parent_group_id` names the Group track
//! it sits in, and a Group may itself sit in another Group, to any depth. In
//! `tracks` every group is followed directly by its members, their members
//! after them, so a group and everything inside it is one contiguous block.
//! [`GroupTree`] reads that structure once, for a whole frame or a whole edit,
//! instead of each row walking the list again.
//!
//! A folder's members play through it: a member whose output is the main mix
//! (or its old folder) is routed to its folder, so the folder's fader, pan,
//! mute and inserts act on everything inside it, and a folder inside a folder
//! passes on through the outer one ([`folder_output`]).

use super::*;
use std::collections::HashMap;

/// The group structure of a track list, indexed like it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupTree {
    /// Each track's folder, when it names a Group that is really its
    /// ancestor (never itself, never a loop).
    pub parent: Vec<Option<usize>>,
    /// How many folders enclose each track.
    pub depth: Vec<usize>,
    /// Whether a collapsed folder encloses each track.
    pub hidden: Vec<bool>,
}

impl GroupTree {
    pub fn of(tracks: &[TrackState]) -> Self {
        let index_of: HashMap<&str, usize> = tracks
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id.as_str(), index))
            .collect();
        let named_parent: Vec<Option<usize>> = tracks
            .iter()
            .enumerate()
            .map(|(index, track)| {
                let parent = *index_of.get(track.parent_group_id.as_deref()?)?;
                (parent != index && tracks[parent].track_type == TrackType::Group).then_some(parent)
            })
            .collect();
        let mut tree = Self {
            parent: vec![None; tracks.len()],
            depth: vec![0; tracks.len()],
            hidden: vec![false; tracks.len()],
        };
        for index in 0..tracks.len() {
            // Walk up at most once around the list: a chain longer than that
            // is a loop a hand-edited project could hold, and a track in a
            // loop is treated as top level rather than hanging the UI.
            let mut chain = Vec::new();
            let mut at = named_parent[index];
            while let Some(parent) = at {
                if parent == index || chain.contains(&parent) || chain.len() >= tracks.len() {
                    chain.clear();
                    break;
                }
                chain.push(parent);
                at = named_parent[parent];
            }
            tree.parent[index] = chain.first().copied();
            tree.depth[index] = chain.len();
            tree.hidden[index] = chain.iter().any(|&group| tracks[group].group_collapsed);
        }
        tree
    }

    /// Whether `ancestor` encloses `index`, at any depth.
    pub fn is_inside(&self, index: usize, ancestor: usize) -> bool {
        let mut at = self.parent.get(index).copied().flatten();
        let mut steps = 0;
        while let Some(parent) = at {
            if parent == ancestor {
                return true;
            }
            steps += 1;
            if steps > self.parent.len() {
                return false;
            }
            at = self.parent[parent];
        }
        false
    }

    /// One past the last track of the block that starts at `index`: the
    /// track itself and, for a folder, everything inside it that follows it.
    pub fn block_end(&self, index: usize) -> usize {
        let mut end = index + 1;
        while end < self.parent.len() && self.is_inside(end, index) {
            end += 1;
        }
        end
    }

    /// The folders enclosing `index`, innermost first.
    pub fn ancestors(&self, index: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut at = self.parent.get(index).copied().flatten();
        while let Some(parent) = at {
            if out.contains(&parent) {
                break;
            }
            out.push(parent);
            at = self.parent[parent];
        }
        out
    }

    /// Tracks directly inside the folder at `index`.
    pub fn member_count(&self, index: usize) -> usize {
        self.parent
            .iter()
            .filter(|parent| **parent == Some(index))
            .count()
    }
}

/// The output a folder member plays through: its folder, or the main mix at
/// the top level.
pub fn folder_output(parent_group_id: Option<&str>) -> TrackOutputRouting {
    match parent_group_id {
        Some(group_id) => TrackOutputRouting::Bus {
            bus_id: group_id.to_string(),
        },
        None => TrackOutputRouting::Main,
    }
}

impl TimelineState {
    pub fn group_tree(&self) -> GroupTree {
        GroupTree::of(&self.tracks)
    }

    /// Puts `track_id` (with everything inside it, for a folder) into the
    /// folder `group_id`, as its last member. Refuses a folder into itself or
    /// into a folder inside it. Returns whether anything changed.
    pub fn assign_track_to_group(&mut self, track_id: &str, group_id: &str) -> bool {
        let tree = self.group_tree();
        let Some(track_index) = self.tracks.iter().position(|track| track.id == track_id) else {
            return false;
        };
        let Some(group_index) = self
            .tracks
            .iter()
            .position(|track| track.id == group_id && track.track_type == TrackType::Group)
        else {
            return false;
        };
        if track_index == group_index || tree.is_inside(group_index, track_index) {
            return false;
        }
        let insert_at = tree.block_end(group_index);
        let changed = self.move_track_block(track_index, insert_at, Some(group_id), &tree);
        self.clear_track_drag();
        changed
    }

    /// Takes `track_id` out of its folder, one level up: it lands right after
    /// that folder's block, inside the folder's own folder if it has one.
    pub fn remove_track_from_group(&mut self, track_id: &str) -> bool {
        let tree = self.group_tree();
        let Some(index) = self.tracks.iter().position(|track| track.id == track_id) else {
            return false;
        };
        let Some(folder) = tree.parent[index] else {
            return false;
        };
        let outer = tree.parent[folder].map(|outer| self.tracks[outer].id.clone());
        let insert_at = tree.block_end(folder);
        self.move_track_block(index, insert_at, outer.as_deref(), &tree)
    }

    /// Moves the track at `index` — with everything inside it, for a folder —
    /// to `insert_at` (an index into the list as it is now), in the folder
    /// the position lands in. Returns whether anything changed.
    ///
    /// The folder a position belongs to is the one of the row below it, so
    /// dropping between two members of a folder joins it, and dropping below
    /// a folder's last member leaves it. Hidden rows of a collapsed folder
    /// are skipped first: a position among them is past that folder.
    pub fn move_track_to(&mut self, index: usize, insert_at: usize) -> bool {
        let tree = self.group_tree();
        if index >= self.tracks.len() {
            return false;
        }
        let end = tree.block_end(index);
        let mut insert_at = insert_at.min(self.tracks.len());
        // Into its own block is no move at all (and would put a folder inside
        // itself).
        if insert_at > index && insert_at <= end {
            return false;
        }
        while insert_at < self.tracks.len() && tree.hidden[insert_at] {
            insert_at += 1;
        }
        let parent = self
            .tracks
            .get(insert_at)
            .and_then(|_| tree.parent[insert_at])
            .map(|parent| self.tracks[parent].id.clone());
        self.move_track_block(index, insert_at, parent.as_deref(), &tree)
    }

    /// The one move every folder edit makes: the block starting at `index`
    /// goes to `insert_at` with `parent` as its folder, and the block's head
    /// follows its folder's output if it was playing through its old one.
    fn move_track_block(
        &mut self,
        index: usize,
        insert_at: usize,
        parent: Option<&str>,
        tree: &GroupTree,
    ) -> bool {
        let end = tree.block_end(index);
        let before: Vec<String> = self.tracks.iter().map(|track| track.id.clone()).collect();
        let old_parent = self.tracks[index].parent_group_id.clone();
        let block: Vec<TrackState> = self.tracks.drain(index..end).collect();
        let insert_at = if insert_at > index {
            insert_at.saturating_sub(end - index)
        } else {
            insert_at
        }
        .min(self.tracks.len());
        self.tracks.splice(insert_at..insert_at, block);
        let head = &mut self.tracks[insert_at];
        head.parent_group_id = parent.map(str::to_string);
        follow_folder_output(head, old_parent.as_deref());
        let moved = before
            != self
                .tracks
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>();
        moved || old_parent.as_deref() != parent
    }

    /// Makes a new folder around the selected tracks, where the first of them
    /// is, and returns its id. Tracks inside a selected folder go with it.
    pub fn group_selected_tracks(&mut self, name: String) -> Option<String> {
        let tree = self.group_tree();
        let selected = self.selected_range_track_ids();
        let mut heads: Vec<usize> = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(index, track)| {
                selected.contains(&track.id)
                    && !is_arrangement_hidden_track(track)
                    && !tree
                        .ancestors(*index)
                        .iter()
                        .any(|ancestor| selected.contains(&self.tracks[*ancestor].id))
            })
            .map(|(index, _)| index)
            .collect();
        heads.sort_unstable();
        let first = *heads.first()?;
        let outer = tree.parent[first].map(|outer| self.tracks[outer].id.clone());
        let color = self.tracks[first].color;
        let group_id = self.create_track(CreateTrackOptions {
            track_type: TrackType::Group,
            name,
            color,
            volume: volume::db_to_norm(0.0),
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        // `create_track` appends; the folder goes where its first member was.
        let group = self.tracks.pop()?;
        self.tracks.insert(first, group);
        self.tracks[first].parent_group_id = outer.clone();
        self.tracks[first].routing.output = folder_output(outer.as_deref());
        let ids: Vec<String> = heads
            .iter()
            .map(|&index| {
                let shifted = if index >= first { index + 1 } else { index };
                self.tracks[shifted].id.clone()
            })
            .collect();
        for id in ids {
            self.assign_track_to_group(&id, &group_id);
        }
        self.select_track(&group_id);
        Some(group_id)
    }

    /// Dissolves the folder `group_id`: its members move up into the folder's
    /// own folder, in place, and the folder track itself goes.
    pub fn ungroup(&mut self, group_id: &str) -> bool {
        let is_group = self
            .tracks
            .iter()
            .any(|track| track.id == group_id && track.track_type == TrackType::Group);
        if is_group {
            self.delete_track(group_id);
        }
        is_group
    }

    /// The folder a track drag at `viewport_y` drops into: the one whose row
    /// the pointer is over, in the middle half of it. The top and bottom
    /// quarters stay a reorder, so a track can still be dropped just above or
    /// below a folder. Never the dragged track itself, or a folder inside it.
    pub fn folder_drop_target_at_y(&self, viewport_y: f32) -> Option<String> {
        let dragged = self.dragging_track_id.as_deref()?;
        let content_y = viewport_y + self.viewport.scroll_y;
        let layout = self.track_row_layout();
        let row = layout.track_at_content_y(content_y)?;
        let folder = self.tracks.get(row.index)?;
        if folder.track_type != TrackType::Group {
            return None;
        }
        let within = content_y - row.y;
        if within < row.height * 0.25 || within > row.height * 0.75 {
            return None;
        }
        let dragged_index = self.tracks.iter().position(|track| track.id == dragged)?;
        let tree = self.group_tree();
        if row.index == dragged_index || tree.is_inside(row.index, dragged_index) {
            return None;
        }
        Some(folder.id.clone())
    }

    /// Everything inside the folder `group_id`, at any depth, in list order.
    pub fn group_member_ids(&self, group_id: &str) -> Vec<String> {
        let tree = self.group_tree();
        let Some(index) = self.tracks.iter().position(|track| track.id == group_id) else {
            return Vec::new();
        };
        (index + 1..tree.block_end(index))
            .map(|member| self.tracks[member].id.clone())
            .collect()
    }
}

/// A block head that played through its old folder (or the main mix) plays
/// through its new one; one routed anywhere else keeps that.
pub(crate) fn follow_folder_output(track: &mut TrackState, old_parent: Option<&str>) {
    if !track_plays_audio_out(track) {
        return;
    }
    let follows = match &track.routing.output {
        TrackOutputRouting::Main => true,
        TrackOutputRouting::Bus { bus_id } => old_parent == Some(bus_id.as_str()),
        _ => false,
    };
    if follows {
        track.routing.output = folder_output(track.parent_group_id.as_deref());
    }
}

/// Whether a track's output is an audio destination a folder can take over:
/// not the master, and not a MIDI track whose output is a MIDI port.
fn track_plays_audio_out(track: &TrackState) -> bool {
    !matches!(track.track_type, TrackType::Master | TrackType::Midi)
        && !is_vsti_output_child_track_id(&track.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(tracks: &[(&str, TrackType, Option<&str>)]) -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        for (id, kind, parent) in tracks {
            let created = state.create_track(CreateTrackOptions {
                track_type: *kind,
                name: id.to_string(),
                color: gpui::rgba(0x336699ff),
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
            let track = state.tracks.last_mut().unwrap();
            track.id = id.to_string();
            track.parent_group_id = parent.map(str::to_string);
            let _ = created;
        }
        state
    }

    fn order(state: &TimelineState) -> Vec<&str> {
        state.tracks.iter().map(|track| track.id.as_str()).collect()
    }

    fn parent<'a>(state: &'a TimelineState, id: &str) -> Option<&'a str> {
        state
            .tracks
            .iter()
            .find(|track| track.id == id)
            .and_then(|track| track.parent_group_id.as_deref())
    }

    fn output(state: &TimelineState, id: &str) -> TrackOutputRouting {
        state
            .tracks
            .iter()
            .find(|track| track.id == id)
            .unwrap()
            .routing
            .output
            .clone()
    }

    #[test]
    fn the_tree_reads_depth_hidden_rows_and_blocks() {
        let mut state = state_with(&[
            ("outer", TrackType::Group, None),
            ("inner", TrackType::Group, Some("outer")),
            ("a", TrackType::Audio, Some("inner")),
            ("b", TrackType::Audio, Some("outer")),
            ("c", TrackType::Audio, None),
        ]);
        state.tracks[1].group_collapsed = true;
        let tree = state.group_tree();
        assert_eq!(tree.depth, [0, 1, 2, 1, 0]);
        assert_eq!(tree.hidden, [false, false, true, false, false]);
        assert_eq!(tree.block_end(0), 4);
        assert_eq!(tree.block_end(1), 3);
        assert_eq!(tree.ancestors(2), [1, 0]);
        assert_eq!(tree.member_count(0), 2);
    }

    #[test]
    fn a_loop_in_a_loaded_project_is_read_as_top_level() {
        let state = state_with(&[
            ("g1", TrackType::Group, Some("g2")),
            ("g2", TrackType::Group, Some("g1")),
            ("self", TrackType::Group, Some("self")),
        ]);
        let tree = state.group_tree();
        assert_eq!(tree.depth, [0, 0, 0]);
        assert_eq!(tree.parent, [None, None, None]);
    }

    #[test]
    fn a_folder_goes_into_a_folder_with_everything_inside_it() {
        let mut state = state_with(&[
            ("outer", TrackType::Group, None),
            ("x", TrackType::Audio, Some("outer")),
            ("inner", TrackType::Group, None),
            ("a", TrackType::Audio, Some("inner")),
        ]);
        assert!(state.assign_track_to_group("inner", "outer"));
        assert_eq!(order(&state), ["outer", "x", "inner", "a"]);
        assert_eq!(parent(&state, "inner"), Some("outer"));
        assert_eq!(parent(&state, "a"), Some("inner"));
        assert_eq!(
            output(&state, "inner"),
            TrackOutputRouting::Bus {
                bus_id: "outer".to_string()
            }
        );
        // Never into itself or a folder inside it.
        assert!(!state.assign_track_to_group("outer", "inner"));
        assert!(!state.assign_track_to_group("outer", "outer"));
    }

    #[test]
    fn a_member_plays_through_its_folder_and_leaves_it_one_level_up() {
        let mut state = state_with(&[
            ("outer", TrackType::Group, None),
            ("inner", TrackType::Group, Some("outer")),
            ("a", TrackType::Audio, None),
        ]);
        state.tracks[2].routing.output = TrackOutputRouting::Main;
        assert!(state.assign_track_to_group("a", "inner"));
        assert_eq!(
            output(&state, "a"),
            TrackOutputRouting::Bus {
                bus_id: "inner".to_string()
            }
        );
        assert!(state.remove_track_from_group("a"));
        assert_eq!(parent(&state, "a"), Some("outer"));
        assert_eq!(
            output(&state, "a"),
            TrackOutputRouting::Bus {
                bus_id: "outer".to_string()
            }
        );
        assert!(state.remove_track_from_group("a"));
        assert_eq!(parent(&state, "a"), None);
        assert_eq!(output(&state, "a"), TrackOutputRouting::Main);
    }

    #[test]
    fn a_member_routed_to_a_bus_keeps_that_route() {
        let mut state = state_with(&[
            ("g", TrackType::Group, None),
            ("verb", TrackType::Bus, None),
            ("a", TrackType::Audio, None),
        ]);
        let to_verb = TrackOutputRouting::Bus {
            bus_id: "verb".to_string(),
        };
        state.tracks[2].routing.output = to_verb.clone();
        assert!(state.assign_track_to_group("a", "g"));
        assert_eq!(output(&state, "a"), to_verb);
    }

    #[test]
    fn dropping_between_members_joins_and_below_the_last_leaves() {
        let mut state = state_with(&[
            ("g", TrackType::Group, None),
            ("m1", TrackType::Audio, Some("g")),
            ("m2", TrackType::Audio, Some("g")),
            ("t", TrackType::Audio, None),
            ("u", TrackType::Audio, None),
        ]);
        // `u` dropped between m1 and m2.
        assert!(state.move_track_to(4, 2));
        assert_eq!(order(&state), ["g", "m1", "u", "m2", "t"]);
        assert_eq!(parent(&state, "u"), Some("g"));
        // m1 dropped below t: out of the folder.
        assert!(state.move_track_to(1, 5));
        assert_eq!(order(&state), ["g", "u", "m2", "t", "m1"]);
        assert_eq!(parent(&state, "m1"), None);
    }

    #[test]
    fn a_folder_cannot_be_dropped_inside_itself() {
        let mut state = state_with(&[
            ("g", TrackType::Group, None),
            ("m1", TrackType::Audio, Some("g")),
            ("m2", TrackType::Audio, Some("g")),
        ]);
        assert!(!state.move_track_to(0, 2));
        assert_eq!(order(&state), ["g", "m1", "m2"]);
    }

    #[test]
    fn a_drop_among_a_collapsed_folders_hidden_rows_lands_past_it() {
        let mut state = state_with(&[
            ("t", TrackType::Audio, None),
            ("g", TrackType::Group, None),
            ("m1", TrackType::Audio, Some("g")),
            ("u", TrackType::Audio, None),
        ]);
        state.tracks[1].group_collapsed = true;
        assert!(state.move_track_to(0, 2));
        assert_eq!(order(&state), ["g", "m1", "t", "u"]);
        assert_eq!(parent(&state, "t"), None);
    }

    #[test]
    fn grouping_the_selection_makes_a_folder_where_it_was() {
        let mut state = state_with(&[
            ("t", TrackType::Audio, None),
            ("a", TrackType::Audio, None),
            ("b", TrackType::Audio, None),
            ("c", TrackType::Audio, None),
        ]);
        state.select_track("a");
        state.selection.selected_track_ids = vec!["a".to_string(), "c".to_string()];
        let group = state.group_selected_tracks("Folder".to_string()).unwrap();
        assert_eq!(order(&state), ["t", group.as_str(), "a", "c", "b"]);
        assert_eq!(parent(&state, "a"), Some(group.as_str()));
        assert_eq!(parent(&state, "c"), Some(group.as_str()));
        assert_eq!(parent(&state, "b"), None);
    }

    #[test]
    fn ungrouping_moves_members_up_a_level() {
        let mut state = state_with(&[
            ("outer", TrackType::Group, None),
            ("inner", TrackType::Group, Some("outer")),
            ("a", TrackType::Audio, Some("inner")),
        ]);
        state.tracks[2].routing.output = folder_output(Some("inner"));
        assert!(state.ungroup("inner"));
        assert_eq!(order(&state), ["outer", "a"]);
        assert_eq!(parent(&state, "a"), Some("outer"));
        assert_eq!(output(&state, "a"), folder_output(Some("outer")));
    }
}
