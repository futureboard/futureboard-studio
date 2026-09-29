//! SQLite-backed plug-in catalog cache.
//!
//! Stored at:
//! * Windows  — `%APPDATA%\Futureboard Studio\Plugin Database\index.dat`
//! * macOS    — `~/Library/Application Support/Futureboard Studio/Plugin Database/index.dat`
//! * Linux    — `~/.config/Futureboard Studio/Plugin Database/index.dat`
//!
//! The file is SQLite internally (extension `.dat` is deliberate — we own this
//! database, the suffix keeps it from being treated as a generic SQLite file
//! by file managers).
//!
//! This module is the only place that talks to `rusqlite`; the rest of the
//! crate sees `PluginCatalogEntry` / `PluginCatalog` and lets the DB module
//! handle schema, transactions, and upsert. The catalog is read on a worker
//! thread by [`crate::registry::PluginRegistry::load_catalog`] and written
//! transactionally by [`crate::registry::PluginRegistry::scan_with_progress`].

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::registry::{
    PluginFormat, PluginKind, PluginStatus, RegistryPlugin, classify_kind, display_category,
};

/// 1 → 2 added `plugins.sub_categories`; 2 → 3 added `plugins.is_ara`; 3 → 4
/// added `plugins.preset_path`. Every migration is additive (`ALTER TABLE ADD
/// COLUMN`) and read paths tolerate the column's absence, so a database written
/// by an older build keeps loading and an older build keeps reading a database
/// written by this one.
const SCHEMA_VERSION: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginScanStatus {
    Pending,
    Scanning,
    Success,
    Failed,
    Crashed,
    Skipped,
    /// Legacy alias for [`Self::Success`].
    Ok,
    /// Legacy alias for [`Self::Failed`].
    MetadataOnly,
    /// Legacy alias for [`Self::Skipped`].
    Disabled,
}

impl PluginScanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PluginScanStatus::Pending => "pending",
            PluginScanStatus::Scanning => "scanning",
            PluginScanStatus::Success | PluginScanStatus::Ok => "success",
            PluginScanStatus::Failed | PluginScanStatus::MetadataOnly => "failed",
            PluginScanStatus::Crashed => "crashed",
            PluginScanStatus::Skipped | PluginScanStatus::Disabled => "skipped",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "pending" => PluginScanStatus::Pending,
            "scanning" => PluginScanStatus::Scanning,
            "success" | "ok" => PluginScanStatus::Success,
            "failed" | "metadata_only" => PluginScanStatus::Failed,
            "crashed" => PluginScanStatus::Crashed,
            "skipped" | "disabled" => PluginScanStatus::Skipped,
            _ => PluginScanStatus::Success,
        }
    }

    pub fn is_usable(self) -> bool {
        matches!(self, PluginScanStatus::Success | PluginScanStatus::Ok)
    }
}

/// One row in the SQLite catalog (1:1 with the spec).
#[derive(Debug, Clone)]
pub struct PluginCatalogEntry {
    pub id: String,
    pub format: PluginFormat,
    pub name: String,
    pub vendor: Option<String>,
    pub category: Option<String>,
    /// Raw `|`-separated class tags exactly as the plug-in declared them
    /// (VST3 `subCategories`, CLAP features). Persisted so a restart can
    /// re-derive the same [`PluginKind`] and category label the scan did —
    /// previously dropped, which made every cached row fall back to the
    /// normalized display string.
    pub sub_categories: Option<String>,
    /// The plug-in registers an ARA main factory. Cached so the picker can offer
    /// ARA without re-opening every module at startup.
    pub is_ara: bool,
    pub path: PathBuf,
    pub class_id: Option<String>,
    pub bundle_id: Option<String>,
    pub version: Option<String>,
    pub is_instrument: bool,
    pub is_effect: bool,
    pub scan_status: PluginScanStatus,
    pub validation_level: Option<String>,
    pub disabled: bool,
    pub favorite: bool,
    pub file_modified_at: Option<String>,
    pub file_size: Option<i64>,
    pub last_scanned_at: Option<String>,
    pub error: Option<String>,
    pub metadata_json: Option<String>,
    /// The plug-in's `.pst` — the file the scan wrote for it, which the
    /// Browser's Instruments list and a drop onto the arrangement load it
    /// through. `None` for a row written before the column existed, until the
    /// next scan fills it.
    pub preset_path: Option<PathBuf>,
    /// Precomputed lowercased `name + vendor + category + format` string for
    /// substring search.
    pub search_text: String,
}

