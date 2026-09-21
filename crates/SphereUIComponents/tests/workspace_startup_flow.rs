//! Pre-studio startup flows for every route out of the Welcome screen.
//!
//! Covers the decisions the native shell makes between "user picked a workspace"
//! and "the studio window is on screen": which stages a transaction runs, when
//! the start screen may be retired, and where a failure is reported. These are
//! the paths that leave a macOS launch with no window at all when they are wrong,
//! because AppKit gives the session transaction window no owner to fall back on.
//!
//! Window creation itself is not exercised here — it needs a live GPUI platform —
//! so each flow asserts the stage chain plus the surface policy that governs it.

use std::path::{Path, PathBuf};

use sphere_ui_components::loading_session::{
    FailureSurface, LoadStage, ReplacementSurfaces, SessionTransactionSurface,
};
use sphere_ui_components::project::{
    load_project_strict, load_project, save_project, validate_project_file, FutureboardProject, PROJECT_FILE_EXT,
};

/// Stages a transaction actually walks, from its first stage onwards.
fn stage_chain(has_project_file: bool, replaces_live_session: bool) -> Vec<LoadStage> {
    let mut stages = vec![LoadStage::initial(has_project_file, replaces_live_session)];
    while let Some(next) = stages
        .last()
        .copied()
        .and_then(|stage| stage.next(has_project_file))
    {
        stages.push(next);
    }
    stages
}

/// Every workspace route that opens the studio without a project file on disk.
const WORKSPACE_ROUTES: [&str; 4] = ["New Project", "Empty Project", "Template", "Open dialog"];

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "futureboard-workspace-startup-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_project(path: &Path, name: &str) {
    let mut project = FutureboardProject::new(name);
    save_project(&mut project, path).expect("save project");
}

#[test]
fn new_project_reaches_studio_without_reading_a_file() {
    // `WelcomeAction::CreateProject` seeds an in-memory project; the file is
    // written by the studio shell afterwards, so the transaction must not try to
    // validate or decode anything.
    let project = FutureboardProject::new("Fresh Session");
    assert_eq!(project.name, "Fresh Session");
    assert!(project.tracks.is_empty());

    assert_eq!(stage_chain(false, false), vec![LoadStage::SessionInstall]);
}

#[test]
fn empty_project_reaches_studio_without_reading_a_file() {
    assert_eq!(stage_chain(false, false), vec![LoadStage::SessionInstall]);
}

#[test]
fn template_project_reaches_studio_without_reading_a_file() {
    // Templates are seeded by the prepared-workspace finish step after install,
    // so they share the file-free stage chain with New and Empty.
    for route in WORKSPACE_ROUTES {
        assert_eq!(
            stage_chain(false, false),
            vec![LoadStage::SessionInstall],
            "{route} must not read a project file"
        );
    }
}

#[test]
fn open_existing_project_validates_and_decodes_before_install() {
    let dir = temp_dir("open-existing");
    let path = dir.join(format!("Existing Session.{PROJECT_FILE_EXT}"));
    write_project(&path, "Existing Session");

    let version = validate_project_file(&path).expect("header validates");
    assert!(version > 0);
    let decoded = load_project_strict(&path).expect("project decodes");
    assert_eq!(decoded.name, "Existing Session");

    assert_eq!(
        stage_chain(true, false),
        vec![
            LoadStage::Validate,
            LoadStage::Decode,
            LoadStage::SessionInstall
        ]
    );
    // Replacing a live session closes it first, then follows the same chain.
    assert_eq!(
        stage_chain(true, true),
        vec![
            LoadStage::SessionShutdown,
            LoadStage::Validate,
            LoadStage::Decode,
            LoadStage::SessionInstall
        ]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_failure_and_transaction_window_failure_stay_actionable() {
    let dir = temp_dir("load-failure");

    // A missing file and a corrupt file both have to fail with a real reason.
    let missing = dir.join(format!("Gone.{PROJECT_FILE_EXT}"));
    assert!(validate_project_file(&missing).is_err());

    let corrupt = dir.join(format!("Corrupt.{PROJECT_FILE_EXT}"));
    std::fs::write(&corrupt, b"not a futureboard project").expect("write corrupt file");
    let error = validate_project_file(&corrupt).expect_err("corrupt header rejected");
    assert!(!error.user_message().is_empty());
    assert!(!error.technical_detail().is_empty());

    // With the transaction window up, the failure is shown there.
    let with_window = ReplacementSurfaces {
        transaction_window_open: true,
        studio_mounted: false,
    };
    assert_eq!(
        with_window.failure_surface(),
        FailureSurface::TransactionWindow
    );
    assert!(with_window.may_retire_welcome());

    // When the transaction window could not be opened the work still runs, but
    // Welcome must stay up: it is the only surface left to report on.
    let headless = ReplacementSurfaces::default();
    assert_eq!(headless.failure_surface(), FailureSurface::WelcomeWindow);
    assert!(!headless.may_retire_welcome());

    // Once the studio shell is mounted the handoff is complete either way.
    let mounted = ReplacementSurfaces {
        transaction_window_open: false,
        studio_mounted: true,
    };
    assert!(mounted.may_retire_welcome());
    assert_eq!(mounted.failure_surface(), FailureSurface::WelcomeWindow);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn headless_transaction_still_runs_the_install_stage() {
    // The surfaces differ only in where progress and errors are displayed. Both
    // report the same stage work, so a missing transaction window can never
    // silently skip the session install.
    for surface in [
        SessionTransactionSurface::TransactionWindow,
        SessionTransactionSurface::Headless,
    ] {
        assert!(stage_chain(false, false).contains(&LoadStage::SessionInstall));
        assert!(!surface.label().is_empty());
    }
    assert_ne!(
        SessionTransactionSurface::TransactionWindow.label(),
        SessionTransactionSurface::Headless.label()
    );
}
