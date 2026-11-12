//! Past agent sessions: discovery of the transcripts the agents write.
//!
//! Ported from syro's `session.rs`, trimmed to what the agent panel's history menu needs: the
//! sessions of one project, newest first, with a title and the command that re-enters them.
//!
//! The file layouts are Claude Code (`~/.claude/projects/<encoded-cwd>/<id>.jsonl`), Pi
//! (`~/.pi/agent/sessions/--<encoded-cwd>--/*.jsonl`) and Codex's dated rollouts. OpenCode keeps
//! its history in SQLite, so it is read through the same database its sessions live in.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rust_i18n::t;
use serde_json::Value;

/// The head zone: the first 64 KB, where the records that name the session are.
const HEAD_BYTES: u64 = 64 * 1024;
/// The tail zone: the newest turns, which is where the preview comes from.
const TAIL_BYTES: u64 = 128 * 1024;
/// How many characters a title keeps.
const TITLE_CHARS: usize = 80;
/// How many characters a preview keeps.
const PREVIEW_CHARS: usize = 80;
/// How many sessions the menu lists.
const LIMIT: usize = 20;

/// The agent a transcript belongs to; its spelling is the preset's command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAgent {
    Claude,
    Pi,
    Codex,
    OpenCode,
}

impl SessionAgent {
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Pi => "Pi",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "claude" => Some(Self::Claude),
            "pi" => Some(Self::Pi),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            _ => None,
        }
    }
}

/// One past session, as the history menu lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub agent: SessionAgent,
    /// The agent's own session id (the file stem for Pi, the `sessionId` record for Claude).
    pub id: String,
    /// The transcript itself; for OpenCode, its database.
    pub path: PathBuf,
    /// The directory the agent worked in.
    pub cwd: PathBuf,
    pub title: String,
    pub preview: String,
    /// Last write: what the rows sort by.
    pub updated: SystemTime,
}

/// Where the agents keep their transcripts under a home directory.
#[derive(Clone, Debug)]
pub struct Roots {
    pub claude: PathBuf,
    pub pi: PathBuf,
    pub codex: PathBuf,
    /// OpenCode's SQLite database.
    pub opencode: PathBuf,
}

impl Roots {
    pub fn from_home(home: &Path) -> Self {
        Self {
            claude: home.join(".claude").join("projects"),
            pi: home.join(".pi").join("agent").join("sessions"),
            codex: home.join(".codex").join("sessions"),
            opencode: home
                .join(".local")
                .join("share")
                .join("opencode")
                .join("opencode.db"),
        }
    }

    /// This user's roots, from the home directory.
    pub fn default_roots() -> Self {
        Self::from_home(&dirs::home_dir().unwrap_or_default())
    }
}

/// The sessions of `project`, newest first.
pub fn list(project: &Path) -> Vec<Session> {
    list_in(&Roots::default_roots(), project)
}

/// The same scan, with the roots handed in: a test points them at a fixture home.
pub fn list_in(roots: &Roots, project: &Path) -> Vec<Session> {
    let mut sessions = Vec::new();
    sessions.extend(claude(roots, project));
    sessions.extend(pi(roots, project));
    sessions.extend(codex(roots, project));
    sessions.extend(opencode(roots, project));
    sessions.sort_by(|left, right| {
        right
            .updated
            .cmp(&left.updated)
            .then_with(|| left.id.cmp(&right.id))
    });
    sessions.truncate(LIMIT);
    sessions
}

/// The agent's command without another agent's resume flags: `claude` stays
/// `claude --dangerously-skip-permissions`, while `$SHELL` becomes the session's own agent.
pub fn agent_base(current: &str, agent: SessionAgent) -> String {
    let program = current.split_whitespace().next().unwrap_or_default();
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if SessionAgent::from_id(program) == Some(agent) {
        current.trim().to_string()
    } else {
        agent.id().to_string()
    }
}

/// The shell command that re-enters `session` with `base`'s agent.
pub fn resume_command(base: &str, session: &Session) -> String {
    match session.agent {
        SessionAgent::Claude => format!("{base} --resume {}", quote(&session.id)),
        SessionAgent::Codex => format!("{base} resume {}", quote(&session.id)),
        SessionAgent::Pi => format!(
            "{base} --session {}",
            quote(&session.path.to_string_lossy())
        ),
        SessionAgent::OpenCode => format!("{base} --session {}", quote(&session.id)),
    }
}

