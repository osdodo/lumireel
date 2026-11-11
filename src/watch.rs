use std::{path::Path, time::Duration};

use anyhow::Result;
use notify_debouncer_mini::{
    DebounceEventResult, Debouncer, new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
};

/// Watches a project and signals after a quiet moment. `.reel/`, `.git/` and
/// half-written MP4 files are ignored so a render does not reload the preview.
pub struct ProjectWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
    pub events: flume::Receiver<()>,
}

pub fn watch(project: &Path) -> Result<ProjectWatcher> {
    watch_mode(project, RecursiveMode::Recursive)
}

/// Watch the library's first level so new or removed projects refresh the list.
pub fn watch_library(root: &Path) -> Result<ProjectWatcher> {
    watch_mode(root, RecursiveMode::NonRecursive)
}

fn watch_mode(project: &Path, mode: RecursiveMode) -> Result<ProjectWatcher> {
    let (tx, rx) = flume::unbounded();
    let mut debouncer = new_debouncer(
        Duration::from_millis(300),
        move |result: DebounceEventResult| {
            let Ok(events) = result else {
                return;
            };
            let interesting = events.iter().any(|event| interesting(&event.path));
            if interesting {
                let _ = tx.send(());
            }
        },
    )?;
    debouncer.watcher().watch(project, mode)?;
    Ok(ProjectWatcher {
        _debouncer: debouncer,
        events: rx,
    })
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
