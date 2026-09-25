use std::path::PathBuf;

use super::{new_id, now_secs};

/// Canonical in-memory model for the project currently loaded in a studio
/// workspace. All UI chrome, save/open commands, and engine sync should read
/// from this struct (via [`StudioLayout::sync_project_session_to_workspace`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSession {
    pub id: String,
    pub name: String,
    pub folder_path: Option<PathBuf>,
    pub project_file_path: Option<PathBuf>,
    pub is_untitled: bool,
    pub is_dirty: bool,
    /// Bumped by every [`Self::mark_dirty`]. A save captures it with its
    /// snapshot and only marks the session clean when it is unchanged on
    /// completion, so an edit made while a background save runs is never
    /// reported as saved.
    pub dirty_generation: u64,
    pub created_at: u64,
    pub modified_at: u64,
}

impl Default for ProjectSession {
    fn default() -> Self {
        Self::untitled()
    }
}

impl ProjectSession {
    pub fn fresh_id() -> String {
        new_id()
    }

    pub fn untitled() -> Self {
        let now = now_secs();
        Self {
            id: new_id(),
            name: "Untitled Project".to_string(),
            folder_path: None,
            project_file_path: None,
            is_untitled: true,
            is_dirty: false,
            dirty_generation: 0,
            created_at: now,
            modified_at: now,
        }
    }

    pub fn bind_saved(
        &mut self,
        id: String,
        name: String,
        folder_path: Option<PathBuf>,
        project_file_path: PathBuf,
        created_at: u64,
        modified_at: u64,
    ) {
        self.id = id;
        self.name = name;
        self.folder_path = folder_path;
        self.project_file_path = Some(project_file_path);
        self.is_untitled = false;
        self.is_dirty = false;
        self.created_at = created_at;
        self.modified_at = modified_at;
    }

    /// Bind the file a save just wrote, where the save's snapshot was taken at
    /// `saved_generation`. The session only becomes clean when nothing was
    /// edited since that snapshot; otherwise it stays dirty so the later edits
    /// still prompt and still autosave. Returns `true` when the session is clean.
    #[allow(clippy::too_many_arguments)]
    pub fn bind_saved_snapshot(
        &mut self,
        id: String,
        name: String,
        folder_path: Option<PathBuf>,
        project_file_path: PathBuf,
        created_at: u64,
        modified_at: u64,
        saved_generation: u64,
    ) -> bool {
        let edited_since_snapshot = self.dirty_generation != saved_generation;
        self.bind_saved(
            id,
            name,
            folder_path,
            project_file_path,
            created_at,
            modified_at,
        );
        self.is_dirty = edited_since_snapshot;
        !edited_since_snapshot
    }

    pub fn bind_untitled(&mut self, name: impl Into<String>, dirty: bool) {
        let now = now_secs();
        self.id = new_id();
        self.name = name.into();
        self.folder_path = None;
        self.project_file_path = None;
        self.is_untitled = true;
        self.is_dirty = false;
        if dirty {
            self.mark_dirty();
        }
        self.created_at = now;
        self.modified_at = now;
    }

    /// Bind an untitled session recovered from its autosave. It keeps the
    /// autosave's id, so later autosaves overwrite that same recovery file
    /// instead of leaving it behind to be offered again, and it is dirty: the
    /// recovered work exists nowhere but in the autosave.
    pub fn bind_recovered_untitled(
        &mut self,
        id: String,
        name: impl Into<String>,
        created_at: u64,
    ) {
        self.bind_untitled(name, true);
        self.id = id;
        self.created_at = created_at;
    }

    /// Titlebar / window chrome display name.
    pub fn display_name(&self) -> &str {
        if self.is_untitled {
            "Untitled Project"
        } else {
            &self.name
        }
    }

    pub fn needs_save_as(&self) -> bool {
        self.is_untitled || self.project_file_path.is_none()
    }

    pub fn mark_dirty(&mut self) {
        self.is_dirty = true;
        self.dirty_generation = self.dirty_generation.wrapping_add(1);
        self.modified_at = now_secs();
    }

    pub fn mark_clean(&mut self, modified_at: Option<u64>) {
        self.is_dirty = false;
        if let Some(ts) = modified_at {
            self.modified_at = ts;
        }
    }