/// One shell word, quoted only when it needs to be.
fn quote(text: &str) -> String {
    crate::platform::quote_shell_word(text)
}

fn claude(roots: &Roots, project: &Path) -> Vec<Session> {
    let Ok(buckets) = fs::read_dir(&roots.claude) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for bucket in buckets.flatten() {
        let Ok(files) = fs::read_dir(bucket.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Some(fold) = fold(&path, SessionAgent::Claude) else {
                continue;
            };
            let Some(cwd) = fold.cwd.clone() else {
                continue;
            };
            if !under(&cwd, project) {
                continue;
            }
            sessions.push(session(
                SessionAgent::Claude,
                fold.id.clone().unwrap_or_else(|| file_stem(&path)),
                path,
                cwd,
                fold,
                metadata,
            ));
        }
    }
    sessions
}

fn pi(roots: &Roots, project: &Path) -> Vec<Session> {
    let Ok(buckets) = fs::read_dir(&roots.pi) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for bucket in buckets.flatten() {
        let Ok(files) = fs::read_dir(bucket.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Some(fold) = fold(&path, SessionAgent::Pi) else {
                continue;
            };
            let Some(cwd) = fold.cwd.clone() else {
                continue;
            };
            if !under(&cwd, project) {
                continue;
            }
            sessions.push(session(
                SessionAgent::Pi,
                fold.id.clone().unwrap_or_else(|| file_stem(&path)),
                path,
                cwd,
                fold,
                metadata,
            ));
        }
    }
    sessions
}

/// Codex sorts its rollouts into year/month/day directories under its root rather than one
/// bucket per project, so the walk goes by depth rather than by shape.
fn codex(roots: &Roots, project: &Path) -> Vec<Session> {
    /// Enough for the dated tree with room to spare.
    const DEPTH: usize = 4;

    let mut sessions = Vec::new();
    let mut dirs = vec![(roots.codex.clone(), 0_usize)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if depth < DEPTH {
                    dirs.push((path, depth + 1));
                }
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Some((id, cwd)) = codex_header(&path) else {
                continue;
            };
            if !under(&cwd, project) {
                continue;
            }
            let Some(fold) = fold(&path, SessionAgent::Codex) else {
                continue;
            };
            sessions.push(session(
                SessionAgent::Codex,
                fold.id.clone().unwrap_or(id),
                path,
                cwd,
                fold,
                metadata,
            ));
        }
    }
    sessions
}

/// A Codex rollout's first line: the session's own id and the directory it ran in.
fn codex_header(path: &Path) -> Option<(String, PathBuf)> {
    let file = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let payload = value.get("payload")?;
    let cwd = PathBuf::from(payload.get("cwd")?.as_str()?);
    let id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("id").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    Some((id, cwd))
}

fn session(
    agent: SessionAgent,
    id: String,
    path: PathBuf,
    cwd: PathBuf,
    fold: Fold,
    metadata: fs::Metadata,
) -> Session {
    let id = if id.is_empty() { file_stem(&path) } else { id };
    Session {
        agent,
        id,
        path,
        cwd,
        title: fold
            .title
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| t!("history.untitled").to_string()),
        preview: fold.preview.unwrap_or_default(),
        updated: metadata.modified().unwrap_or(UNIX_EPOCH),
    }
}

