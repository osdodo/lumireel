use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context as _, Result, bail};
use rust_i18n::t;

#[derive(Clone, Debug)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    /// The film's length in seconds, once the preview or a capture has measured it.
    pub duration: Option<f64>,
}

/// Every first-level, non-hidden directory under the library, newest first.
pub fn list_projects(root: &Path) -> Vec<Project> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut projects = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let duration = duration(&path);
            Project {
                name,
                path,
                modified,
                duration,
            }
        })
        .collect::<Vec<_>>();
    projects.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    projects
}

pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if ch.is_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug
    }
}

/// Create a project from the embedded template and make it a valid lumireel folder.
pub fn create_project(root: &Path, name: &str) -> Result<Project> {
    let slug = slugify(name);
    let path = root.join(&slug);
    if path.exists() {
        bail!("project {slug} already exists");
    }
    fs::create_dir_all(path.join("shots"))
        .with_context(|| format!("creating {}", path.display()))?;
    fs::create_dir_all(path.join("assets"))?;
    fs::write(
        path.join("index.html"),
        include_str!("../template/index.html"),
    )?;
    crate::prompt::ensure_agents_md(&path)?;
    let modified = path
        .metadata()
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::now());
    Ok(Project {
        name: slug,
        path,
        modified,
        duration: None,
    })
}

/// The agent command this project runs, kept in the project so it travels with the folder.
pub fn saved_agent(project: &Path) -> Option<String> {
    let text = fs::read_to_string(project.join(".reel").join("agent")).ok()?;
    let command = text.trim();
    (!command.is_empty()).then(|| command.to_string())
}

pub fn save_agent(project: &Path, command: &str) -> std::io::Result<()> {
    let dir = project.join(".reel");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("agent"), format!("{}\n", command.trim()))
}

/// Where the measured film length is kept: written by the preview and by captures.
fn duration_path(project: &Path) -> PathBuf {
    project.join(".reel").join("duration")
}

/// The film's length in seconds, once measured. Zero and unreadable files mean "unknown".
pub fn duration(project: &Path) -> Option<f64> {
    let text = fs::read_to_string(duration_path(project)).ok()?;
    let seconds = text.trim().parse::<f64>().ok()?;
    (seconds.is_finite() && seconds > 0.0).then_some(seconds)
}

/// Whether a capture has measured this project already (even if the film is empty).
pub fn has_duration(project: &Path) -> bool {
    duration_path(project).is_file()
}

/// Remember the film's length, so the project list can show it before the preview loads.
pub fn save_duration(project: &Path, seconds: f64) -> std::io::Result<()> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Ok(());
    }
    let dir = project.join(".reel");
    fs::create_dir_all(&dir)?;
    fs::write(duration_path(project), format!("{seconds:.3}\n"))
}

/// "0:12" or "1:02:03" — a length for the project list.
pub fn duration_label(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The project list's thumbnail, drawn by the capture webview into the app's private folder.
pub fn cover_path(project: &Path) -> PathBuf {
    project.join(".reel").join("cover.png")
}

/// Whether the cover is missing or older than some source file (exports and `.reel/` aside).
pub fn cover_is_stale(project: &Path) -> bool {
    let Ok(cover) = cover_path(project).metadata().and_then(|m| m.modified()) else {
        return true;
    };
    newer_than(project, cover)
}

fn newer_than(dir: &Path, time: SystemTime) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name.ends_with(".mp4") {
            return false;
        }
        let Ok(meta) = entry.metadata() else {
            return false;
        };
        if meta.is_dir() {
            newer_than(&entry.path(), time)
        } else {
            meta.modified().is_ok_and(|modified| modified > time)
        }
    })
}

/// Move a project to the system Trash / Recycle Bin, never permanently delete it.
pub fn trash(path: &Path) -> Result<()> {
    ::trash::delete(path).with_context(|| format!("moving {} to the Trash", path.display()))
}

