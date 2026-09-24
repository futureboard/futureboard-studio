use std::collections::HashMap;
use std::sync::OnceLock;

const EN_US: &str = include_str!("../../../packages/shared/locales/en-US/app.ftl");
const JA_JP: &str = include_str!("../../../packages/shared/locales/ja-JP/app.ftl");
const TH_TH: &str = include_str!("../../../packages/shared/locales/th-TH/app.ftl");
const ZH_CN: &str = include_str!("../../../packages/shared/locales/zh-CN/app.ftl");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    EnUs,
    JaJp,
    ThTh,
    ZhCn,
}

impl Locale {
    pub const ALL: [Self; 4] = [Self::EnUs, Self::JaJp, Self::ThTh, Self::ZhCn];

    pub fn from_code(code: &str) -> Self {
        match code.trim().replace('_', "-").to_ascii_lowercase().as_str() {
            "ja" | "ja-jp" => Self::JaJp,
            "th" | "th-th" => Self::ThTh,
            "zh" | "zh-cn" | "zh-hans" | "zh-hans-cn" => Self::ZhCn,
            _ => Self::EnUs,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::EnUs => "en-US",
            Self::JaJp => "ja-JP",
            Self::ThTh => "th-TH",
            Self::ZhCn => "zh-CN",
        }
    }

    pub fn language_key(self) -> &'static str {
        match self {
            Self::EnUs => "settings.language.en",
            Self::JaJp => "settings.language.ja",
            Self::ThTh => "settings.language.th",
            Self::ZhCn => "settings.language.zh",
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
            .copied()
            .or_else(|| locale_messages(Locale::EnUs).get(key).copied())
    }
}

fn locale_messages(locale: Locale) -> &'static HashMap<&'static str, &'static str> {
    match locale {
        Locale::EnUs => messages_en_us(),
        Locale::JaJp => messages_ja_jp(),
        Locale::ThTh => messages_th_th(),
        Locale::ZhCn => messages_zh_cn(),
    }
}

fn parse_messages(source: &'static str) -> HashMap<&'static str, &'static str> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.trim(), value.trim()))
        })
        .collect()
}

fn messages_en_us() -> &'static HashMap<&'static str, &'static str> {
    static MESSAGES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MESSAGES.get_or_init(|| parse_messages(EN_US))
}

fn messages_ja_jp() -> &'static HashMap<&'static str, &'static str> {
    static MESSAGES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MESSAGES.get_or_init(|| parse_messages(JA_JP))
}

fn messages_th_th() -> &'static HashMap<&'static str, &'static str> {
    static MESSAGES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MESSAGES.get_or_init(|| parse_messages(TH_TH))
}

fn messages_zh_cn() -> &'static HashMap<&'static str, &'static str> {
    static MESSAGES: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MESSAGES.get_or_init(|| parse_messages(ZH_CN))
}
