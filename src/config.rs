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
    /// Project groups the user collapsed, so the list opens the same way next time.
    #[serde(default)]
    pub collapsed_groups: Vec<String>,
    /// OpenAI-compatible endpoints used to generate speech, images and video.
    #[serde(default)]
    pub services: Services,
}

/// One generation service: whose API it speaks, where it is, how to authenticate, which model.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Service {
    /// `openai`, `mimo`, `doubao`, `qwen`, `ark` or `minimax`; empty in configs saved before it existed.
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
}

impl Service {
    /// The provider, guessed from the URL when an older config did not save one.
    pub fn provider(&self) -> &str {
        match self.provider.as_str() {
            "" if self.url.contains("xiaomimimo.com") => "mimo",
            "" if self.url.contains("openspeech.bytedance.com") => "doubao",
            "" if self.url.contains("dashscope") => "qwen",
            "" if self.url.contains("volces.com") || self.url.contains("byteplus") => "ark",
            "" if self.url.contains("minimax") => "minimax",
            "" => "openai",
            provider => provider,
        }
    }
}

/// The three generation services in use, plus saved ones that are not.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Services {
    #[serde(default)]
    pub speech: Service,
    #[serde(default)]
    pub image: Service,
    #[serde(default)]
    pub video: Service,
    /// Services added in Settings but not in use; enabling one swaps it with the one in use.
    #[serde(default)]
    pub spare: Vec<SpareService>,
}

/// A saved service and the kind it generates: `speech`, `image` or `video`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpareService {
    pub kind: String,
    #[serde(flatten)]
    pub service: Service,
}

impl Service {
    /// Nothing filled in: the slot holds no service.
    pub fn is_empty(&self) -> bool {
        self.url.is_empty() && self.api_key.is_empty() && self.model.is_empty()
    }
}

impl Services {
    /// The service in use for `kind`.
    pub fn active(&self, kind: &str) -> &Service {
        match kind {
            "speech" => &self.speech,
            "image" => &self.image,
            _ => &self.video,
        }
    }

    /// The service in use for `kind`.
    pub fn active_mut(&mut self, kind: &str) -> &mut Service {
        match kind {
            "speech" => &mut self.speech,
            "image" => &mut self.image,
            _ => &mut self.video,
        }
    }

    /// Put the spare at `index` in use; the one it replaces becomes a spare.
    pub fn activate(&mut self, index: usize) {
        let SpareService { kind, service } = self.spare.remove(index);
        let old = std::mem::replace(self.active_mut(&kind), service);
        if !old.is_empty() {
            self.spare
                .insert(index, SpareService { kind, service: old });
        }
    }

    /// Stop using `kind`'s service, keeping it as a spare.
    pub fn deactivate(&mut self, kind: &str) {
        let old = std::mem::take(self.active_mut(kind));
        if !old.is_empty() {
            self.spare.push(SpareService {
                kind: kind.to_string(),
                service: old,
            });
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            library_dir: default_library_dir(),
            agent_command: "claude".to_string(),
            locale: None,
            collapsed_groups: Vec::new(),
            services: Services::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabling_a_spare_swaps_it_with_the_one_in_use() {
        let named = |model: &str| Service {
            model: model.into(),
            ..Service::default()
        };
        let mut services = Services {
            image: named("a"),
            spare: vec![SpareService {
                kind: "image".into(),
                service: named("b"),
            }],
            ..Services::default()
        };
        services.activate(0);
        assert_eq!(services.image.model, "b");
        assert_eq!(services.spare[0].service.model, "a");
        services.deactivate("image");
        assert!(services.image.is_empty());
        assert_eq!(services.spare.len(), 2);
        services.activate(1);
        assert_eq!(services.image.model, "b");
        assert_eq!(services.spare.len(), 1);
    }
}
