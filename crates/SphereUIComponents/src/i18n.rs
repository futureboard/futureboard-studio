use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

const EN_US: &str = include_str!("../../../packages/shared/locales/en-US/app.ftl");
const JA_JP: &str = include_str!("../../../packages/shared/locales/ja-JP/app.ftl");
const LO_LA: &str = include_str!("../../../packages/shared/locales/lo-LA/app.ftl");
const TH_TH: &str = include_str!("../../../packages/shared/locales/th-TH/app.ftl");
const ZH_CN: &str = include_str!("../../../packages/shared/locales/zh-CN/app.ftl");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    EnUs,
    JaJp,
    LoLa,
    ThTh,
    ZhCn,
}

impl Locale {
    pub const ALL: [Self; 5] = [Self::EnUs, Self::JaJp, Self::LoLa, Self::ThTh, Self::ZhCn];

    pub fn from_code(code: &str) -> Self {
        match code.trim().replace('_', "-").to_ascii_lowercase().as_str() {
            "ja" | "ja-jp" => Self::JaJp,
            "lo" | "lo-la" => Self::LoLa,
            "th" | "th-th" => Self::ThTh,
            "zh" | "zh-cn" | "zh-hans" | "zh-hans-cn" => Self::ZhCn,
            _ => Self::EnUs,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::EnUs => "en-US",
            Self::JaJp => "ja-JP",
            Self::LoLa => "lo-LA",
            Self::ThTh => "th-TH",
            Self::ZhCn => "zh-CN",
        }
    }

    pub fn language_key(self) -> &'static str {
        match self {
            Self::EnUs => "settings.language.en",
            Self::JaJp => "settings.language.ja",
            Self::LoLa => "settings.language.lo",
            Self::ThTh => "settings.language.th",
            Self::ZhCn => "settings.language.zh",
        }
    }

    fn catalog(self) -> &'static str {
        match self {
            Self::EnUs => EN_US,
            Self::JaJp => JA_JP,
            Self::LoLa => LO_LA,
            Self::ThTh => TH_TH,
            Self::ZhCn => ZH_CN,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct I18n {
    locale: Locale,
}

impl I18n {
    pub fn new(language_code: &str) -> Self {
        Self {
            locale: Locale::from_code(language_code),
        }
    }

    /// Resolve the active UI locale from the global settings model when present.
    pub fn from_app(cx: &gpui::App) -> Self {
        let language = cx
            .try_global::<crate::settings::GlobalSettingsModel>()
            .map(|global| global.0.read(cx).current.general.language.clone())
            .unwrap_or_else(|| "en".to_string());
        Self::new(&language)
    }

    pub fn locale(self) -> Locale {
        self.locale
    }

    pub fn tr(self, key: &str) -> String {
        self.lookup(key).unwrap_or(key).to_string()
    }

    pub fn tr_or(self, key: &str, fallback: &str) -> String {
        self.lookup(key).unwrap_or(fallback).to_string()
    }

    pub fn tr_vars(self, key: &str, vars: &[(&str, String)]) -> String {
        let mut text = self.tr(key);
        for (name, value) in vars {
            text = text.replace(&format!("{{ ${name} }}"), value);
        }
        text
    }

    /// Map a native-menu id (`file.new_project`) to its Fluent key (`menu.file-new_project`).
    pub fn menu_key(menu_id: &str) -> String {
        format!("menu.{}", menu_id.replace('.', "-"))
    }

    pub fn tr_menu(self, menu_id: &str, fallback: &str) -> String {
        let key = Self::menu_key(menu_id);
        self.tr_or(&key, fallback)
    }

    /// Whether this locale defines `key` itself, without the en-US fallback.
    pub fn has_own(self, key: &str) -> bool {
        locale_messages(self.locale).contains_key(key)
    }

    fn lookup(self, key: &str) -> Option<&'static str> {
        locale_messages(self.locale)
            .get(key)
            .or_else(|| locale_messages(Locale::EnUs).get(key))
            .map(|value| value.as_ref())
    }
}

type Messages = HashMap<&'static str, Cow<'static, str>>;

fn locale_messages(locale: Locale) -> &'static Messages {
    static MESSAGES: [OnceLock<Messages>; Locale::ALL.len()] =
        [const { OnceLock::new() }; Locale::ALL.len()];
    MESSAGES[locale as usize].get_or_init(|| parse_messages(locale.catalog()))
}

