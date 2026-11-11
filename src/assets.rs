//! The media files a project keeps around: the pictures, sounds and clips under `assets/`,
//! plus any media in the project root, for the asset shelf under the preview.

use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use rust_i18n::t;

const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "svg", "avif", "heic",
];
const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "m4a", "aac", "ogg", "oga", "flac", "opus", "aiff", "aif",
];
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "m4v", "webm", "mkv", "avi"];
const FONT_EXTS: &[&str] = &["woff", "woff2", "ttf", "otf", "eot"];

/// How many folders below `assets/` are still looked at: enough for an organised shelf,
/// not enough to crawl a checked-in dependency tree.
const MAX_DEPTH: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum AssetKind {
    Image,
    Audio,
    Video,
    Font,
}

impl AssetKind {
    pub fn label(self) -> String {
        match self {
            Self::Image => t!("assets.image").to_string(),
            Self::Audio => t!("assets.audio").to_string(),
            Self::Video => t!("assets.video").to_string(),
            Self::Font => t!("assets.font").to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Asset {
    /// The file on disk.
    pub path: PathBuf,
    /// The path as it is written in `index.html`, relative to the project folder.
    pub relative: String,
    pub name: String,
    pub kind: AssetKind,
    pub size: u64,
    pub modified: SystemTime,
}

/// Every image, sound and clip the project uses, grouped by kind and then by path.
pub fn list(project: &Path) -> Vec<Asset> {
    let mut assets = Vec::new();
    walk(&project.join("assets"), project, 0, &mut assets);
    // Root media is usually a source clip or an export; the shelf is a good place to find it.
    if let Ok(entries) = fs::read_dir(project) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                push(project, &entry.path(), &mut assets);
            }
        }
    }
    assets.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.relative.to_lowercase().cmp(&b.relative.to_lowercase()))
    });
    assets
}

fn walk(dir: &Path, project: &Path, depth: usize, assets: &mut Vec<Asset>) {
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(&entry.path(), project, depth + 1, assets);
        } else if kind.is_file() {
            push(project, &entry.path(), assets);
        }
    }
}

fn push(project: &Path, path: &Path, assets: &mut Vec<Asset>) {
    let Some(kind) = kind_of(path) else {
        return;
    };
    let Ok(metadata) = path.metadata() else {
        return;
    };
    let relative = path
        .strip_prefix(project)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    assets.push(Asset {
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| relative.clone()),
        relative,
        kind,
        size: metadata.len(),
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    });
}

pub fn kind_of(path: &Path) -> Option<AssetKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        Some(AssetKind::Image)
    } else if AUDIO_EXTS.contains(&ext.as_str()) {
        Some(AssetKind::Audio)
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        Some(AssetKind::Video)
    } else if FONT_EXTS.contains(&ext.as_str()) {
        Some(AssetKind::Font)
    } else {
        None
    }
}

/// "820 B", "12 KB", "2.2 MB" — a size a person can read at a glance.
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;
    if bytes < KB {
        format!("{bytes:.0} B")
    } else if bytes < MB {
        format!("{:.0} KB", bytes / KB)
    } else if bytes < GB {
        format!("{:.1} MB", bytes / MB)
    } else {
        format!("{:.2} GB", bytes / GB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lumireel-assets-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("assets/icons")).unwrap();
        fs::create_dir_all(dir.join("shots")).unwrap();
        fs::write(dir.join("assets/music.mp3"), b"12345").unwrap();
        fs::write(dir.join("assets/logo.png"), b"png").unwrap();
        fs::write(dir.join("assets/icons/mark.svg"), b"<svg/>").unwrap();
        fs::write(dir.join("assets/.hidden.png"), b"no").unwrap();
        fs::write(dir.join("assets/notes.txt"), b"no").unwrap();
        fs::write(dir.join("shots/intro.js"), b"no").unwrap();
        fs::write(dir.join("clip.mp4"), b"video").unwrap();
        fs::write(dir.join("assets/Inter.woff2"), b"font").unwrap();
        dir
    }

    #[test]
    fn lists_media_by_kind_and_skips_noise() {
        let dir = project("list");
        let assets = list(&dir);
        let rel: Vec<_> = assets.iter().map(|a| a.relative.as_str()).collect();
        assert_eq!(
            rel,
            vec![
                "assets/icons/mark.svg",
                "assets/logo.png",
                "assets/music.mp3",
                "clip.mp4",
                "assets/Inter.woff2"
            ]
        );
        let kinds: Vec<_> = assets.iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            vec![
                AssetKind::Image,
                AssetKind::Image,
                AssetKind::Audio,
                AssetKind::Video,
                AssetKind::Font
            ]
        );
        assert_eq!(assets[0].size, 6);
        assert_eq!(assets[3].name, "clip.mp4");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extensions_are_case_insensitive() {
        assert_eq!(kind_of(Path::new("A.PNG")), Some(AssetKind::Image));
        assert_eq!(kind_of(Path::new("mix.M4A")), Some(AssetKind::Audio));
        assert_eq!(kind_of(Path::new("film.MOV")), Some(AssetKind::Video));
        assert_eq!(kind_of(Path::new("Inter.woff2")), Some(AssetKind::Font));
        assert_eq!(kind_of(Path::new("index.html")), None);
    }

    #[test]
    fn sizes_read_like_a_file_browser() {
        assert_eq!(human_size(820), "820 B");
        assert_eq!(human_size(12 * 1024), "12 KB");
        assert_eq!(human_size(2_254_857), "2.2 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.00 GB");
    }
}
