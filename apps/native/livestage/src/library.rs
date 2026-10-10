//! The strip library: settings saved from a strip, to apply to others later
//! — in this show or the next. A server-side file (`--library FILE`, by
//! default `library.json` next to the session), written atomically like the
//! session (`FILE.tmp` renamed over it, `FILE.bak` the one before), plus a
//! few read-only factory items built into the binary.
//!
//! An item carries [`StripSettings`] and the [`Section`]s it applies; applying
//! it is the engine's `paste_strip` on the chosen strips.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use livestage_engine::processing::{
    Comp, Delay, Eq, EqBand, EqKind, Gate, Hpf, Processing, ProcessingOrder,
};
use livestage_engine::{Command, Section, StripRef, StripSettings, clamp_processing, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The longest name or category kept, in characters.
const MAX_LABEL: usize = 64;
/// The category of an item saved without one.
const DEFAULT_CATEGORY: &str = "Other";
/// The file's format.
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryItem {
    pub id: String,
    pub name: String,
    pub category: String,
    pub sections: Vec<Section>,
    pub settings: StripSettings,
    /// Built into the binary: never renamed, deleted or written to the file.
    #[serde(default)]
    pub factory: bool,
}

#[derive(Serialize, Deserialize)]
struct LibraryFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    items: Vec<LibraryItem>,
}

pub struct Library {
    /// Where the user's items live; `None`: in memory only.
    path: Option<PathBuf>,
    factory: Vec<LibraryItem>,
    user: Vec<LibraryItem>,
    /// Bumps on every change, for the `library` push.
    revision: u64,
}

impl Library {
    /// The library kept in `path` (read now; a missing file is an empty
    /// library), or in memory only. Returns what to log.
    pub fn open(path: Option<PathBuf>) -> (Self, Vec<String>) {
        let mut log = Vec::new();
        let user = match &path {
            Some(path) => read_items(path, &mut log),
            None => Vec::new(),
        };
        let mut library = Self {
            path,
            factory: factory_items(),
            user: Vec::new(),
            revision: 0,
        };
        library.user = library.sanitized(user);
        (library, log)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Factory items first, then the user's, each in the order saved.
    pub fn items(&self) -> impl Iterator<Item = &LibraryItem> {
        self.factory.iter().chain(&self.user)
    }

    pub fn get(&self, id: &str) -> Option<&LibraryItem> {
        self.items().find(|item| item.id == id)
    }

    /// `{"type":"library","items":[…]}`.
    pub fn message(&self) -> Value {
        let items: Vec<&LibraryItem> = self.items().collect();
        json!({"type": "library", "items": items})
    }

    /// Keep `settings` as a new item; returns its id.
    pub fn save(
        &mut self,
        name: &str,
        category: Option<&str>,
        settings: StripSettings,
        sections: Vec<Section>,
    ) -> Result<String, String> {
        let name = label(name).ok_or("the item needs a name")?;
        if sections.is_empty() {
            return Err("choose at least one section to keep".to_string());
        }
        let id = new_id(|id| self.get(id).is_some());
        let mut item = LibraryItem {
            id: id.clone(),
            name,
            category: category
                .and_then(label)
                .unwrap_or_else(|| DEFAULT_CATEGORY.to_string()),
            sections: dedup(sections),
            settings,
            factory: false,
        };
        tidy_settings(&mut item.settings);
        let mut user = self.user.clone();
        user.push(item);
        self.commit(user)?;
        Ok(id)
    }

    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        let index = self.user_index(id)?;
        let mut user = self.user.clone();
        user.remove(index);
        self.commit(user)
    }

    /// A new name, and a new category when given.
    pub fn rename(&mut self, id: &str, name: &str, category: Option<&str>) -> Result<(), String> {
        let index = self.user_index(id)?;
        let name = label(name).ok_or("the item needs a name")?;
        let mut user = self.user.clone();
        user[index].name = name;
        if let Some(category) = category {
            user[index].category = label(category).unwrap_or_else(|| DEFAULT_CATEGORY.to_string());
        }
        self.commit(user)
    }

    /// The engine command that applies item `id` to `targets`.
    pub fn paste_command(&self, id: &str, targets: Vec<StripRef>) -> Result<Command, String> {
        let item = self
            .get(id)
            .ok_or_else(|| format!("no library item {id}"))?;
        if targets.is_empty() {
            return Err("choose the strips to apply it to".to_string());
        }
        Ok(Command::PasteStrip {
            targets,
            settings: item.settings.clone(),
            sections: item.sections.clone(),
        })
    }