impl PluginCatalogEntry {
    /// Project a catalog row back into the legacy [`RegistryPlugin`] shape the
    /// rest of the UI consumes, `.pst` path included.
    pub fn to_registry_plugin(&self) -> RegistryPlugin {
        let raw_category = self.category.clone();
        let category = display_category(
            self.format,
            self.category.as_deref().unwrap_or(""),
            raw_category.as_deref(),
            self.sub_categories.as_deref(),
        );
        let kind = kind_from_flags(self.is_instrument, self.is_effect);
        let status = match self.scan_status {
            PluginScanStatus::Success | PluginScanStatus::Ok => PluginStatus::PresetReady,
            _ => PluginStatus::MissingPreset,
        };
        RegistryPlugin {
            id: self.id.clone(),
            name: self.name.clone(),
            vendor: self.vendor.clone().unwrap_or_default(),
            format: self.format,
            category,
            raw_category,
            sub_categories: self.sub_categories.clone(),
            kind,
            path: self.path.clone(),
            class_id: self.class_id.clone(),
            version: self.version.clone(),
            sdk_metadata_loaded: self.scan_status.is_usable(),
            is_ara: self.is_ara,
            preset_path: self.preset_path.clone().unwrap_or_default(),
            scanned_at_ms: parse_iso8601_to_ms(self.last_scanned_at.as_deref()).unwrap_or(0),
            status,
            scan_status: self.scan_status,
            error_message: self.error.clone(),
        }
    }
}

/// Recover the three-state kind from the two boolean columns the schema has
/// always had. `is_instrument` wins; both-clear means the plug-in never
/// declared a class, which is [`PluginKind::Unknown`]. Rows written by older
/// builds always set exactly one flag (`is_effect` defaults to 1), so existing
/// databases keep classifying exactly as before.
fn kind_from_flags(is_instrument: bool, is_effect: bool) -> PluginKind {
    match (is_instrument, is_effect) {
        (true, _) => PluginKind::Instrument,
        (false, true) => PluginKind::Effect,
        (false, false) => PluginKind::Unknown,
    }
}