    pub fn subtitle(&self) -> &'static str {
        if self.is_dirty {
            "Unsaved changes"
        } else if self.is_untitled {
            "New project"
        } else {
            "Saved"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_untitled() {
        let session = ProjectSession::untitled();
        assert_eq!(session.display_name(), "Untitled Project");
        assert!(session.needs_save_as());
    }

    #[test]
    fn bind_saved_clears_untitled() {
        let mut session = ProjectSession::untitled();
        let path = PathBuf::from("/tmp/Test Song/Test Song.fbproj");
        session.bind_saved(
            "id-1".to_string(),
            "Test Song".to_string(),
            Some(PathBuf::from("/tmp/Test Song")),
            path.clone(),
            1,
            2,
        );
        assert_eq!(session.name, "Test Song");
        assert_eq!(session.project_file_path.as_ref(), Some(&path));
        assert!(!session.is_untitled);
        assert!(!session.is_dirty);
        assert!(!session.needs_save_as());
        assert_eq!(session.display_name(), "Test Song");
    }

    #[test]
    fn save_as_from_untitled_then_direct_save() {
        let mut session = ProjectSession::untitled();
        assert!(session.needs_save_as());
        session.bind_saved(
            "id-2".to_string(),
            "My Project".to_string(),
            Some(PathBuf::from("/tmp/My Project")),
            PathBuf::from("/tmp/My Project/My Project.fbproj"),
            10,
            10,
        );
        assert_eq!(session.name, "My Project");
        assert!(!session.is_untitled);
        assert!(!session.needs_save_as());
    }

    #[test]
    fn dirty_state_round_trip() {
        let mut session = ProjectSession::untitled();
        session.bind_saved(
            "id-3".to_string(),
            "Beat Demo".to_string(),
            Some(PathBuf::from("/tmp/Beat Demo")),
            PathBuf::from("/tmp/Beat Demo/Beat Demo.fbproj"),
            1,
            1,
        );
        session.mark_dirty();
        assert!(session.is_dirty);
        session.mark_clean(Some(99));
        assert!(!session.is_dirty);
        assert_eq!(session.modified_at, 99);
    }

    fn bind_snapshot(session: &mut ProjectSession, generation: u64) -> bool {
        session.bind_saved_snapshot(
            "id-4".to_string(),
            "Race".to_string(),
            Some(PathBuf::from("/tmp/Race")),
            PathBuf::from("/tmp/Race/Race.fbproj"),
            1,
            2,
            generation,
        )
    }

    /// A background save snapshots the project, writes it off the UI thread,
    /// then binds the result. An edit that lands in between is not in the file,
    /// so the session must stay dirty instead of reporting it as saved.
    #[test]
    fn an_edit_during_a_save_keeps_the_session_dirty() {
        let mut session = ProjectSession::untitled();
        session.mark_dirty();
        let snapshot_generation = session.dirty_generation;
        session.mark_dirty();
        assert!(!bind_snapshot(&mut session, snapshot_generation));
        assert!(session.is_dirty);
        assert_eq!(session.subtitle(), "Unsaved changes");
        assert!(!session.needs_save_as(), "the file is still bound");
    }

    #[test]
    fn a_save_with_no_later_edits_marks_the_session_clean() {
        let mut session = ProjectSession::untitled();
        session.mark_dirty();
        let snapshot_generation = session.dirty_generation;
        assert!(bind_snapshot(&mut session, snapshot_generation));
        assert!(!session.is_dirty);
        assert_eq!(session.subtitle(), "Saved");
    }

    #[test]
    fn binding_a_dirty_untitled_session_counts_as_an_edit() {
        let mut session = ProjectSession::untitled();
        let before = session.dirty_generation;
        session.bind_untitled("Imported", true);
        assert!(session.is_dirty);
        assert_ne!(session.dirty_generation, before);
        let generation = session.dirty_generation;
        session.bind_untitled("Clean", false);
        assert!(!session.is_dirty);
        assert_eq!(session.dirty_generation, generation);
    }

    #[test]
    fn a_recovered_untitled_session_keeps_its_autosave_id_and_is_dirty() {
        let mut session = ProjectSession::untitled();
        session.bind_recovered_untitled("autosave-id".to_string(), "Untitled Project", 7);
        assert_eq!(session.id, "autosave-id");
        assert_eq!(session.created_at, 7);
        assert!(session.is_dirty);
        assert!(session.needs_save_as());
    }
}
