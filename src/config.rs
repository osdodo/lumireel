use std::{fs, path::PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub library_dir: PathBuf,
    pub agent_command: String,
    /// The interface language last chosen in the app. `None` means follow the system language.
    #[serde(default)]
    pub locale: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            library_dir: default_library_dir(),
            agent_command: "claude".to_string(),
            locale: None,
        }
    }
}

pub fn default_library_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LUMIREEL_LIBRARY_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "macos")]
    let base = dirs::video_dir().unwrap_or_else(|| home().join("Movies"));
    #[cfg(not(target_os = "macos"))]
    let base = dirs::video_dir().unwrap_or_else(|| home().join("Videos"));
    base.join("lumireel")
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("lumireel").join("config.json"))
}

impl Config {
    /// Load the config, creating the library directory if needed.
    pub fn load() -> Self {
        let config: Config = config_path()
            .and_then(|path| fs::read(path).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        if let Err(error) = fs::create_dir_all(&config.library_dir) {
            log::warn!(
                "could not create library directory {}: {error}",
                config.library_dir.display()
            );
        }
        config
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = config_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
