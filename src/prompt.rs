use std::{fs, path::Path};

use anyhow::Result;

const MARKER: &str = "<!-- lumireel:managed v1 -->";
const AGENTS: &str = include_str!("../assets/AGENTS.md");

/// Install or update the prompt files. A user-authored AGENTS.md is never overwritten.
pub fn ensure_agents_md(project: &Path) -> Result<()> {
    let agents_path = project.join("AGENTS.md");
    let existing = fs::read_to_string(&agents_path).ok();
    let should_write = existing
        .as_ref()
        .map(|text| text.starts_with(MARKER))
        .unwrap_or(true);
    if should_write && existing.as_deref() != Some(AGENTS) {
        fs::write(&agents_path, AGENTS)?;
    }

    let claude_path = project.join("CLAUDE.md");
    if !claude_path.exists() {
        fs::write(&claude_path, "@AGENTS.md\n")?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShotInfo {
    pub id: String,
    pub src: Option<String>,
    pub at: f64,
    pub duration: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reference {
    Range {
        start: f64,
        end: f64,
    },
    Region {
        t: f64,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        shot: Option<ShotInfo>,
    },
}

/// Render the block pasted into the agent terminal.
pub fn format_refs(
    page: &str,
    width: u32,
    height: u32,
    references: &[Reference],
    shots: &[ShotInfo],
) -> String {
    let mut out = format!(
        "<reel_refs page=\"{}\" size=\"{}x{}\">\n",
        page, width, height
    );
    for (index, reference) in references.iter().enumerate() {
        let n = index + 1;
        match reference {
            Reference::Range { start, end } => {
                out.push_str(&format!("[{n}] range {:.3}–{:.3} s", start, end));
                let overlapping = overlapping_shots(*start, *end, shots);
                if !overlapping.is_empty() {
                    let parts = overlapping
                        .iter()
                        .map(|(shot, local_start, local_end)| {
                            shot_part(shot, &format!("its {local_start:.3}–{local_end:.3} s"))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(" · shots ");
                    out.push_str(&parts);
                }
            }
            Reference::Region {
                t,
                x,
                y,
                w,
                h,
                shot,
            } => {
                out.push_str(&format!(
                    "[{n}] region at {t:.3} s · x {x:.0}, y {y:.0}, w {w:.0}, h {h:.0}"
                ));
                let shot = shot.clone().or_else(|| shot_at(*t, shots));
                if let Some(shot) = shot {
                    let local = (*t - shot.at).max(0.0);
                    out.push_str(" · shot ");
                    out.push_str(&shot_part(&shot, &format!("its {local:.3} s")));
                }
            }
        }
        out.push('\n');
    }
    out.push_str("</reel_refs>\n");
    out
}

fn shot_part(shot: &ShotInfo, timing: &str) -> String {
    match &shot.src {
        Some(src) => format!("{} ({}, {})", shot.id, src, timing),
        None => format!("{} ({})", shot.id, timing),
    }
}

fn overlapping_shots<'a>(
    start: f64,
    end: f64,
    shots: &'a [ShotInfo],
) -> Vec<(&'a ShotInfo, f64, f64)> {
    shots
        .iter()
        .filter_map(|shot| {
            let shot_end = shot.at + shot.duration;
            let overlap_start = start.max(shot.at);
            let overlap_end = end.min(shot_end);
            (overlap_end > overlap_start).then_some((
                shot,
                overlap_start - shot.at,
                overlap_end - shot.at,
            ))
        })
        .collect()
}

fn shot_at(t: f64, shots: &[ShotInfo]) -> Option<ShotInfo> {
    shots
        .iter()
        .rev()
        .find(|shot| t >= shot.at && t < shot.at + shot.duration)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shots() -> Vec<ShotInfo> {
        vec![
            ShotInfo {
                id: "intro".into(),
                src: Some("shots/intro.js".into()),
                at: 0.0,
                duration: 3.0,
            },
            ShotInfo {
                id: "demo".into(),
                src: Some("shots/demo.js".into()),
                at: 3.0,
                duration: 5.0,
            },
        ]
    }

    #[test]
    fn only_range() {
        let text = format_refs(
            "index.html",
            1920,
            1080,
            &[Reference::Range {
                start: 4.0,
                end: 8.0,
            }],
            &shots(),
        );
        assert_eq!(
            text,
            "<reel_refs page=\"index.html\" size=\"1920x1080\">\n\
             [1] range 4.000–8.000 s · shots demo (shots/demo.js, its 1.000–5.000 s)\n\
             </reel_refs>\n"
        );
    }

    #[test]
    fn only_region_with_shot() {
        let text = format_refs(
            "index.html",
            1920,
            1080,
            &[Reference::Region {
                t: 5.2,
                x: 120.0,
                y: 80.0,
                w: 640.0,
                h: 200.0,
                shot: None,
            }],
            &shots(),
        );
        assert_eq!(
            text,
            "<reel_refs page=\"index.html\" size=\"1920x1080\">\n\
             [1] region at 5.200 s · x 120, y 80, w 640, h 200 · shot demo (shots/demo.js, its 2.200 s)\n\
             </reel_refs>\n"
        );
    }

    #[test]
    fn both_kinds_and_no_shots() {
        let text = format_refs(
            "index.html",
            1280,
            720,
            &[
                Reference::Range {
                    start: 1.0,
                    end: 2.5,
                },
                Reference::Region {
                    t: 2.0,
                    x: 0.0,
                    y: 0.0,
                    w: 10.0,
                    h: 10.0,
                    shot: None,
                },
            ],
            &[],
        );
        assert_eq!(
            text,
            "<reel_refs page=\"index.html\" size=\"1280x720\">\n\
             [1] range 1.000–2.500 s\n\
             [2] region at 2.000 s · x 0, y 0, w 10, h 10\n\
             </reel_refs>\n"
        );
    }

    #[test]
    fn prompt_files_are_installed_without_overwriting_user_text() {
        let dir = std::env::temp_dir().join(format!("lumireel-prompt-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        ensure_agents_md(&dir).unwrap();
        let managed = fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert!(managed.starts_with(MARKER));
        assert_eq!(
            fs::read_to_string(dir.join("CLAUDE.md")).unwrap(),
            "@AGENTS.md\n"
        );

        fs::write(dir.join("AGENTS.md"), "my own notes").unwrap();
        ensure_agents_md(&dir).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("AGENTS.md")).unwrap(),
            "my own notes"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn range_spans_two_shots() {
        let text = format_refs(
            "index.html",
            1920,
            1080,
            &[Reference::Range {
                start: 2.0,
                end: 4.5,
            }],
            &shots(),
        );
        assert!(text.contains("shots intro (shots/intro.js, its 2.000–3.000 s), demo (shots/demo.js, its 0.000–1.500 s)"));
    }
}
