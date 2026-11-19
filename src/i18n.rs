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
    /// Chart type: Baobab-style rings instead of the treemap.
    pub rings: bool,
    /// Closing the window hides it to the tray instead of quitting.
    pub close_to_tray: bool,
    /// Drives left out of the "All" view (e.g. "H:").
    pub excluded_drives: Vec<String>,
    /// Folders the duplicate finder never looks into (full paths).
    pub dup_exclude: Vec<String>,
}

fn settings_path() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("WizEverything");
    Some(dir.join("settings.ini"))
}

impl Settings {
    pub fn load() -> Self {
        let text = settings_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        Self::parse(&text, system_lang())
    }

    /// Parse `settings.ini` text; unknown keys are ignored, missing ones keep defaults.
    pub fn parse(text: &str, default_lang: Lang) -> Self {
        let mut s = Settings {
            lang: default_lang,
            theme: Theme::System,
            show_treemap: true,
            show_exts: true,
            rings: false,
            close_to_tray: true,
            excluded_drives: Vec::new(),
            dup_exclude: Vec::new(),
        };
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
                ("close_to_tray", v) => s.close_to_tray = v != "0",
                ("chart", v) => s.rings = v == "rings",
                // One line per folder: dup_exclude=C:\path
                ("dup_exclude", v) if !v.is_empty() => s.dup_exclude.push(v.to_string()),
                ("excluded_drives", v) => {
                    s.excluded_drives = v.split(',').map(str::trim).filter(|d| !d.is_empty()).map(String::from).collect()
                }
                _ => {}
            }
        }
        s
    }

    pub fn to_text(&self) -> String {
        format!(
            "lang={}\ntheme={}\nshow_treemap={}\nshow_exts={}\nchart={}\nclose_to_tray={}\nexcluded_drives={}\n",
            if self.lang == Lang::Es { "es" } else { "en" },
            match self.theme {
                Theme::System => "system",
                Theme::Light => "light",
                Theme::Dark => "dark",
            },
            self.show_treemap as u8,
            self.show_exts as u8,
            if self.rings { "rings" } else { "treemap" },
            self.close_to_tray as u8,
            self.excluded_drives.join(","),
        ) + &self.dup_exclude.iter().map(|d| format!("dup_exclude={d}\n")).collect::<String>()
    }

    pub fn save(&self) {
        let Some(path) = settings_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, self.to_text());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip() {
        let mut s = Settings::parse("", Lang::Es);
        assert_eq!(s.lang, Lang::Es);
        assert!(s.show_treemap && s.show_exts && s.close_to_tray && !s.rings);
        s.lang = Lang::En;
        s.theme = Theme::Dark;
        s.rings = true;
        s.show_exts = false;
        s.close_to_tray = false;
        s.excluded_drives = vec!["H:".into(), "X:".into()];
        let back = Settings::parse(&s.to_text(), Lang::Es);
        assert_eq!(back.lang, Lang::En);
        assert_eq!(back.theme, Theme::Dark);
        assert!(back.rings && !back.show_exts && !back.close_to_tray && back.show_treemap);
        assert_eq!(back.excluded_drives, vec!["H:", "X:"]);
    }

    #[test]
    fn settings_ignore_garbage() {
        let s = Settings::parse("nonsense\nlang=xx\n=\ntheme=light\nexcluded_drives= , H: ,\n", Lang::En);
        assert_eq!(s.lang, Lang::En);
        assert_eq!(s.theme, Theme::Light);
        assert_eq!(s.excluded_drives, vec!["H:"]);
    }
}
