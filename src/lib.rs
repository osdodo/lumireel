rust_i18n::i18n!("locales", fallback = "en");

pub mod agent;
pub mod assets;
pub mod capture;
pub mod config;
pub mod export;
pub mod frames;
pub mod generate;
pub mod history;
pub mod i18n;
pub mod library;
pub mod mix;
#[cfg(target_os = "macos")]
pub mod native_audio;
pub mod platform;
pub mod preview;
pub mod prompt;
pub mod server;
pub mod settings;
pub mod terminal;
pub mod ui;
pub mod watch;
