//! Watches a project (or the library's first level) and signals after writes stop.
//!
//! notify's inotify backend reports a file being *opened* as an `Access` event, and the preview
//! opens the project's files on every load and asset scan. Reacting to those would make the
//! preview reload itself in a loop, which reads as the stage flashing black and white, so only
//! events that change something count.

use std::{
    path::Path,
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// How long the writes have to stop before a reload.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Watches a project, ignoring `.reel/`, `.git/` and half-written MP4 files so a render does not
/// reload the preview.
pub struct ProjectWatcher {
    _watcher: RecommendedWatcher,
    pub events: flume::Receiver<()>,
}

pub fn watch(project: &Path) -> Result<ProjectWatcher> {
    watch_mode(project, RecursiveMode::Recursive)
}

/// Watch the library's first level so new or removed projects refresh the list.
pub fn watch_library(root: &Path) -> Result<ProjectWatcher> {
    watch_mode(root, RecursiveMode::NonRecursive)
}

fn watch_mode(path: &Path, mode: RecursiveMode) -> Result<ProjectWatcher> {
    let (tx, rx) = flume::unbounded();
    let (raw_tx, raw_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        if let Ok(event) = result {
            let _ = raw_tx.send(event);
        }
    })
    .context("starting the file watcher")?;
    watcher.watch(path, mode)?;
    thread::Builder::new()
        .name("lumireel-watch".to_string())
        .spawn(move || debounce(raw_rx, tx))
        .context("starting the watch debouncer")?;
    Ok(ProjectWatcher {
        _watcher: watcher,
        events: rx,
    })
}

/// Turn raw events into one signal once the writes stop. A save often touches the same file a few
/// times, and editors write with renames, so every change restarts the quiet window. Reads are
/// ignored outright: they must not hold the window open either.
fn debounce(raw: mpsc::Receiver<Event>, events: flume::Sender<()>) {
    let mut deadline: Option<Instant> = None;
    loop {
        let event = match deadline {
            None => match raw.recv() {
                Ok(event) => event,
                Err(_) => return,
            },
            Some(at) => {
                let wait = at.saturating_duration_since(Instant::now());
                match raw.recv_timeout(wait) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => {
                        deadline = None;
                        if events.send(()).is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        };
        if triggers(&event) {
            deadline = Some(Instant::now() + DEBOUNCE);
        }
    }
}

/// Whether an event changed a file we reload for. Reads (open, close, access) must not count:
/// the preview itself opens these files while loading them.
fn triggers(event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.iter().any(|path| interesting(path))
}

fn interesting(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text.contains("/.reel/")
        || text.ends_with("/.reel")
        || text.contains("/.git/")
        || text.ends_with("/.git")
    {
        return false;
    }
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
    {
        return false;
    }
    // Ignore editor/temp noise.
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    !name.ends_with('~') && !name.starts_with(".#")
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::{
        EventKind,
        event::{AccessKind, AccessMode, DataChange, ModifyKind},
    };
    use std::path::PathBuf;

    #[test]
    fn reading_a_file_does_not_trigger_a_reload() {
        // The preview opening its own files is not a change; reacting to it reloaded in a loop.
        let open = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)))
            .add_path(PathBuf::from("/film/index.html"));
        assert!(!triggers(&open));

        let write = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
            .add_path(PathBuf::from("/film/index.html"));
        assert!(triggers(&write));
    }

    #[test]
    fn the_projects_own_output_is_ignored() {
        let cover = Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/film/.reel/cover.png"));
        assert!(!triggers(&cover));

        let render = Event::new(EventKind::Create(notify::event::CreateKind::File))
            .add_path(PathBuf::from("/film/out.mp4"));
        assert!(!triggers(&render));
    }
}