/// "刚刚", "5 分钟前", "3 小时前", "2 天前" — how long ago `time` was, relative to `now`.
pub fn ago(time: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(time).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..60 => t!("time.just_now").to_string(),
        60..3600 => t!("time.minutes_ago", n = secs / 60).to_string(),
        3600..86400 => t!("time.hours_ago", n = secs / 3600).to_string(),
        _ => t!("time.days_ago", n = secs / 86400).to_string(),
    }
}

/// Reveal an item in the platform's file manager; opening the item itself would play a video.
pub fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map(|_| ());
    #[cfg(windows)]
    let result = std::process::Command::new("explorer.exe")
        .arg("/select,")
        .arg(path)
        .spawn()
        .map(|_| ());
    #[cfg(not(any(target_os = "macos", windows)))]
    let result = open::that_detached(if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    });
    if let Err(error) = result {
        log::warn!("could not reveal {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_safe_folder_names() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("  ---  "), "untitled");
        assert_eq!(slugify("发布片头"), "发布片头");
        assert_eq!(slugify("a/b\\c"), "a-b-c");
    }

    #[test]
    fn ago_reads_like_a_person_would_say_it() {
        use std::time::Duration;
        rust_i18n::set_locale("zh-CN");
        let now = SystemTime::now();
        assert_eq!(ago(now, now), "刚刚");
        assert_eq!(ago(now - Duration::from_secs(300), now), "5 分钟前");
        assert_eq!(ago(now - Duration::from_secs(7200), now), "2 小时前");
        assert_eq!(ago(now - Duration::from_secs(3 * 86400), now), "3 天前");
        // A clock that moved backwards is not "in the future".
        assert_eq!(ago(now + Duration::from_secs(60), now), "刚刚");
    }

    #[test]
    fn durations_are_saved_and_read_back() {
        let dir =
            std::env::temp_dir().join(format!("lumireel-duration-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        assert_eq!(duration(&dir), None);
        assert!(!has_duration(&dir));
        save_duration(&dir, 12.3456).unwrap();
        assert!(has_duration(&dir));
        assert_eq!(duration(&dir), Some(12.346));

        // An empty film is measured, just not shown.
        save_duration(&dir, 0.0).unwrap();
        assert!(has_duration(&dir));
        assert_eq!(duration(&dir), None);

        assert_eq!(duration_label(12.4), "0:12");
        assert_eq!(duration_label(75.0), "1:15");
        assert_eq!(duration_label(3723.0), "1:02:03");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_a_playable_project_from_the_template() {
        let root = std::env::temp_dir().join(format!("lumireel-lib-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let project = create_project(&root, "My Film").unwrap();
        assert_eq!(project.name, "my-film");
        assert!(project.path.join("index.html").is_file());
        // Blank: an empty film and an empty shots folder for the agent to fill.
        assert!(project.path.join("shots").is_dir());
        assert_eq!(fs::read_dir(project.path.join("shots")).unwrap().count(), 0);
        let agents = fs::read_to_string(project.path.join("AGENTS.md")).unwrap();
        assert!(agents.starts_with("<!-- lumireel:managed v1 -->"));
        assert_eq!(
            fs::read_to_string(project.path.join("CLAUDE.md")).unwrap(),
            "@AGENTS.md\n"
        );
        assert!(list_projects(&root).iter().any(|p| p.name == "my-film"));
        save_duration(&project.path, 8.0).unwrap();
        let listed = list_projects(&root);
        let listed = listed.iter().find(|p| p.name == "my-film").unwrap();
        assert_eq!(listed.duration, Some(8.0));

        assert_eq!(saved_agent(&project.path), None);
        save_agent(&project.path, "  codex --full-auto ").unwrap();
        assert_eq!(
            saved_agent(&project.path).as_deref(),
            Some("codex --full-auto")
        );

        // No cover yet; a cover newer than every file is fresh; adding a shot makes it stale again.
        assert!(cover_is_stale(&project.path));
        fs::create_dir_all(project.path.join(".reel")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(cover_path(&project.path), b"png").unwrap();
        assert!(!cover_is_stale(&project.path));
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(project.path.join("shots/intro.js"), b"// new shot").unwrap();
        assert!(cover_is_stale(&project.path));

        let _ = fs::remove_dir_all(&root);
    }
}
