//! Which language the interface speaks, and where that choice is remembered.
//!
//! The locale is global process state (`rust_i18n::set_locale`), so every view reads the same
//! language the next time it renders. Views are notified by whoever calls [`apply`].

use rust_i18n::t;

/// Locales this build ships translations for, in the order the menu lists them.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("zh-CN", "简体中文"),
    ("zh-TW", "繁體中文"),
    ("ja", "日本語"),
    ("ko", "한국어"),
    ("es", "Español"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("pt-BR", "Português (Brasil)"),
    ("ru", "Русский"),
];

pub const DEFAULT: &str = "en";

/// The locale for this launch: an explicit setting wins, then `LUMIREEL_LOCALE`, then the
/// system language. Anything we do not translate falls back to English.
pub fn init(configured: Option<&str>) -> String {
    let locale = configured
        .and_then(normalize)
        .map(str::to_string)
        .or_else(|| {
            std::env::var("LUMIREEL_LOCALE")
                .ok()
                .and_then(|value| normalize(&value).map(str::to_string))
        })
        .unwrap_or_else(|| {
            normalize(&sys_locale::get_locale().unwrap_or_default())
                .unwrap_or(DEFAULT)
                .to_string()
        });
    apply(&locale);
    locale
}

/// Map a system or user locale onto one of [`LANGUAGES`].
pub fn normalize(locale: &str) -> Option<&'static str> {
    let locale = locale.trim().to_ascii_lowercase().replace('_', "-");
    if locale.starts_with("zh") {
        // Traditional script, or a region that writes it.
        let traditional = ["hant", "-tw", "-hk", "-mo"].iter().any(|tag| locale.contains(tag));
        return Some(if traditional { "zh-TW" } else { "zh-CN" });
    }
    if locale.starts_with("pt") {
        return Some("pt-BR");
    }
    let language = locale.split('-').next().unwrap_or_default();
    LANGUAGES
        .iter()
        .map(|(code, _)| *code)
        .find(|code| *code == language)
}

/// Switch the whole interface; callers must notify every view that renders text.
pub fn apply(locale: &str) {
    rust_i18n::set_locale(locale);
    // gpui-component's own strings (dialogs, date pickers, …) use the same global locale.
    gpui_kit::component::set_locale(locale);
}

/// Labels the injected host script draws itself (region selection), in the current language.
pub fn host_strings() -> serde_json::Value {
    serde_json::json!({
        "sendToAgent": rust_i18n::t!("preview.send_to_agent"),
        "sendTooltip": rust_i18n::t!("preview.send_tooltip"),
        "clearRegion": rust_i18n::t!("preview.clear_region"),
    })
}

/// The locale currently in use.
pub fn current() -> String {
    rust_i18n::locale().to_string()
}

/// "Show in Finder" / "Show in File Explorer" / "Show in File Manager", named the way the
/// platform names its file manager.
pub fn reveal() -> String {
    #[cfg(target_os = "macos")]
    return t!("common.reveal_macos").to_string();
    #[cfg(windows)]
    return t!("common.reveal_windows").to_string();
    #[cfg(not(any(target_os = "macos", windows)))]
    return t!("common.reveal_linux").to_string();
}

/// The assets shelf's tooltip for the same action.
pub fn reveal_folder() -> String {
    #[cfg(target_os = "macos")]
    return t!("assets.reveal_folder_macos").to_string();
    #[cfg(windows)]
    return t!("assets.reveal_folder_windows").to_string();
    #[cfg(not(any(target_os = "macos", windows)))]
    return t!("assets.reveal_folder_linux").to_string();
}

/// "Move … to the Trash?" with the platform's name for it.
pub fn trash_title(name: &str) -> String {
    #[cfg(target_os = "macos")]
    return t!("projects.trash_title_macos", name = name).to_string();
    #[cfg(windows)]
    return t!("projects.trash_title_windows", name = name).to_string();
    #[cfg(not(any(target_os = "macos", windows)))]
    return t!("projects.trash_title_linux", name = name).to_string();
}

/// The dialog body that goes with [`trash_title`].
pub fn trash_body() -> String {
    #[cfg(target_os = "macos")]
    return t!("projects.trash_body_macos").to_string();
    #[cfg(windows)]
    return t!("projects.trash_body_windows").to_string();
    #[cfg(not(any(target_os = "macos", windows)))]
    return t!("projects.trash_body_linux").to_string();
}

/// The "Move to Trash" action label.
pub fn trash() -> String {
    #[cfg(target_os = "macos")]
    return t!("projects.trash_macos").to_string();
    #[cfg(windows)]
    return t!("projects.trash_windows").to_string();
    #[cfg(not(any(target_os = "macos", windows)))]
    return t!("projects.trash_linux").to_string();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_i18n::t;

    #[test]
    fn maps_system_locales_to_shipped_ones() {
        assert_eq!(normalize("en-US"), Some("en"));
        assert_eq!(normalize("EN"), Some("en"));
        assert_eq!(normalize("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(normalize("zh-TW"), Some("zh-TW"));
        assert_eq!(normalize("zh-Hant-HK"), Some("zh-TW"));
        assert_eq!(normalize("zh_Hans"), Some("zh-CN"));
        assert_eq!(normalize("ja-JP"), Some("ja"));
        assert_eq!(normalize("pt-PT"), Some("pt-BR"));
        assert_eq!(normalize("de_DE"), Some("de"));
        assert_eq!(normalize("it-IT"), None);
    }

    /// A key missing from a locale silently falls back to English, so check the files directly.
    #[test]
    fn every_locale_has_every_key() {
        let keys = |code: &str| -> Vec<String> {
            let path = format!("{}/locales/{code}.yml", env!("CARGO_MANIFEST_DIR"));
            let text = std::fs::read_to_string(&path).expect(&path);
            let mut keys: Vec<String> = text
                .lines()
                .filter_map(|line| line.split_once(':').map(|(key, _)| key.to_string()))
                .filter(|key| !key.is_empty() && !key.starts_with(' '))
                .collect();
            keys.sort();
            keys
        };
        let english = keys("en");
        for (code, _) in LANGUAGES {
            assert_eq!(keys(code), english, "{code}.yml keys differ from en.yml");
        }
    }

    #[test]
    fn both_locales_translate_the_same_key() {
        assert_eq!(t!("projects.title", locale = "en").to_string(), "Projects");
        assert_eq!(t!("projects.title", locale = "zh-CN").to_string(), "项目");
        assert_eq!(t!("projects.title", locale = "ja").to_string(), "プロジェクト");
        assert_eq!(t!("projects.title", locale = "pt-BR").to_string(), "Projetos");
    }
}