    /// The index of a user item, or why it cannot be changed.
    fn user_index(&self, id: &str) -> Result<usize, String> {
        if self.factory.iter().any(|item| item.id == id) {
            return Err("factory items are read-only".to_string());
        }
        self.user
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| format!("no library item {id}"))
    }

    /// Write `user` (when there is a file), then keep it: a failed write
    /// changes nothing.
    fn commit(&mut self, user: Vec<LibraryItem>) -> Result<(), String> {
        if let Some(path) = &self.path {
            let file = LibraryFile {
                version: FILE_VERSION,
                items: user.clone(),
            };
            let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
            write_atomic(path, text.as_bytes())
                .map_err(|error| format!("library not saved: {error}"))?;
        }
        self.user = user;
        self.revision += 1;
        Ok(())
    }

    /// Items read from a file, made fit to keep: never factory, ids unique
    /// (and not a factory id), names and categories present, processing
    /// within range.
    fn sanitized(&self, items: Vec<LibraryItem>) -> Vec<LibraryItem> {
        let mut seen: HashSet<String> = self.factory.iter().map(|i| i.id.clone()).collect();
        let mut out: Vec<LibraryItem> = Vec::with_capacity(items.len());
        for mut item in items {
            item.factory = false;
            if item.id.trim().is_empty() || seen.contains(&item.id) {
                item.id = new_id(|id| seen.contains(id));
            }
            seen.insert(item.id.clone());
            item.name = label(&item.name).unwrap_or_else(|| "Untitled".to_string());
            item.category = label(&item.category).unwrap_or_else(|| DEFAULT_CATEGORY.to_string());
            item.sections = dedup(std::mem::take(&mut item.sections));
            tidy_settings(&mut item.settings);
            out.push(item);
        }
        out
    }
}

/// The user's items in `path`: the file, else its `.bak`. A file that cannot
/// be read is set aside as `FILE.unreadable` rather than written over.
fn read_items(path: &Path, log: &mut Vec<String>) -> Vec<LibraryItem> {
    let parse = |path: &Path| -> Result<Option<Vec<LibraryItem>>, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<LibraryFile>(&text)
                .map(|file| Some(file.items))
                .map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    };
    match parse(path) {
        Ok(Some(items)) => {
            log.push(format!(
                "strip library: {} item(s) from {}",
                items.len(),
                path.display()
            ));
            return items;
        }
        Ok(None) => {
            // Never written, or a crash between the two renames: the backup
            // is the last good one.
            if let Ok(Some(items)) = parse(&sibling(path, "bak")) {
                log.push(format!(
                    "strip library: {} missing, {} item(s) from its backup",
                    path.display(),
                    items.len()
                ));
                return items;
            }
            log.push(format!(
                "strip library: {} (new, written on the first save)",
                path.display()
            ));
            Vec::new()
        }
        Err(error) => {
            log.push(format!("strip library unreadable: {error}"));
            let aside = sibling(path, "unreadable");
            if std::fs::rename(path, &aside).is_ok() {
                log.push(format!("strip library: kept as {}", aside.display()));
            }
            match parse(&sibling(path, "bak")) {
                Ok(Some(items)) => {
                    log.push(format!(
                        "strip library: {} item(s) from the backup",
                        items.len()
                    ));
                    items
                }
                _ => Vec::new(),
            }
        }
    }
}

/// `FILE.ext` next to `FILE` (`library.json` → `library.json.bak`).
pub fn sibling(path: &Path, ext: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

/// `text` trimmed and cut to [`MAX_LABEL`] characters; `None` when empty.
fn label(text: &str) -> Option<String> {
    let text: String = text.trim().chars().take(MAX_LABEL).collect();
    let text = text.trim_end().to_string();
    (!text.is_empty()).then_some(text)
}

fn dedup(sections: Vec<Section>) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::with_capacity(sections.len());
    for section in sections {
        if !out.contains(&section) {
            out.push(section);
        }
    }
    out
}

/// Keep what is saved within the ranges the engine documents.
fn tidy_settings(settings: &mut StripSettings) {
    if let Some(processing) = &mut settings.processing {
        clamp_processing(processing);
    }
}

