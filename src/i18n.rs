//! Minimal runtime i18n (Spanish / English) and persisted user settings.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Es,
    En,
}

static LANG: AtomicU8 = AtomicU8::new(1);

pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == 0 { Lang::Es } else { Lang::En }
}

pub fn set_lang(l: Lang) {
    LANG.store(if l == Lang::Es { 0 } else { 1 }, Ordering::Relaxed);
}

/// Pick the string for the current language.
pub fn tr(es: &'static str, en: &'static str) -> &'static str {
    if lang() == Lang::Es { es } else { en }
}

/// Default language from the Windows UI language (Spanish → Es, else En).
pub fn system_lang() -> Lang {
    use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
    let id = unsafe { GetUserDefaultUILanguage() };
    if id & 0x3ff == 0x0a { Lang::Es } else { Lang::En }
}

/// Decimal and thousands separators for the current language.
pub fn seps() -> (char, char) {
    if lang() == Lang::Es { (',', '.') } else { ('.', ',') }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    System,
    Light,
    Dark,
}

pub struct Settings {
    pub lang: Lang,
    pub theme: Theme,
    pub show_treemap: bool,
    pub show_exts: bool,
}

fn settings_path() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("WizEverything");
    Some(dir.join("settings.ini"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Settings { lang: system_lang(), theme: Theme::System, show_treemap: true, show_exts: true };
        let Some(text) = settings_path().and_then(|p| std::fs::read_to_string(p).ok()) else { return s };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match (k.trim(), v.trim()) {
                ("lang", "es") => s.lang = Lang::Es,
                ("lang", "en") => s.lang = Lang::En,
                ("theme", "light") => s.theme = Theme::Light,
                ("theme", "dark") => s.theme = Theme::Dark,
                ("theme", "system") => s.theme = Theme::System,
                ("show_treemap", v) => s.show_treemap = v != "0",
                ("show_exts", v) => s.show_exts = v != "0",
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(path) = settings_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "lang={}\ntheme={}\nshow_treemap={}\nshow_exts={}\n",
            if self.lang == Lang::Es { "es" } else { "en" },
            match self.theme {
                Theme::System => "system",
                Theme::Light => "light",
                Theme::Dark => "dark",
            },
            self.show_treemap as u8,
            self.show_exts as u8,
        );
        let _ = std::fs::write(path, text);
    }
}