impl From<&RegistryPlugin> for PluginCatalogEntry {
    fn from(p: &RegistryPlugin) -> Self {
        let kind_is_instrument = matches!(p.kind, PluginKind::Instrument);
        let kind_is_effect = matches!(p.kind, PluginKind::Effect);
        let category = if p.category.is_empty() {
            p.raw_category.clone()
        } else {
            Some(p.category.clone())
        };
        let search_text = format!(
            "{} {} {} {}",
            p.name,
            p.vendor,
            p.display_category(),
            p.format.label()
        )
        .to_lowercase();
        let last_scanned_at = ms_to_iso8601(p.scanned_at_ms);
        // Capture the on-disk file signature so a later scan can detect when the
        // plug-in binary changed (mtime/size) and invalidate this cached row.
        // Previously stored as `None`, leaving the columns permanently empty and
        // making cache invalidation impossible.
        let (file_modified_at, file_size) = file_signature(&p.path);
        Self {
            id: p.id.clone(),
            format: p.format,
            name: p.name.clone(),
            vendor: if p.vendor.is_empty() {
                None
            } else {
                Some(p.vendor.clone())
            },
            category,
            sub_categories: p.sub_categories.clone(),
            is_ara: p.is_ara,
            path: p.path.clone(),
            class_id: p.class_id.clone(),
            bundle_id: None,
            version: p.version.clone(),
            is_instrument: kind_is_instrument,
            is_effect: kind_is_effect,
            scan_status: p.scan_status,
            validation_level: None,
            disabled: false,
            favorite: false,
            file_modified_at,
            file_size,
            last_scanned_at,
            error: p.error_message.clone(),
            metadata_json: None,
            preset_path: (!p.preset_path.as_os_str().is_empty()).then(|| p.preset_path.clone()),
            search_text,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginCatalog {
    pub plugins: Vec<PluginCatalogEntry>,
    pub loaded_at: std::time::Instant,
    pub source_path: PathBuf,
}

impl PluginCatalog {
    pub fn empty(source_path: PathBuf) -> Self {
        Self {
            plugins: Vec::new(),
            loaded_at: std::time::Instant::now(),
            source_path,
        }
    }
}

/// Resolve the on-disk path of the catalog DB. Always returns a path even if
/// the parent directory does not yet exist — call [`ensure_database_dir`]
/// before opening when you intend to write.
pub fn database_path() -> PathBuf {
    let root = root_dir();
    root.join("Futureboard Studio")
        .join("Plugin Database")
        .join("index.dat")
}

#[cfg(target_os = "windows")]
fn root_dir() -> PathBuf {
    // `dirs::config_dir()` on Windows resolves to `%APPDATA%` (Roaming).
    dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(target_os = "macos")]
fn root_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn root_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn database_dir() -> PathBuf {
    database_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn database_exists() -> bool {
    database_path().is_file()
}

pub fn ensure_database_dir() -> Result<(), String> {
    let dir = database_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())
}

/// Open (or create) the SQLite catalog DB. Always runs schema migrations.
pub fn open_database() -> Result<Connection, String> {
    ensure_database_dir()?;
    let path = database_path();
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|e| format!("open {}: {e}", path.display()))?;
    init_schema(&conn).map_err(|e| format!("init schema: {e}"))?;
    Ok(conn)
}

/// Open in read-only mode (returns Err if DB does not exist).
pub fn open_database_readonly() -> Result<Connection, String> {
    let path = database_path();
    if !path.is_file() {
        return Err(format!("Plugin database not found at {}", path.display()));
    }
    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open ro {}: {e}", path.display()))
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS schema_meta (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS plugins (
             id TEXT PRIMARY KEY,
             format TEXT NOT NULL,
             name TEXT NOT NULL,
             vendor TEXT,
             category TEXT,
             sub_categories TEXT,
             is_ara INTEGER NOT NULL DEFAULT 0,
             path TEXT NOT NULL,
             class_id TEXT,
             bundle_id TEXT,
             version TEXT,
             is_instrument INTEGER NOT NULL DEFAULT 0,
             is_effect INTEGER NOT NULL DEFAULT 1,
             scan_status TEXT NOT NULL,
             validation_level TEXT,
             disabled INTEGER NOT NULL DEFAULT 0,
             favorite INTEGER NOT NULL DEFAULT 0,
             file_modified_at TEXT,
             file_size INTEGER,
             last_scanned_at TEXT,
             error TEXT,
             metadata_json TEXT,
             search_text TEXT NOT NULL,
             preset_path TEXT
         );
         CREATE TABLE IF NOT EXISTS plugin_scan_runs (
             id TEXT PRIMARY KEY,
             started_at TEXT,
             finished_at TEXT,
             status TEXT,
             plugins_found INTEGER,
             plugins_valid INTEGER,
             plugins_failed INTEGER,
             plugins_metadata_only INTEGER
         );
         CREATE TABLE IF NOT EXISTS plugin_scan_failures (
             id TEXT PRIMARY KEY,
             plugin_path TEXT,
             format TEXT,
             reason TEXT,
             scanned_at TEXT,
             stderr TEXT,
             crash_code TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_plugins_name ON plugins(name);
         CREATE INDEX IF NOT EXISTS idx_plugins_vendor ON plugins(vendor);
         CREATE INDEX IF NOT EXISTS idx_plugins_format ON plugins(format);
         CREATE INDEX IF NOT EXISTS idx_plugins_category ON plugins(category);
         CREATE INDEX IF NOT EXISTS idx_plugins_search ON plugins(search_text);
         CREATE INDEX IF NOT EXISTS idx_plugins_status ON plugins(scan_status);",
    )?;
    // Additive migration for databases created by schema v1. `CREATE TABLE IF
    // NOT EXISTS` above is a no-op on an existing table, so the column has to be
    // added explicitly.
    if !has_column(conn, "plugins", "sub_categories")? {
        conn.execute_batch("ALTER TABLE plugins ADD COLUMN sub_categories TEXT")?;
    }
    if !has_column(conn, "plugins", "is_ara")? {
        conn.execute_batch("ALTER TABLE plugins ADD COLUMN is_ara INTEGER NOT NULL DEFAULT 0")?;
        // Unlike the other additive columns, this one cannot be defaulted
        // truthfully: whether a plug-in is ARA-capable is only knowable by
        // reading its module, and every existing row was scanned before the
        // scanner could report it. Defaulting to 0 would therefore assert
        // "not ARA" about plug-ins that are. Clearing the cached file
        // signature marks every row stale so an incremental scan re-reads it
        // instead of serving the placeholder forever.
        conn.execute_batch("UPDATE plugins SET file_modified_at = NULL, file_size = NULL")?;
    }
    if !has_column(conn, "plugins", "preset_path")? {
        // Filled by the next scan: the `.pst` a row points at is the one that
        // scan writes, and a guess made here from the name could name a file
        // that belongs to a different plug-in of the same name.
        conn.execute_batch("ALTER TABLE plugins ADD COLUMN preset_path TEXT")?;
    }
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key, value) VALUES('version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// Whether `table` currently has `column`. Used both to migrate on write and to
/// pick a compatible SELECT on read — [`open_database_readonly`] cannot run
/// migrations, so the picker has to be able to read a v1 database as-is.
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn count_rows(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM plugins", [], |r| r.get::<_, i64>(0))
}

pub fn read_all(conn: &Connection) -> rusqlite::Result<Vec<PluginCatalogEntry>> {
    // A v1 database opened read-only has no `sub_categories` column and cannot
    // be migrated here, so select a literal NULL in its place rather than
    // failing the whole catalog load.
    let sub_categories_expr = if has_column(conn, "plugins", "sub_categories")? {
        "sub_categories"
    } else {
        "NULL"
    };
    // Same reasoning for `is_ara`: a pre-v3 database read-only reports every row
    // as non-ARA until the next writable scan migrates and refills it.
    let is_ara_expr = if has_column(conn, "plugins", "is_ara")? {
        "is_ara"
    } else {
        "0"
    };
    let preset_path_expr = if has_column(conn, "plugins", "preset_path")? {
        "preset_path"
    } else {
        "NULL"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT id, format, name, vendor, category, path, class_id, bundle_id, version,
                is_instrument, is_effect, scan_status, validation_level, disabled, favorite,
                file_modified_at, file_size, last_scanned_at, error, metadata_json, search_text,
                {sub_categories_expr}, {is_ara_expr}, {preset_path_expr}
           FROM plugins
          ORDER BY favorite DESC, vendor COLLATE NOCASE ASC, name COLLATE NOCASE ASC"
    ))?;
    let rows = stmt.query_map([], |row| {
        let format: String = row.get(1)?;
        let path: String = row.get(5)?;
        let scan_status: String = row.get(11)?;
        Ok(PluginCatalogEntry {
            id: row.get(0)?,
            format: PluginFormat::from_str_lossy(&format),
            name: row.get(2)?,
            vendor: row.get(3)?,
            category: row.get(4)?,
            sub_categories: row.get(21)?,
            is_ara: row.get::<_, i64>(22)? != 0,
            path: PathBuf::from(path),
            class_id: row.get(6)?,
            bundle_id: row.get(7)?,
            version: row.get(8)?,
            is_instrument: row.get::<_, i64>(9)? != 0,
            is_effect: row.get::<_, i64>(10)? != 0,
            scan_status: PluginScanStatus::from_str_lossy(&scan_status),
            validation_level: row.get(12)?,
            disabled: row.get::<_, i64>(13)? != 0,
            favorite: row.get::<_, i64>(14)? != 0,
            file_modified_at: row.get(15)?,
            file_size: row.get(16)?,
            last_scanned_at: row.get(17)?,
            error: row.get(18)?,
            metadata_json: row.get(19)?,
            preset_path: row
                .get::<_, Option<String>>(23)?
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            search_text: row.get(20)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// One instrument the Browser lists: straight from the catalog, pointing at
/// the `.pst` the scan wrote for it.
#[derive(Debug, Clone, PartialEq)]
pub struct InstrumentPreset {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub format: PluginFormat,
    pub category: String,
    pub preset_path: PathBuf,
}

/// Every usable instrument in the catalog that has a `.pst`, by vendor then
/// name. A row the user disabled and one the scan could not load are not
/// offered.
///
/// A row with no `.pst` recorded — every row of a database written before the
/// column existed, until the next scan — asks `cached_preset` for the scan
/// cache's file with that plug-in id. One still without a file is left out:
/// it would be a row that does nothing when dragged.
pub fn instrument_presets(
    conn: &Connection,
    mut cached_preset: impl FnMut(&str) -> Option<PathBuf>,
) -> rusqlite::Result<Vec<InstrumentPreset>> {
    let mut out: Vec<InstrumentPreset> = read_all(conn)?
        .into_iter()
        .filter(|e| e.is_instrument && !e.disabled && e.scan_status.is_usable())
        .filter_map(|e| {
            let preset_path = e.preset_path.clone().or_else(|| cached_preset(&e.id))?;
            let plugin = e.to_registry_plugin();
            Some(InstrumentPreset {
                id: e.id,
                name: e.name,
                vendor: e.vendor.unwrap_or_default(),
                format: e.format,
                category: plugin.display_category().to_string(),
                preset_path,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a.vendor
            .to_lowercase()
            .cmp(&b.vendor.to_lowercase())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

/// Read `last_scanned_at` of the most recently updated row, returned as
/// milliseconds since epoch. `0` if the table is empty.
pub fn last_scan_ms(conn: &Connection) -> rusqlite::Result<i64> {
    let raw: Option<String> = conn
        .query_row("SELECT MAX(last_scanned_at) FROM plugins", [], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(parse_iso8601_to_ms(raw.as_deref()).unwrap_or(0))
}

/// Make the catalog exactly `entries`, in one transaction.
///
/// A scan produces the complete post-scan catalog, so rows it did not produce
/// are stale: an uninstalled plug-in, or — until the scanner stopped reporting
/// bundle-format plug-ins twice — a duplicate row whose id was derived from the
/// second, inner path. [`upsert_plugins`] alone never removed those, so a
/// rescan could not clear them and the shipped catalog held 710 duplicate
/// vendor+name+format pairs against a 972-class library.
///
/// Returns the number of rows removed. Favorites on surviving rows are kept
/// (the upsert deliberately does not overwrite `favorite`).
pub fn replace_plugins(
    conn: &mut Connection,
    entries: &[PluginCatalogEntry],
) -> rusqlite::Result<u32> {
    // A scan that produced nothing is far more likely a scan root that went
    // away (unplugged drive, revoked permission) than a user who uninstalled
    // every plug-in, so it must not take the catalog with it. Emptying the
    // catalog on purpose is what `clear_with_run_record` is for.
    if entries.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let removed = {
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS scan_ids (id TEXT PRIMARY KEY);
             DELETE FROM scan_ids;",
        )?;
        {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO scan_ids(id) VALUES (?1)")?;
            for entry in entries {
                stmt.execute(params![entry.id])?;
            }
        }
        let removed = tx.execute(
            "DELETE FROM plugins WHERE id NOT IN (SELECT id FROM scan_ids)",
            [],
        )?;
        upsert_within(&tx, entries)?;
        tx.execute_batch("DELETE FROM scan_ids;")?;
        removed as u32
    };
    tx.commit()?;
    Ok(removed)
}

/// Upsert every row in `entries` inside a single transaction. Existing rows
/// keyed by `id` are replaced (favorite flag is preserved). Additive only — use
/// [`replace_plugins`] when the caller owns the whole catalog.
pub fn upsert_plugins(
    conn: &mut Connection,
    entries: &[PluginCatalogEntry],
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    upsert_within(&tx, entries)?;
    tx.commit()
}

fn upsert_within(
    tx: &rusqlite::Transaction<'_>,
    entries: &[PluginCatalogEntry],
) -> rusqlite::Result<()> {
    {
        let mut stmt = tx.prepare(
            "INSERT INTO plugins
                (id, format, name, vendor, category, path, class_id, bundle_id, version,
                 is_instrument, is_effect, scan_status, validation_level, disabled, favorite,
                 file_modified_at, file_size, last_scanned_at, error, metadata_json, search_text,
                 sub_categories, is_ara, preset_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)
             ON CONFLICT(id) DO UPDATE SET
                format = excluded.format,
                name = excluded.name,
                vendor = excluded.vendor,
                category = excluded.category,
                sub_categories = excluded.sub_categories,
                path = excluded.path,
                class_id = excluded.class_id,
                bundle_id = excluded.bundle_id,
                version = excluded.version,
                is_instrument = excluded.is_instrument,
                is_effect = excluded.is_effect,
                scan_status = excluded.scan_status,
                validation_level = excluded.validation_level,
                disabled = excluded.disabled,
                file_modified_at = excluded.file_modified_at,
                file_size = excluded.file_size,
                last_scanned_at = excluded.last_scanned_at,
                error = excluded.error,
                metadata_json = excluded.metadata_json,
                search_text = excluded.search_text,
                is_ara = excluded.is_ara,
                preset_path = excluded.preset_path",
        )?;
        for e in entries {
            stmt.execute(params![
                e.id,
                e.format.label(),
                e.name,
                e.vendor,
                e.category,
                e.path.to_string_lossy(),
                e.class_id,
                e.bundle_id,
                e.version,
                e.is_instrument as i64,
                e.is_effect as i64,
                e.scan_status.as_str(),
                e.validation_level,
                e.disabled as i64,
                e.favorite as i64,
                e.file_modified_at,
                e.file_size,
                e.last_scanned_at,
                e.error,
                e.metadata_json,
                e.search_text,
                e.sub_categories,
                e.is_ara as i64,
                e.preset_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            ])?;
        }
    }
    Ok(())
}

/// Record a scan run summary.
#[allow(clippy::too_many_arguments)]
pub fn record_scan_run(
    conn: &Connection,
    id: &str,
    started_at: &str,
    finished_at: &str,
    status: &str,
    plugins_found: i64,
    plugins_valid: i64,
    plugins_failed: i64,
    plugins_metadata_only: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO plugin_scan_runs
            (id, started_at, finished_at, status, plugins_found, plugins_valid,
             plugins_failed, plugins_metadata_only)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            started_at,
            finished_at,
            status,
            plugins_found,
            plugins_valid,
            plugins_failed,
            plugins_metadata_only
        ],
    )?;
    Ok(())
}

/// Delete every plug-in row. Scan-run / failure history is preserved.
pub fn clear_plugins(conn: &Connection) -> rusqlite::Result<u32> {
    let n = conn.execute("DELETE FROM plugins", [])?;
    Ok(n as u32)
}

/// Delete the SQLite file outright (and the WAL/SHM siblings if present).
pub fn delete_database_file() -> Result<(), String> {
    let path = database_path();
    for ext in ["", "-wal", "-shm"] {
        let target = if ext.is_empty() {
            path.clone()
        } else {
            let mut p = path.clone().into_os_string();
            p.push(ext);
            PathBuf::from(p)
        };
        if target.exists() {
            std::fs::remove_file(&target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn now_iso8601() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    epoch_secs_to_iso8601(now)
}

/// File-change signature used to decide whether a cached catalog entry is still
/// valid for the plug-in binary on disk: its last-modified time (ISO-8601, the
/// same format as `last_scanned_at`) and byte size. Both are `None` when the
/// path cannot be stat'd (missing / permission denied), which callers treat as
/// "not fresh" so a missing file forces a re-scan rather than a silent reuse.
pub(crate) fn file_signature(path: &Path) -> (Option<String>, Option<i64>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (None, None);
    };
    let size = i64::try_from(meta.len()).ok();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .and_then(ms_to_iso8601);
    (mtime, size)
}

/// Whether a cached entry's stored file signature still matches what is on disk.
/// Fresh only when BOTH the modified-time and size are present on each side and
/// equal — any missing component (an unstamped legacy row, an un-stat'able
/// file) is treated as stale so the scanner re-examines the bundle. This is the
/// standard mtime+size freshness heuristic (make / cargo / rsync): a matching
/// pair means the binary is unchanged; a moved file changes the lookup key, so
/// a path change is covered too.
///
/// Consumed by the opt-in per-bundle scan-skip in
/// [`crate::registry::PluginRegistry::scan_with_progress`]
/// (`FUTUREBOARD_PLUGIN_SCAN_INCREMENTAL`).
pub(crate) fn signature_is_fresh(
    cached_mtime: Option<&str>,
    cached_size: Option<i64>,
    current_mtime: Option<&str>,
    current_size: Option<i64>,
) -> bool {
    match (cached_mtime, cached_size, current_mtime, current_size) {
        (Some(cached_m), Some(cached_s), Some(current_m), Some(current_s)) => {
            cached_m == current_m && cached_s == current_s
        }
        _ => false,
    }
}

fn ms_to_iso8601(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    Some(epoch_secs_to_iso8601(ms / 1000))
}

fn epoch_secs_to_iso8601(secs: i64) -> String {
    // Plain ISO-8601-ish UTC stamp. Picked manually so we don't pull in
    // `chrono`/`time` just for the cache.
    let days_from_epoch = secs.div_euclid(86_400);
    let secs_in_day = secs.rem_euclid(86_400);
    let (year, month, day) = days_from_epoch_to_ymd(days_from_epoch);
    let h = secs_in_day / 3600;
    let m = (secs_in_day % 3600) / 60;
    let s = secs_in_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_from_epoch_to_ymd(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp.wrapping_sub(9) } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

fn parse_iso8601_to_ms(s: Option<&str>) -> Option<i64> {
    let s = s?;
    // Expect YYYY-MM-DDTHH:MM:SSZ; tolerate trailing 'Z' missing.
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    let days = ymd_to_days_from_epoch(year, month, day)?;
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec;
    Some(secs * 1000)
}

fn ymd_to_days_from_epoch(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let doy = ((153 * m as u64 + 2) / 5 + day as u64 - 1) as i64;
    let doe = yoe as i64 * 365 + (yoe / 4) as i64 - (yoe / 100) as i64 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Convenience: clear the in-DB plug-in table and bookkeep a "user cleared"
/// scan run.
pub fn clear_with_run_record(conn: &mut Connection) -> rusqlite::Result<u32> {
    let removed = clear_plugins(conn)?;
    let stamp = now_iso8601();
    record_scan_run(conn, &stamp, &stamp, &stamp, "cleared", 0, 0, 0, 0)?;
    Ok(removed)
}

/// Re-export of the catalog helper that classifies rows from declared metadata.
/// Kept here so callers don't have to import `registry::classify_kind`.
#[allow(dead_code)]
pub(crate) fn classify_kind_compat(
    format: PluginFormat,
    category: &str,
    sub_categories: Option<&str>,
) -> PluginKind {
    classify_kind(format, category, sub_categories, true)
}

#[cfg(test)]
mod tests {
    use super::{file_signature, kind_from_flags, signature_is_fresh};
    use crate::registry::PluginKind;

    #[test]
    fn legacy_rows_keep_their_kind() {
        // Every row written by a pre-`Unknown` build sets exactly one flag
        // (`is_effect` defaults to 1 in the schema), so an existing database
        // classifies exactly as it did before.
        assert_eq!(kind_from_flags(true, false), PluginKind::Instrument);
        assert_eq!(kind_from_flags(false, true), PluginKind::Effect);
    }

    #[test]
    fn both_flags_clear_reads_back_as_unknown() {
        assert_eq!(kind_from_flags(false, false), PluginKind::Unknown);
    }

    #[test]
    fn replacing_with_an_empty_scan_keeps_the_catalog() {
        // Guard: an unplugged plug-in drive produces an empty scan, and the
        // prune must not read that as "everything was uninstalled".
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        super::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO plugins (id, format, name, path, scan_status, search_text)
             VALUES ('vst3:1', 'VST3', 'Keeper', 'C:/x.vst3', 'success', 'keeper')",
            [],
        )
        .unwrap();

        assert_eq!(super::replace_plugins(&mut conn, &[]).unwrap(), 0);
        assert_eq!(super::count_rows(&conn).unwrap(), 1);
    }

    #[test]
    fn replacing_drops_rows_the_scan_did_not_produce() {
        use std::path::PathBuf;
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        super::init_schema(&conn).unwrap();
        for id in ["vst3:stale-a", "vst3:stale-b", "vst3:keep"] {
            conn.execute(
                "INSERT INTO plugins (id, format, name, path, scan_status, search_text)
                 VALUES (?1, 'VST3', 'Row', 'C:/x.vst3', 'success', 'row')",
                [id],
            )
            .unwrap();
        }

        let fresh = super::PluginCatalogEntry {
            id: "vst3:keep".into(),
            format: crate::registry::PluginFormat::Vst3,
            name: "Row".into(),
            vendor: None,
            category: None,
            sub_categories: Some("Fx|EQ".into()),
            is_ara: false,
            path: PathBuf::from("C:/x.vst3"),
            class_id: None,
            bundle_id: None,
            version: None,
            is_instrument: false,
            is_effect: true,
            scan_status: super::PluginScanStatus::Success,
            validation_level: None,
            disabled: false,
            favorite: false,
            file_modified_at: None,
            file_size: None,
            last_scanned_at: None,
            error: None,
            metadata_json: None,
            preset_path: Some(PathBuf::from("C:/presets/Row.pst")),
            search_text: "row".into(),
        };
        assert_eq!(super::replace_plugins(&mut conn, &[fresh]).unwrap(), 2);
        assert_eq!(super::count_rows(&conn).unwrap(), 1);

        // And the tags round-trip, so a restart classifies the same way.
        let rows = super::read_all(&conn).unwrap();
        assert_eq!(rows[0].sub_categories.as_deref(), Some("Fx|EQ"));
        // …and so does the `.pst` the row points at.
        assert_eq!(
            rows[0].preset_path.as_deref(),
            Some(std::path::Path::new("C:/presets/Row.pst"))
        );
        assert_eq!(
            rows[0].to_registry_plugin().preset_path,
            PathBuf::from("C:/presets/Row.pst")
        );
    }

    #[test]
    fn a_v3_database_gains_the_preset_column_and_reads_it_empty() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE plugins (
                 id TEXT PRIMARY KEY, format TEXT NOT NULL, name TEXT NOT NULL,
                 vendor TEXT, category TEXT, sub_categories TEXT,
                 is_ara INTEGER NOT NULL DEFAULT 0, path TEXT NOT NULL, class_id TEXT,
                 bundle_id TEXT, version TEXT, is_instrument INTEGER NOT NULL DEFAULT 0,
                 is_effect INTEGER NOT NULL DEFAULT 1, scan_status TEXT NOT NULL,
                 validation_level TEXT, disabled INTEGER NOT NULL DEFAULT 0,
                 favorite INTEGER NOT NULL DEFAULT 0, file_modified_at TEXT, file_size INTEGER,
                 last_scanned_at TEXT, error TEXT, metadata_json TEXT, search_text TEXT NOT NULL
             );
             INSERT INTO plugins (id, format, name, path, is_instrument, is_effect,
                                  scan_status, search_text)
             VALUES ('vst3:synth', 'VST3', 'Synth', 'C:/s.vst3', 1, 0, 'success', 'synth');",
        )
        .unwrap();
        // Read as-is (a read-only open cannot migrate)…
        assert!(super::read_all(&conn).unwrap()[0].preset_path.is_none());
        // …and after the migration the column is there, still empty.
        super::init_schema(&conn).unwrap();
        assert!(super::has_column(&conn, "plugins", "preset_path").unwrap());
        assert!(super::read_all(&conn).unwrap()[0].preset_path.is_none());
        // An instrument without its `.pst` is not offered to the Browser…
        assert!(
            super::instrument_presets(&conn, |_| None)
                .unwrap()
                .is_empty()
        );
        // …unless the scan cache holds one for that plug-in id.
        let cached = std::path::PathBuf::from("C:/cache/Synth.pst");
        let listed =
            super::instrument_presets(&conn, |id| (id == "vst3:synth").then(|| cached.clone()))
                .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].preset_path, cached);
    }

    #[test]
    fn the_browser_lists_usable_instruments_with_a_preset_only() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        super::init_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO plugins (id, format, name, vendor, path, is_instrument, is_effect,
                                  scan_status, disabled, preset_path, search_text) VALUES
             ('a', 'VST3', 'Zeta', 'Beta Audio', 'C:/z', 1, 0, 'success', 0, 'C:/p/Zeta.pst', 'z'),
             ('b', 'VST3', 'Alpha', 'Beta Audio', 'C:/a', 1, 0, 'success', 0, 'C:/p/Alpha.pst', 'a'),
             ('c', 'CLAP', 'Omega', 'Acme', 'C:/o', 1, 0, 'success', 0, 'C:/p/Omega.pst', 'o'),
             ('d', 'VST3', 'Comp', 'Acme', 'C:/c', 0, 1, 'success', 0, 'C:/p/Comp.pst', 'c'),
             ('e', 'VST3', 'Off', 'Acme', 'C:/f', 1, 0, 'success', 1, 'C:/p/Off.pst', 'f'),
             ('f', 'VST3', 'Broken', 'Acme', 'C:/b', 1, 0, 'failed', 0, 'C:/p/Broken.pst', 'b'),
             ('g', 'VST3', 'NoPst', 'Acme', 'C:/n', 1, 0, 'success', 0, NULL, 'n');",
        )
        .unwrap();
        let names: Vec<String> = super::instrument_presets(&conn, |_| None)
            .unwrap()
            .into_iter()
            .map(|i| format!("{} / {}", i.vendor, i.name))
            .collect();
        assert_eq!(
            names,
            ["Acme / Omega", "Beta Audio / Alpha", "Beta Audio / Zeta"]
        );
    }

    #[test]
    fn fresh_only_when_both_components_present_and_equal() {
        let m = "2026-07-08T00:00:00Z";
        assert!(signature_is_fresh(Some(m), Some(4096), Some(m), Some(4096)));
    }

    #[test]
    fn changed_size_is_stale() {
        let m = "2026-07-08T00:00:00Z";
        assert!(!signature_is_fresh(
            Some(m),
            Some(4096),
            Some(m),
            Some(8192)
        ));
    }

    #[test]
    fn changed_mtime_is_stale() {
        assert!(!signature_is_fresh(
            Some("2026-07-08T00:00:00Z"),
            Some(4096),
            Some("2026-07-09T00:00:00Z"),
            Some(4096),
        ));
    }

    #[test]
    fn missing_component_on_either_side_is_stale() {
        let m = "2026-07-08T00:00:00Z";
        // Unstamped legacy cache row (both None) → always re-scan.
        assert!(!signature_is_fresh(None, None, Some(m), Some(4096)));
        // Un-stat'able file on disk → never reuse the cached row.
        assert!(!signature_is_fresh(Some(m), Some(4096), None, None));
        // Partial signatures are stale, not accidentally fresh.
        assert!(!signature_is_fresh(Some(m), None, Some(m), Some(4096)));
    }

    #[test]
    fn file_signature_reports_size_and_mtime_for_real_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("fb_sig_test_{}.bin", std::process::id()));
        std::fs::write(&path, b"hello world").unwrap();

        let (mtime, size) = file_signature(&path);
        assert_eq!(size, Some(11));
        assert!(mtime.is_some(), "a real file has a modified time");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_signature_missing_path_is_none() {
        let path = std::path::Path::new("this/path/definitely/does/not/exist.vst3");
        assert_eq!(file_signature(path), (None, None));
    }
}
