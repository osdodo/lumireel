//! Which language the interface speaks, and where that choice is remembered.
//!
//! The locale is global process state (`rust_i18n::set_locale`), so every view reads the same
//! language the next time it renders. Views are notified by whoever calls [`apply`].

/// Locales this build ships translations for, in the order the menu lists them.
pub const LANGUAGES: &[(&str, &str)] = &[("en", "English"), ("zh-CN", "简体中文")];

pub const DEFAULT: &str = "en";

/// The name of a language as its own speakers write it.
pub fn label(locale: &str) -> &'static str {
    LANGUAGES
        .iter()
        .find(|(code, _)| *code == locale)
        .map_or("English", |(_, name)| name)
}

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
    let locale = locale.trim().to_ascii_lowercase();
    if locale.starts_with("zh") {
        Some("zh-CN")
    } else if locale.starts_with("en") {
        Some("en")
    } else {
        None
    }
}

/// Switch the whole interface; callers must notify every view that renders text.
pub fn apply(locale: &str) {
    rust_i18n::set_locale(locale);
    // gpui-component's own strings (dialogs, date pickers, …) use the same global locale.
    gpui_kit::component::set_locale(locale);
}

/// The locale currently in use.
pub fn current() -> String {
    rust_i18n::locale().to_string()
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
        assert_eq!(normalize("zh-TW"), Some("zh-CN"));
        assert_eq!(normalize("fr-FR"), None);
    }

    #[test]
    fn both_locales_translate_the_same_key() {
        assert_eq!(t!("projects.title", locale = "en").to_string(), "Projects");
        assert_eq!(t!("projects.title", locale = "zh-CN").to_string(), "项目");
    }
}