fn parse_messages(source: &'static str) -> Messages {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.trim(), unescape_properties(value.trim())))
        })
        .collect()
}

/// Crowdin round-trips these catalogs as Java `.properties` (`crowdin.yml`),
/// and its exports escape the properties separator and comment characters as
/// `\:`, `\=`, `\#` and `\!`. Undo exactly those; an escaped backslash (`\\`)
/// and every other sequence (`\n` …) is kept as written.
fn unescape_properties(value: &'static str) -> Cow<'static, str> {
    if !value.contains('\\') {
        return Cow::Borrowed(value);
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(escaped @ (':' | '=' | '#' | '!')) => out.push(escaped),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    const LOCALES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/shared/locales");
    const SRC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

    /// Every `<locale>/app.ftl` shipped under `packages/shared/locales`.
    fn shipped_catalogs() -> BTreeMap<String, String> {
        let mut catalogs = BTreeMap::new();
        for entry in std::fs::read_dir(LOCALES_DIR).expect("locales directory") {
            let dir = entry.expect("locale entry").path();
            let file = dir.join("app.ftl");
            if dir.is_dir() && file.is_file() {
                let code = dir.file_name().unwrap().to_string_lossy().into_owned();
                catalogs.insert(code, std::fs::read_to_string(&file).expect("read app.ftl"));
            }
        }
        catalogs
    }

    fn catalog_keys(source: &str) -> BTreeSet<String> {
        source
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('['))
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim().to_string()))
            .collect()
    }

    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("source directory") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    fn looks_like_key(text: &str) -> bool {
        let mut segments = text.split('.');
        let first = segments.next().unwrap_or_default();
        let valid = |segment: &str| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        };
        valid(first) && text.contains('.') && segments.all(valid)
    }

    /// Keys passed literally to `tr(..)` / `tr_vars(..)`, which have no inline
    /// fallback: a missing key renders as the raw key.
    fn direct_tr_keys(source: &str, out: &mut BTreeSet<String>) {
        for call in ["tr(", "tr_vars("] {
            for (at, _) in source.match_indices(call) {
                let ident_before = source[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
                if ident_before {
                    continue;
                }
                let rest = source[at + call.len()..].trim_start();
                if let Some(literal) = rest.strip_prefix('"') {
                    if let Some(end) = literal
                        .find('"')
                        .filter(|&end| looks_like_key(&literal[..end]))
                    {
                        out.insert(literal[..end].to_string());
                    }
                }
            }
        }
    }

    /// #213: a locale that ships an `app.ftl` must be offered by the language
    /// picker (`Locale::ALL`), round-trip through its settings code, and load
    /// its own catalog rather than falling back to English.
    #[test]
    fn every_shipped_locale_is_selectable_and_loads() {
        let catalogs = shipped_catalogs();
        let offered: BTreeSet<String> = Locale::ALL.iter().map(|l| l.code().to_string()).collect();
        let shipped: BTreeSet<String> = catalogs.keys().cloned().collect();
        assert_eq!(
            shipped, offered,
            "packages/shared/locales and Locale::ALL disagree"
        );

        for locale in Locale::ALL {
            let code = locale.code();
            assert_eq!(
                Locale::from_code(code),
                locale,
                "{code} does not round-trip"
            );
            assert_eq!(
                Locale::from_code(&code.replace('-', "_").to_ascii_lowercase()),
                locale,
                "{code} does not parse from its lowercase underscore form"
            );
            let i18n = I18n::new(code);
            assert_eq!(i18n.locale(), locale);
            let keys = catalog_keys(&catalogs[code]);
            assert!(!keys.is_empty(), "{code} catalog is empty");
            for key in &keys {
                assert!(i18n.has_own(key), "{code} does not load its own `{key}`");
            }
        }

        // The picker maps the chosen label back to a locale, so in every UI
        // language each locale needs its own label and the labels must differ.
        for ui in Locale::ALL {
            let i18n = I18n::new(ui.code());
            let mut labels = BTreeSet::new();
            for locale in Locale::ALL {
                assert!(
                    i18n.has_own(locale.language_key()),
                    "{} has no `{}`",
                    ui.code(),
                    locale.language_key()
                );
                assert!(
                    labels.insert(i18n.tr(locale.language_key())),
                    "{} labels two locales `{}`",
                    ui.code(),
                    i18n.tr(locale.language_key())
                );
            }
            if ui != Locale::EnUs {
                assert_ne!(
                    i18n.tr(Locale::LoLa.language_key()),
                    I18n::new(Locale::EnUs.code()).tr(Locale::LoLa.language_key()),
                    "{} leaves `{}` untranslated",
                    ui.code(),
                    Locale::LoLa.language_key()
                );
            }
        }
        assert_eq!(I18n::new("en-US").tr("settings.language.lo"), "Lao");
        assert_eq!(I18n::new("ja-JP").tr("settings.language.lo"), "ラオ語");
        assert_eq!(I18n::new("lo-LA").tr("settings.language.lo"), "ລາວ");
        assert_eq!(I18n::new("th-TH").tr("settings.language.lo"), "ลาว");
        assert_eq!(I18n::new("zh-CN").tr("settings.language.lo"), "老挝语");
    }

    /// #215, #259: every shipped locale, English included, must define every
    /// key the UI can ask for. That is every key any shipped catalog defines
    /// (so a stale `en-US` copy cannot silently drop a key the translations
    /// still carry), every key passed literally to `tr`/`tr_vars`, and every
    /// key reached through a table: `Locale::language_key` (the language
    /// picker) and `SettingsTab::label_key` (the settings sidebar). The tables
    /// are listed explicitly so a key dropped from *every* catalog at once is
    /// still caught. `tr_or` keys that no catalog defines carry their own
    /// English fallback and are not checked here.
    #[test]
    fn every_key_the_code_asks_for_exists_in_every_locale() {
        let catalogs: BTreeMap<String, BTreeSet<String>> = shipped_catalogs()
            .into_iter()
            .map(|(code, source)| (code, catalog_keys(&source)))
            .collect();
        let mut asked: BTreeSet<String> = catalogs.values().flatten().cloned().collect();
        for locale in Locale::ALL {
            asked.insert(locale.language_key().to_string());
        }
        for tab in crate::components::settings_dialog::SettingsTab::all() {
            asked.insert(tab.label_key().to_string());
        }
        let mut files = Vec::new();
        rust_sources(Path::new(SRC_DIR), &mut files);
        for file in &files {
            let source = std::fs::read_to_string(file).expect("read source");
            direct_tr_keys(&source, &mut asked);
        }
        assert!(asked.contains("settings.tab.experimental"));
        assert!(asked.contains("settings.language.lo"));

        let mut missing = Vec::new();
        for (code, keys) in &catalogs {
            for key in &asked {
                if !keys.contains(key) {
                    missing.push(format!("{code}: {key}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "missing locale keys:\n{}",
            missing.join("\n")
        );
    }

    /// #214: Crowdin exports the catalogs as Java `.properties`, escaping
    /// `:`/`=`/`#`/`!`. No resolved string may show those escapes.
    #[test]
    fn no_translated_value_shows_a_properties_escape() {
        let catalogs = shipped_catalogs();
        let mut stray = Vec::new();
        for locale in Locale::ALL {
            let i18n = I18n::new(locale.code());
            for key in catalog_keys(&catalogs[locale.code()]) {
                let value = i18n.tr(&key);
                if ["\\:", "\\=", "\\#", "\\!"]
                    .iter()
                    .any(|e| value.contains(e))
                {
                    stray.push(format!("{}: {key} = {value}", locale.code()));
                }
            }
        }
        assert!(stray.is_empty(), "escaped values:\n{}", stray.join("\n"));
    }

    #[test]
    fn properties_escapes_unescape_and_other_sequences_stay() {
        assert_eq!(
            unescape_properties("最終スキャン\\: { $when }"),
            "最終スキャン: { $when }"
        );
        assert_eq!(unescape_properties("a \\= b \\# c \\! d"), "a = b # c ! d");
        assert_eq!(unescape_properties("line\\nnext"), "line\\nnext");
        assert_eq!(unescape_properties("path\\\\: x"), "path\\\\: x");
        assert!(matches!(
            unescape_properties("plain: text"),
            Cow::Borrowed(_)
        ));
        assert_eq!(
            I18n::new("ja-JP").tr("plugin-manager.footer.last-scan"),
            "最終スキャン: { $when }"
        );
    }
}