/// OpenCode's newest sessions for `project`, from its SQLite database.
fn opencode(roots: &Roots, project: &Path) -> Vec<Session> {
    let Some(connection) = open_opencode(&roots.opencode) else {
        return Vec::new();
    };
    let directory = project.to_string_lossy();
    let directory = directory.trim_end_matches('/');
    let Ok(mut statement) = connection.prepare(
        "select id, directory, title, time_updated from session_v2 \
         where directory = ?1 or substr(directory, 1, length(?1) + 1) = ?1 || '/' \
         order by time_updated desc limit ?2",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map(rusqlite::params![directory, LIMIT as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .map(|(id, directory, title, updated)| {
            let cwd = PathBuf::from(directory);
            let title = title
                .filter(|title| !title.trim().is_empty())
                .map(|title| one_line(&title, TITLE_CHARS))
                .or_else(|| first_user_text(&connection, &id))
                .unwrap_or_else(|| t!("history.untitled").to_string());
            Session {
                agent: SessionAgent::OpenCode,
                id: id.clone(),
                path: roots.opencode.clone(),
                cwd,
                title,
                preview: last_assistant_text(&connection, &id).unwrap_or_default(),
                updated: from_millis(updated),
            }
        })
        .collect()
}

/// A read-only connection to OpenCode's database. `None` when the agent has never run here, or
/// when the database is locked in a way SQLite's read-only mode cannot recover from; both mean
/// the same thing to a scan: nothing to read.
fn open_opencode(db: &Path) -> Option<rusqlite::Connection> {
    if !db.is_file() {
        return None;
    }
    let connection = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // OpenCode writes while it runs; a short wait reads the committed rows.
    let _ = connection.busy_timeout(std::time::Duration::from_secs(2));
    Some(connection)
}

/// The first thing the user asked in an OpenCode session.
fn first_user_text(connection: &rusqlite::Connection, id: &str) -> Option<String> {
    let data: String = connection
        .query_row(
            "select data from session_message \
             where session_id = ?1 and type = 'user' \
             order by time_created limit 1",
            [id],
            |row| row.get(0),
        )
        .ok()?;
    let value: Value = serde_json::from_str(&data).ok()?;
    let text = value.get("text")?.as_str()?.trim();
    (!text.is_empty()).then(|| one_line(text, TITLE_CHARS))
}

/// The newest assistant answer in an OpenCode session.
fn last_assistant_text(connection: &rusqlite::Connection, id: &str) -> Option<String> {
    let data: String = connection
        .query_row(
            "select data from session_message \
             where session_id = ?1 and type = 'assistant' \
             order by time_created desc limit 1",
            [id],
            |row| row.get(0),
        )
        .ok()?;
    let value: Value = serde_json::from_str(&data).ok()?;
    texts(value.get("content")?).map(|text| one_line(&text, PREVIEW_CHARS))
}

/// Everything one transcript's pass accumulates.
#[derive(Default)]
struct Fold {
    id: Option<String>,
    cwd: Option<PathBuf>,
    title: Option<String>,
    preview: Option<String>,
}

/// Fold one transcript: the head names it and holds the first user turn; the tail holds the
/// newest answer. A file small enough to be both is read once.
fn fold(path: &Path, agent: SessionAgent) -> Option<Fold> {
    let len = fs::metadata(path).ok()?.len();
    let mut fold = Fold::default();
    let mut first_user = String::new();
    let mut last_assistant = String::new();

    if len <= HEAD_BYTES + TAIL_BYTES {
        let file = File::open(path).ok()?;
        read_lines(
            BufReader::new(file),
            agent,
            &mut fold,
            &mut first_user,
            &mut last_assistant,
            true,
        )
        .ok()?;
    } else {
        let file = File::open(path).ok()?;
        read_lines(
            BufReader::new(file.take(HEAD_BYTES)),
            agent,
            &mut fold,
            &mut first_user,
            &mut last_assistant,
            true,
        )
        .ok()?;

        let mut file = File::open(path).ok()?;
        file.seek(SeekFrom::Start(len - TAIL_BYTES)).ok()?;
        let mut reader = BufReader::new(file);
        // The tail may begin mid-record; that partial line is not one to read.
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        read_lines(
            reader,
            agent,
            &mut fold,
            &mut first_user,
            &mut last_assistant,
            false,
        )
        .ok()?;
    }

    fold.title = (!first_user.is_empty()).then(|| one_line(&first_user, TITLE_CHARS));
    fold.preview = (!last_assistant.is_empty()).then(|| one_line(&last_assistant, PREVIEW_CHARS));
    Some(fold)
}

/// Fold every line a reader yields in.
fn read_lines(
    mut reader: impl BufRead,
    agent: SessionAgent,
    fold: &mut Fold,
    first_user: &mut String,
    last_assistant: &mut String,
    head: bool,
) -> std::io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        consume(agent, &line, fold, first_user, last_assistant, head);
    }
    Ok(())
}

/// Fold one record in. Only the records that name the session or carry a turn are read.
fn consume(
    agent: SessionAgent,
    line: &str,
    fold: &mut Fold,
    first_user: &mut String,
    last_assistant: &mut String,
    head: bool,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => return,
    };
    let (kind, text) = match agent {
        SessionAgent::Claude => {
            if head {
                if fold.id.is_none() {
                    fold.id = value
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                if fold.cwd.is_none() {
                    fold.cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                }
            }
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let text = value
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(texts);
            (kind, text)
        }
        SessionAgent::Pi => {
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if head && kind == "session" {
                if fold.id.is_none() {
                    fold.id = value.get("id").and_then(Value::as_str).map(str::to_string);
                }
                if fold.cwd.is_none() {
                    fold.cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                }
            }
            if kind != "message" {
                return;
            }
            let role = value
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let text = value
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(texts);
            (role, text)
        }
        SessionAgent::Codex => {
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if kind != "response_item" {
                return;
            }
            let payload = match value.get("payload") {
                Some(payload) => payload,
                None => return,
            };
            if payload.get("type").and_then(Value::as_str) != Some("message") {
                return;
            }
            let role = payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let text = payload.get("content").and_then(texts);
            (role, text)
        }
        SessionAgent::OpenCode => return,
    };

    let Some(text) = text else {
        return;
    };
    match kind {
        "user" => {
            if head && first_user.is_empty() {
                *first_user = text;
            }
        }
        "assistant" => *last_assistant = text,
        _ => {}
    }
}

/// The text of a message's content: a bare string, or the array's text blocks.
fn texts(content: &Value) -> Option<String> {
    let joined = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Whitespace collapsed to single spaces, cut to `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        return collapsed;
    }
    let mut cut: String = collapsed.chars().take(limit.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Whether `path` is `root` or inside it, by path components — `…/app` must not claim
/// `…/app-secret`.
fn under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn from_millis(millis: i64) -> SystemTime {
    let millis = millis.max(0) as u64;
    UNIX_EPOCH + std::time::Duration::from_millis(millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lumireel-history-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, lines: &[String]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut body = String::new();
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        fs::write(path, body).unwrap();
    }

    fn claude_line(kind: &str, cwd: &str, text: &str) -> String {
        serde_json::json!({
            "type": kind,
            "sessionId": "8f2c1d4e",
            "cwd": cwd,
            "message": {
                "role": kind,
                "content": [{"type": "text", "text": text}],
            },
        })
        .to_string()
    }

    #[test]
    fn claude_sessions_are_listed_with_their_first_and_last_turns() {
        let home = home("claude");
        let project = Path::new("/Users/ada/code/reel");
        let path = home
            .join(".claude/projects/-Users-ada-code-reel")
            .join("8f2c1d4e.jsonl");
        write(
            &path,
            &[
                claude_line("user", "/Users/ada/code/reel", "Make a rap video"),
                claude_line("assistant", "/Users/ada/code/reel", "On it."),
            ],
        );

        let sessions = list_in(&Roots::from_home(&home), project);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.agent, SessionAgent::Claude);
        assert_eq!(session.id, "8f2c1d4e");
        assert_eq!(session.cwd.as_path(), project);
        assert_eq!(session.title, "Make a rap video");
        assert_eq!(session.preview, "On it.");
        assert_eq!(
            resume_command("claude", session),
            "claude --resume 8f2c1d4e"
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn pi_sessions_are_listed_by_their_bucket() {
        let home = home("pi");
        let project = Path::new("/Users/ada/code/reel");
        let path = home
            .join(".pi/agent/sessions/--Users-ada-code-reel--")
            .join("2026-10-12T01-00-01-191Z_pi-one.jsonl");
        write(
            &path,
            &[
                serde_json::json!({
                    "type": "session",
                    "id": "pi-one",
                    "cwd": "/Users/ada/code/reel",
                })
                .to_string(),
                serde_json::json!({
                    "type": "message",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "Fix the flaky test"}],
                    },
                })
                .to_string(),
                serde_json::json!({
                    "type": "message",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": "Done."}],
                    },
                })
                .to_string(),
            ],
        );

        let sessions = list_in(&Roots::from_home(&home), project);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.agent, SessionAgent::Pi);
        assert_eq!(session.id, "pi-one");
        assert_eq!(session.title, "Fix the flaky test");
        assert_eq!(session.preview, "Done.");
        assert_eq!(
            resume_command("pi", session),
            format!("pi --session {}", quote(&session.path.to_string_lossy()))
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_sessions_are_listed_by_their_rollout() {
        let home = home("codex");
        let project = Path::new("/Users/ada/code/reel");
        let path = home
            .join(".codex/sessions/2026/11/11")
            .join("rollout-2026-11-11T01-00-00-01a1.jsonl");
        write(
            &path,
            &[
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {
                        "session_id": "01a1",
                        "id": "01a1",
                        "cwd": "/Users/ada/code/reel",
                    },
                })
                .to_string(),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "text", "text": "Ship it"}],
                    },
                })
                .to_string(),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "Shipped."}],
                    },
                })
                .to_string(),
            ],
        );

        let sessions = list_in(&Roots::from_home(&home), project);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.agent, SessionAgent::Codex);
        assert_eq!(session.id, "01a1");
        assert_eq!(session.title, "Ship it");
        assert_eq!(session.preview, "Shipped.");
        assert_eq!(resume_command("codex", session), "codex resume 01a1");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn opencode_sessions_are_listed_from_the_database() {
        let home = home("opencode");
        let project = Path::new("/Users/ada/code/reel");
        let db = home.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "create table session_v2 (
                    id text primary key,
                    directory text,
                    title text,
                    time_updated integer,
                    model text
                );
                create table session_message (
                    id text primary key,
                    session_id text,
                    type text,
                    time_created integer,
                    data text
                );",
            )
            .unwrap();
        connection
            .execute(
                "insert into session_v2 values ('ses_one', ?1, 'Make a rap video', 1000, null)",
                [project.to_string_lossy().as_ref()],
            )
            .unwrap();
        connection
            .execute(
                "insert into session_message values ('m1', 'ses_one', 'user', 900, '{\"text\":\"Make a rap video\"}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "insert into session_message values ('m2', 'ses_one', 'assistant', 950, '{\"content\":[{\"type\":\"text\",\"text\":\"On it.\"}]}')",
                [],
            )
            .unwrap();
        drop(connection);

        let sessions = list_in(&Roots::from_home(&home), project);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.agent, SessionAgent::OpenCode);
        assert_eq!(session.id, "ses_one");
        assert_eq!(session.cwd.as_path(), project);
        assert_eq!(session.title, "Make a rap video");
        assert_eq!(session.preview, "On it.");
        assert_eq!(session.path, db);
        assert_eq!(
            resume_command("opencode", session),
            "opencode --session ses_one"
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn sessions_of_another_project_are_ignored() {
        let home = home("filter");
        let project = Path::new("/Users/ada/code/reel");
        let path = home
            .join(".claude/projects/-Users-ada-code-other")
            .join("other.jsonl");
        write(
            &path,
            &[claude_line("user", "/Users/ada/code/other", "Not this one")],
        );

        assert!(list_in(&Roots::from_home(&home), project).is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_base_command_keeps_its_flags_only_for_its_own_agent() {
        assert_eq!(
            agent_base(
                "claude --dangerously-skip-permissions",
                SessionAgent::Claude
            ),
            "claude --dangerously-skip-permissions"
        );
        assert_eq!(
            agent_base("/opt/homebrew/bin/opencode", SessionAgent::OpenCode),
            "/opt/homebrew/bin/opencode"
        );
        assert_eq!(
            agent_base("claude --dangerously-skip-permissions", SessionAgent::Codex),
            "codex"
        );
        assert_eq!(agent_base("$SHELL", SessionAgent::Pi), "pi");
    }

    #[test]
    fn a_resume_path_with_spaces_is_one_shell_word() {
        let session = Session {
            agent: SessionAgent::Pi,
            id: "pi-one".into(),
            path: PathBuf::from("/Users/ada/my code/session.jsonl"),
            cwd: PathBuf::from("/Users/ada/my code"),
            title: "title".into(),
            preview: String::new(),
            updated: UNIX_EPOCH,
        };
        assert_eq!(
            resume_command("pi", &session),
            "pi --session '/Users/ada/my code/session.jsonl'"
        );
    }

    #[test]
    fn a_long_title_is_cut_to_one_line() {
        let text = "one\ntwo ".repeat(200);
        let cut = one_line(&text, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with('…'));
    }
}