/// A fresh id, `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, that `taken` does
/// not report as in use. From the time, a counter and the standard library's per-process
/// random hash keys: no dependency for a random number.
fn new_id(taken: impl Fn(&str) -> bool) -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut words = [0u64; 2];
        for (salt, word) in words.iter_mut().enumerate() {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_u128(nanos);
            hasher.write_u64(count);
            hasher.write_u32(std::process::id());
            hasher.write_usize(salt);
            *word = hasher.finish();
        }
        let [a, b] = words;
        let id = format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            a >> 32,
            (a >> 16) & 0xffff,
            a & 0xffff,
            b >> 48,
            b & 0xffff_ffff_ffff
        );
        if !taken(&id) {
            return id;
        }
    }
}

/// The factory items: conservative starting points for the processing
/// section only — they never touch a strip's fader, name, sends or inserts.
/// Every value is inside the range [`Processing`] documents.
pub fn factory_items() -> Vec<LibraryItem> {
    let band = |kind, hz, gain_db, q| EqBand {
        kind,
        hz,
        gain_db,
        q,
    };
    let hpf = |hz, slope_db| Hpf {
        on: true,
        hz,
        slope_db,
    };
    let comp = |threshold_db, ratio, attack_ms, release_ms, makeup_db| Comp {
        on: true,
        threshold_db,
        ratio,
        attack_ms,
        release_ms,
        knee_db: 6.0,
        makeup_db,
    };
    let gate = |threshold_db, range_db, hold_ms, release_ms| Gate {
        on: true,
        threshold_db,
        range_db,
        attack_ms: 0.1,
        hold_ms,
        release_ms,
    };
    let processing = |hpf: Hpf, gate: Gate, bands: [EqBand; 4], comp: Comp| Processing {
        hpf,
        gate,
        eq: Eq { on: true, bands },
        comp,
        delay: Delay::default(),
        order: ProcessingOrder::EqThenComp,
    };
    use EqKind::{Bell, HighShelf, LowShelf};
    let items = [
        (
            "factory-vocal",
            "Vocal",
            "Vocal",
            // HPF under the voice, a little mud out, a little presence and
            // air in, a gentle 3:1 to even the level.
            processing(
                hpf(100.0, 18),
                Gate::default(),
                [
                    band(LowShelf, 100.0, 0.0, 0.7),
                    band(Bell, 300.0, -2.0, 1.0),
                    band(Bell, 3_000.0, 2.0, 1.0),
                    band(HighShelf, 10_000.0, 1.5, 0.7),
                ],
                comp(-18.0, 3.0, 10.0, 120.0, 2.0),
            ),
        ),
        (
            "factory-kick",
            "Kick",
            "Drums",
            // Rumble out, a gate that leaves 40 dB of range (not a hard
            // mute), low end up, the boxy mids down, a little beater click.
            processing(
                hpf(30.0, 12),
                gate(-35.0, -40.0, 50.0, 120.0),
                [
                    band(LowShelf, 70.0, 3.0, 0.7),
                    band(Bell, 350.0, -4.0, 1.4),
                    band(Bell, 3_500.0, 2.0, 1.4),
                    band(HighShelf, 8_000.0, 0.0, 0.7),
                ],
                comp(-12.0, 3.0, 20.0, 100.0, 0.0),
            ),
        ),
        (
            "factory-snare",
            "Snare",
            "Drums",
            // A gentle gate against hi-hat spill (30 dB of range), body,
            // less box, a little crack.
            processing(
                hpf(80.0, 12),
                gate(-30.0, -30.0, 40.0, 150.0),
                [
                    band(Bell, 200.0, 2.0, 1.2),
                    band(Bell, 600.0, -3.0, 1.4),
                    band(Bell, 5_000.0, 2.0, 1.0),
                    band(HighShelf, 10_000.0, 1.0, 0.7),
                ],
                comp(-15.0, 3.0, 10.0, 120.0, 0.0),
            ),
        ),
        (
            "factory-bass-di",
            "Bass DI",
            "Bass",
            // Subsonics out, a little weight, the mud down, string noise
            // softened, a 4:1 to hold the level.
            processing(
                hpf(35.0, 12),
                Gate::default(),
                [
                    band(LowShelf, 80.0, 2.0, 0.7),
                    band(Bell, 250.0, -3.0, 1.2),
                    band(Bell, 700.0, 1.5, 1.0),
                    band(HighShelf, 5_000.0, -2.0, 0.7),
                ],
                comp(-20.0, 4.0, 20.0, 200.0, 2.0),
            ),
        ),
        (
            "factory-acoustic-guitar",
            "Acoustic guitar",
            "Guitar",
            // The body boom out (and the HPF against feedback), a little
            // sparkle, a light 2.5:1.
            processing(
                hpf(100.0, 18),
                Gate::default(),
                [
                    band(LowShelf, 100.0, 0.0, 0.7),
                    band(Bell, 200.0, -3.0, 1.2),
                    band(Bell, 2_500.0, 1.0, 1.0),
                    band(HighShelf, 10_000.0, 2.0, 0.7),
                ],
                comp(-18.0, 2.5, 15.0, 150.0, 1.0),
            ),
        ),
        (
            "factory-overheads",
            "Overheads",
            "Drums",
            // Cymbals and the kit's picture: the kick's low end left to its
            // own mic, a little boxiness out, some air. No dynamics.
            processing(
                hpf(150.0, 12),
                Gate::default(),
                [
                    band(LowShelf, 100.0, 0.0, 0.7),
                    band(Bell, 400.0, -2.0, 1.0),
                    band(Bell, 2_500.0, 0.0, 1.0),
                    band(HighShelf, 10_000.0, 1.5, 0.7),
                ],
                Comp::default(),
            ),
        ),
    ];
    items
        .into_iter()
        .map(|(id, name, category, processing)| LibraryItem {
            id: id.to_string(),
            name: name.to_string(),
            category: category.to_string(),
            sections: vec![Section::Processing],
            settings: StripSettings {
                processing: Some(processing),
                ..StripSettings::default()
            },
            factory: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use livestage_engine::InsertSettings;

    /// A fresh, empty folder for one test.
    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("livestage-library-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn vocal_settings() -> StripSettings {
        StripSettings {
            processing: Some(Processing::default()),
            fader_db: Some(-5.0),
            ..StripSettings::default()
        }
    }

    fn user_items(library: &Library) -> Vec<&LibraryItem> {
        library.items().filter(|item| !item.factory).collect()
    }

    #[test]
    fn items_survive_a_restart_and_every_write_is_atomic() {
        let dir = folder("roundtrip");
        let path = dir.join("library.json");
        let (mut library, log) = Library::open(Some(path.clone()));
        assert!(log.iter().any(|line| line.contains("new")), "{log:?}");
        assert!(user_items(&library).is_empty());
        assert!(!path.exists(), "nothing is written before the first save");

        let first = library
            .save(
                "  Lead vox  ",
                Some("Vocal"),
                vocal_settings(),
                vec![Section::Processing, Section::FaderPan, Section::Processing],
            )
            .unwrap();
        assert!(path.is_file());
        assert!(!sibling(&path, "tmp").exists(), "the temporary is renamed");
        assert!(
            !sibling(&path, "bak").exists(),
            "no backup before a previous file"
        );
        let second = library
            .save("Backing vox", None, vocal_settings(), vec![Section::Eq])
            .unwrap();
        assert_ne!(first, second);
        // The previous file is the backup.
        let bak: LibraryFile =
            serde_json::from_str(&std::fs::read_to_string(sibling(&path, "bak")).unwrap()).unwrap();
        assert_eq!(bak.items.len(), 1);

        let (reopened, _) = Library::open(Some(path.clone()));
        let items = user_items(&reopened);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, first);
        assert_eq!(items[0].name, "Lead vox");
        assert_eq!(items[0].category, "Vocal");
        assert_eq!(
            items[0].sections,
            vec![Section::Processing, Section::FaderPan]
        );
        assert_eq!(items[0].settings, vocal_settings());
        assert_eq!(items[1].category, DEFAULT_CATEGORY);
        assert_eq!(
            user_items(&reopened),
            user_items(&library),
            "what is read back is what was kept"
        );

        // Only the user's items are in the file.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("factory-"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn factory_items_are_read_only() {
        let dir = folder("factory");
        let path = dir.join("library.json");
        let (mut library, _) = Library::open(Some(path.clone()));
        let before = library.revision();
        for item in factory_items() {
            let error = library.delete(&item.id).unwrap_err();
            assert!(error.contains("read-only"), "{error}");
            let error = library.rename(&item.id, "Mine", None).unwrap_err();
            assert!(error.contains("read-only"), "{error}");
        }
        assert_eq!(library.revision(), before);
        assert!(!path.exists(), "a refused change writes nothing");
        assert_eq!(library.items().filter(|i| i.factory).count(), 6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn factory_items_are_honest_processing_starting_points() {
        let items = factory_items();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Vocal",
                "Kick",
                "Snare",
                "Bass DI",
                "Acoustic guitar",
                "Overheads"
            ]
        );
        let ids: HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids.len(), items.len());
        for item in &items {
            assert!(item.factory);
            // The processing section only: never a fader, a name or a send.
            assert_eq!(item.sections, vec![Section::Processing]);
            let processing = item.settings.processing.expect("processing");
            assert_eq!(
                item.settings,
                StripSettings {
                    processing: Some(processing),
                    ..StripSettings::default()
                }
            );
            // Every value already inside its documented range.
            let mut clamped = processing;
            clamp_processing(&mut clamped);
            assert_eq!(clamped, processing, "{} out of range", item.name);
            assert!(processing.hpf.on, "{}: a high-pass on every one", item.name);
            assert!(!processing.delay.on);
            for band in &processing.eq.bands {
                assert!(band.gain_db.abs() <= 4.0, "{}: gentle EQ", item.name);
            }
            if processing.comp.on {
                assert!(processing.comp.ratio <= 4.0, "{}: gentle comp", item.name);
            }
            if processing.gate.on {
                assert!(
                    processing.gate.range_db > -80.0,
                    "{}: a gate that never fully mutes",
                    item.name
                );
            }
        }
        let vocal = items[0].settings.processing.unwrap();
        assert_eq!((vocal.hpf.hz, vocal.hpf.slope_db), (100.0, 18));
        assert_eq!((vocal.comp.ratio, vocal.comp.threshold_db), (3.0, -18.0));
        let kick = items[1].settings.processing.unwrap();
        assert!(kick.gate.on && kick.hpf.hz <= 30.0);
    }

    #[test]
    fn applying_an_item_is_a_paste_of_its_sections() {
        let (mut library, _) = Library::open(None);
        let id = library
            .save(
                "Lead vox",
                Some("Vocal"),
                vocal_settings(),
                vec![Section::Eq, Section::Comp],
            )
            .unwrap();
        let targets = vec![StripRef::Channel(3), StripRef::Bus(9)];
        assert_eq!(
            library.paste_command(&id, targets.clone()).unwrap(),
            Command::PasteStrip {
                targets: targets.clone(),
                settings: vocal_settings(),
                sections: vec![Section::Eq, Section::Comp],
            }
        );
        let factory = library.paste_command("factory-kick", targets).unwrap();
        let Command::PasteStrip { sections, .. } = factory else {
            panic!("a paste");
        };
        assert_eq!(sections, vec![Section::Processing]);
        assert!(
            library
                .paste_command("nope", vec![StripRef::Master])
                .is_err()
        );
        assert!(library.paste_command(&id, Vec::new()).is_err());
    }

    #[test]
    fn without_a_file_the_library_lives_in_memory() {
        let (mut library, log) = Library::open(None);
        assert!(log.is_empty());
        assert!(library.path().is_none());
        let id = library
            .save("Mine", None, vocal_settings(), vec![Section::Processing])
            .unwrap();
        library.rename(&id, "Ours", Some("Keys")).unwrap();
        let item = library.get(&id).unwrap();
        assert_eq!(
            (item.name.as_str(), item.category.as_str()),
            ("Ours", "Keys")
        );
        library.delete(&id).unwrap();
        assert!(library.get(&id).is_none());
        assert_eq!(library.revision(), 3);
    }

    #[test]
    fn what_is_saved_is_checked() {
        let (mut library, _) = Library::open(None);
        assert!(
            library
                .save("   ", None, vocal_settings(), vec![Section::Eq])
                .is_err()
        );
        assert!(
            library
                .save("Name", None, vocal_settings(), vec![])
                .is_err()
        );
        let mut wild = vocal_settings();
        wild.processing.as_mut().unwrap().hpf.hz = 5_000.0;
        let long = "x".repeat(200);
        let id = library
            .save(&long, Some(""), wild, vec![Section::Hpf])
            .unwrap();
        let item = library.get(&id).unwrap();
        assert_eq!(item.name.chars().count(), MAX_LABEL);
        assert_eq!(item.category, DEFAULT_CATEGORY);
        assert_eq!(item.settings.processing.unwrap().hpf.hz, 600.0);
        assert!(library.rename(&id, "", None).is_err());
    }

    #[test]
    fn a_failed_write_changes_nothing() {
        let dir = folder("failed");
        // A file where the library's folder should be.
        std::fs::write(dir.join("blocked"), b"").unwrap();
        let path = dir.join("blocked").join("library.json");
        let (mut library, _) = Library::open(Some(path));
        let error = library
            .save("Mine", None, vocal_settings(), vec![Section::Eq])
            .unwrap_err();
        assert!(error.contains("library not saved"), "{error}");
        assert!(user_items(&library).is_empty());
        assert_eq!(library.revision(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_the_backup_used() {
        let dir = folder("unreadable");
        let path = dir.join("library.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let good = LibraryFile {
            version: FILE_VERSION,
            items: vec![LibraryItem {
                id: "a".into(),
                name: "Saved".into(),
                category: "Vocal".into(),
                sections: vec![Section::Eq],
                settings: vocal_settings(),
                factory: false,
            }],
        };
        std::fs::write(sibling(&path, "bak"), serde_json::to_string(&good).unwrap()).unwrap();
        let (library, log) = Library::open(Some(path.clone()));
        assert!(log.iter().any(|l| l.contains("unreadable")), "{log:?}");
        assert_eq!(user_items(&library).len(), 1);
        assert_eq!(
            std::fs::read(sibling(&path, "unreadable")).unwrap(),
            b"{ not json",
            "the bad file is kept, not written over"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hand_edited_file_cannot_claim_factory_items_or_reuse_ids() {
        let dir = folder("edited");
        let path = dir.join("library.json");
        let item = |id: &str, factory: bool| LibraryItem {
            id: id.into(),
            name: "x".into(),
            category: "y".into(),
            sections: vec![Section::Eq],
            settings: StripSettings {
                inserts: Some(vec![InsertSettings {
                    bypass: false,
                    plugin: livestage_engine::InsertPlugin::Builtin {
                        stem: "equz8".into(),
                        params: Default::default(),
                    },
                }]),
                ..StripSettings::default()
            },
            factory,
        };
        let file = LibraryFile {
            version: FILE_VERSION,
            items: vec![
                item("factory-vocal", true),
                item("same", false),
                item("same", false),
                item("", false),
            ],
        };
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
        let (mut library, _) = Library::open(Some(path));
        let users = user_items(&library);
        assert_eq!(users.len(), 4);
        let ids: HashSet<&str> = library.items().map(|i| i.id.as_str()).collect();
        assert_eq!(ids.len(), 10, "every id unique, factory ones untouched");
        assert!(users.iter().all(|i| !i.factory));
        // The factory vocal is still the factory's.
        assert!(library.get("factory-vocal").unwrap().factory);
        let first = users[0].id.clone();
        library.delete(&first).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_are_unique_uuid_shaped_strings() {
        let mut seen = HashSet::new();
        for _ in 0..2_000 {
            let id = new_id(|id| seen.contains(id));
            let groups: Vec<usize> = id.split('-').map(str::len).collect();
            assert_eq!(groups, [8, 4, 4, 4, 12], "{id}");
            assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
            assert!(seen.insert(id));
        }
    }

    #[test]
    fn the_library_message_lists_factory_then_user_items() {
        let (mut library, _) = Library::open(None);
        library
            .save("Mine", Some("Keys"), vocal_settings(), vec![Section::Eq])
            .unwrap();
        let message = library.message();
        assert_eq!(message["type"], "library");
        let items = message["items"].as_array().unwrap();
        assert_eq!(items.len(), 7);
        assert_eq!(items[0]["id"], "factory-vocal");
        assert_eq!(items[0]["factory"], true);
        let mine = &items[6];
        assert_eq!(mine["factory"], false);
        assert_eq!(mine["category"], "Keys");
        assert_eq!(mine["sections"], json!(["eq"]));
        assert_eq!(mine["settings"]["fader_db"], json!(-5.0));
        let mut keys: Vec<&str> = mine
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["category", "factory", "id", "name", "sections", "settings"]
        );
    }
}
