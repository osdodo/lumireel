//! The terminal session: PTY child + alacritty `Term` + an event channel.
//! Ported from syro-core's `term.rs`, without syro's agent-status protocol and web-panel subscribers.

use std::io::{Read, Write};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Processor, Rgb};
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

/// Grid size; implements alacritty's `Dimensions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl TermSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }

    pub fn pty_size(&self) -> PtySize {
        PtySize {
            rows: self.screen_lines as u16,
            cols: self.columns as u16,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// What to start. `command` is a program name looked up in `PATH`.
#[derive(Debug, Clone)]
pub struct TermSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: std::path::PathBuf,
    /// Extra environment on top of the inherited one. `TERM` is always set.
    pub env: Vec<(String, String)>,
    pub size: TermSize,
    /// Lines of scrollback kept above the viewport.
    pub scrollback: usize,
}

impl TermSpec {
    /// The platform's interactive shell in `cwd`.
    pub fn shell(cwd: impl Into<std::path::PathBuf>, size: TermSize) -> Self {
        let command = crate::platform::default_shell();
        Self {
            command,
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            size,
            scrollback: 10_000,
        }
    }
}

/// Something the terminal wants the front end to know about.
#[derive(Debug, Clone, PartialEq)]
pub enum TermEvent {
    /// New bytes reached the grid: repaint. Coalesced by the reader thread.
    Wakeup,
    Title(String),
    ResetTitle,
    Bell,
    /// `OSC 52` copy: the child asked to put `text` on the clipboard.
    ClipboardStore(String),
    /// The child exited. Emitted once, reaped by `drain_events`.
    Exit(ExitStatus),
    /// The PTY itself failed. The session is dead after.
    Error(String),
}

/// The terminal's view of the front end, seen only inside [`TermSession::with_term`].
pub struct EventProxy {
    events: Sender<TermEvent>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    size: Arc<Mutex<TermSize>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: alacritty_terminal::event::Event) {
        use alacritty_terminal::event::Event;
        let translated = match event {
            Event::Wakeup => TermEvent::Wakeup,
            Event::Title(title) => TermEvent::Title(title),
            Event::ResetTitle => TermEvent::ResetTitle,
            Event::Bell => TermEvent::Bell,
            Event::ClipboardStore(_, text) => TermEvent::ClipboardStore(text),
            Event::Exit => return,
            // Answered here, so the app has nothing to do.
            Event::PtyWrite(text) => {
                let _ = self.write_to_child(text.as_bytes());
                return;
            }
            Event::TextAreaSizeRequest(format) => {
                let size = *self.size.lock().unwrap();
                let report = format(WindowSize {
                    num_lines: size.screen_lines as u16,
                    num_cols: size.columns as u16,
                    cell_width: 0,
                    cell_height: 0,
                });
                let _ = self.write_to_child(report.as_bytes());
                return;
            }
            // An OSC 52 read gets an empty answer rather than a hang.
            Event::ClipboardLoad(..) => {
                let _ = self.write_to_child(b"\x1b]52;c;\x07");
                return;
            }
            Event::ColorRequest(_index, format) => {
                let report = format(Rgb { r: 0, g: 0, b: 0 });
                let _ = self.write_to_child(report.as_bytes());
                return;
            }
            Event::MouseCursorDirty | Event::CursorBlinkingChange | Event::ChildExit(_) => return,
        };
        let _ = self.events.send(translated);
    }
}

impl EventProxy {
    fn write_to_child(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }
}

#[cfg(unix)]
fn std_status(status: portable_pty::ExitStatus) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw((status.exit_code() << 8) as i32)
}

#[cfg(windows)]
fn std_status(status: portable_pty::ExitStatus) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(status.exit_code())
}

/// A live terminal: PTY child + grid + event channel.
pub struct TermSession {
    child: Mutex<Box<dyn Child + Send + Sync>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    term: Arc<FairMutex<Term<EventProxy>>>,
    size: Arc<Mutex<TermSize>>,
    events: Receiver<TermEvent>,
    eof: Arc<AtomicBool>,
    exit_reported: AtomicBool,
}

/// The locale a child gets when the app has none. A `.app` launched from Finder
/// inherits no `LANG`, and a shell in the `C` locale shreds every multi-byte character.
fn fallback_locale(
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<(&'static str, &'static str)> {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .all(|name| lookup(name).is_none_or(|value| value.is_empty()))
        .then_some(("LANG", "en_US.UTF-8"))
}

impl TermSession {
    /// Start `spec` and begin draining its output. A command that does not exist fails here.
    pub fn spawn(spec: TermSpec) -> Result<Self> {
        let pty = portable_pty::native_pty_system()
            .openpty(spec.size.pty_size())
            .context("open the pty")?;

        let mut command = CommandBuilder::new(&spec.command);
        command.args(&spec.args);
        command.cwd(&spec.cwd);
        command.env("TERM", "xterm-256color");
        if let Some((name, value)) = fallback_locale(|name| std::env::var_os(name)) {
            command.env(name, value);
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let child = pty
            .slave
            .spawn_command(command)
            .with_context(|| format!("spawn {:?}", spec.command))?;
        // The parent's copy of the slave would keep the pty open and hide EOF from the master.
        drop(pty.slave);

        let mut reader = pty
            .master
            .try_clone_reader()
            .context("clone the pty reader")?;
        let writer = pty.master.take_writer().context("take the pty writer")?;

        let (events, incoming) = std::sync::mpsc::channel();
        let writer = Arc::new(Mutex::new(writer));
        let size = Arc::new(Mutex::new(spec.size));
        let eof = Arc::new(AtomicBool::new(false));
        let term = Arc::new(FairMutex::new(Term::new(
            TermConfig {
                scrolling_history: spec.scrollback,
                ..TermConfig::default()
            },
            &spec.size,
            EventProxy {
                events: events.clone(),
                writer: Arc::clone(&writer),
                size: Arc::clone(&size),
            },
        )));

        let reader_term = Arc::clone(&term);
        let reader_eof = Arc::clone(&eof);
        std::thread::Builder::new()
            .name("lumireel-term-reader".to_string())
            .spawn(move || {
                let mut processor: Processor = Processor::new();
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(bytes) => {
                            let mut terminal = reader_term.lock();
                            processor.advance(&mut *terminal, &buffer[..bytes]);
                            // Released before the send: the UI paints under this lock.
                            drop(terminal);
                            if events.send(TermEvent::Wakeup).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = events.send(TermEvent::Error(error.to_string()));
                            break;
                        }
                    }
                }
                // The exit is collected, not sent: `drain_events` reaps the child.
                reader_eof.store(true, Ordering::Release);
                let _ = events.send(TermEvent::Wakeup);
            })
            .context("start the terminal reader thread")?;

        Ok(Self {
            child: Mutex::new(child),
            master: Mutex::new(pty.master),
            writer,
            term,
            size,
            events: incoming,
            eof,
            exit_reported: AtomicBool::new(false),
        })
    }

    /// Bytes from the keyboard, straight to the child; the caller encodes the key.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resize both the PTY and the grid.
    pub fn resize(&self, columns: usize, screen_lines: usize) -> Result<()> {
        let size = TermSize::new(columns, screen_lines);
        // The pty first: a TUI reading `stty size` on SIGWINCH must not see a size its grid is not yet.
        self.master
            .lock()
            .unwrap()
            .resize(size.pty_size())
            .context("resize the pty")?;
        self.term.lock().resize(size);
        *self.size.lock().unwrap() = size;
        Ok(())
    }

    /// Scroll the viewport: positive is up into the scrollback.
    pub fn scroll(&self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    /// Whether the child asked for mouse reports (`?1000` / `?1002` / `?1003`).
    pub fn wants_mouse(&self) -> bool {
        self.term.lock().mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Whether a held-button drag (`?1002`) or all motion (`?1003`) is reported.
    pub fn mouse_reports_drag(&self) -> bool {
        self.term
            .lock()
            .mode()
            .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
    }

    /// Whether the child is on the alternate screen, which has no scrollback.
    pub fn on_alternate_screen(&self) -> bool {
        self.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }

    /// One wheel notch in the protocol the child negotiated, at `(column, line)`.
    pub fn wheel_report(
        &self,
        up: bool,
        column: usize,
        line: usize,
        modifiers: MouseModifiers,
    ) -> Vec<u8> {
        let button = if up {
            MouseButton::WheelUp
        } else {
            MouseButton::WheelDown
        };
        encode_mouse(
            *self.term.lock().mode(),
            MouseReport::Press(button),
            column,
            line,
            modifiers,
        )
    }

    /// One mouse event in the protocol the child negotiated, at `(column, line)`.
    pub fn mouse_report(
        &self,
        event: MouseReport,
        column: usize,
        line: usize,
        modifiers: MouseModifiers,
    ) -> Vec<u8> {
        encode_mouse(*self.term.lock().mode(), event, column, line, modifiers)
    }

    /// The current grid size, as last set by `spawn` or `resize`.
    pub fn size(&self) -> TermSize {
        *self.size.lock().unwrap()
    }

    /// Read the grid with the `Term` locked.
    pub fn with_term<R>(&self, f: impl FnOnce(&mut Term<EventProxy>) -> R) -> R {
        f(&mut self.term.lock())
    }

    /// Everything since the last call, without blocking. Reaps a child at EOF and emits `Exit` once.
    pub fn drain_events(&self) -> Vec<TermEvent> {
        let mut events: Vec<TermEvent> = self.events.try_iter().collect();
        if self.eof.load(Ordering::Acquire) && !self.exit_reported.load(Ordering::Acquire) {
            let status = self.child.lock().unwrap().try_wait();
            if let Ok(Some(status)) = status {
                self.exit_reported.store(true, Ordering::Release);
                events.push(TermEvent::Exit(std_status(status)));
            }
        }
        events
    }

    /// The visible grid as plain text, for tests.
    pub fn snapshot(&self) -> TermSnapshot {
        self.with_term(|term| {
            let mode = *term.mode();
            let content = term.renderable_content();
            let display_offset = content.display_offset;
            let cursor = content.cursor;
            let grid = term.grid();
            let columns = grid.columns();
            let screen_lines = grid.screen_lines();

            let mut lines = Vec::with_capacity(screen_lines);
            for screen_line in 0..screen_lines {
                let row = &grid[Line(screen_line as i32 - display_offset as i32)];
                let mut text = String::with_capacity(columns);
                for column in 0..columns {
                    let cell = &row[Column(column)];
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    text.push(cell.c);
                }
                lines.push(text.trim_end().to_string());
            }

            TermSnapshot {
                columns,
                screen_lines,
                lines,
                cursor: (cursor.point.line.0.max(0) as usize, cursor.point.column.0),
                cursor_visible: mode.contains(TermMode::SHOW_CURSOR)
                    && !matches!(cursor.shape, CursorShape::Hidden),
                alt_screen: mode.contains(TermMode::ALT_SCREEN),
                display_offset,
            }
        })
    }

    /// Signal the child to stop and reap it. Idempotent.
    pub fn kill(&mut self) {
        let mut child = self.child.lock().unwrap();
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for TermSession {
    fn drop(&mut self) {
        // The reader thread holds a clone of the pty master, so dropping alone would leave the child running.
        self.kill();
    }
}

/// A plain-text picture of the grid, for tests — not a render path.
#[derive(Debug, Clone, PartialEq)]
pub struct TermSnapshot {
    pub columns: usize,
    pub screen_lines: usize,
    pub lines: Vec<String>,
    pub cursor: (usize, usize),
    pub cursor_visible: bool,
    pub alt_screen: bool,
    pub display_offset: usize,
}

impl TermSnapshot {
    /// The whole grid joined with `\n`, trailing blank lines dropped.
    pub fn text(&self) -> String {
        let mut end = self.lines.len();
        while end > 0 && self.lines[end - 1].is_empty() {
            end -= 1;
        }
        self.lines[..end].join("\n")
    }
}

/// The buttons xterm gives a number to; the wheel is 64/65 in the same table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    WheelUp,
    WheelDown,
}

/// One mouse event, for a child that asked to be told about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseReport {
    Press(MouseButton),
    Release(MouseButton),
    /// The pointer moved; `None` is a bare move, which only `?1003` asks for.
    Motion(Option<MouseButton>),
}

/// The modifiers a mouse report can express (4 shift, 8 alt, 16 control).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// xterm's encoding of one mouse event, in SGR (`?1006`), UTF-8 (`?1005`) or the original form.
fn encode_mouse(
    mode: TermMode,
    event: MouseReport,
    column: usize,
    line: usize,
    modifiers: MouseModifiers,
) -> Vec<u8> {
    let sgr = mode.contains(TermMode::SGR_MOUSE);
    let released = matches!(event, MouseReport::Release(_));
    let mut code = match event {
        MouseReport::Press(button) => button_code(button),
        MouseReport::Release(button) if sgr => button_code(button),
        MouseReport::Release(_) => 3,
        MouseReport::Motion(held) => 32 + held.map(button_code).unwrap_or(3),
    };
    if modifiers.shift {
        code += 4;
    }
    if modifiers.alt {
        code += 8;
    }
    if modifiers.control {
        code += 16;
    }
    let column = column + 1;
    let line = line + 1;

    if sgr {
        let final_byte = if released { 'm' } else { 'M' };
        format!("\x1b[<{code};{column};{line}{final_byte}").into_bytes()
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        let mut bytes = b"\x1b[M".to_vec();
        bytes.push((code + 32) as u8);
        for value in [32 + column, 32 + line] {
            let mut buffer = [0u8; 4];
            let encoded = char::from_u32(value as u32)
                .unwrap_or('?')
                .encode_utf8(&mut buffer);
            bytes.extend_from_slice(encoded.as_bytes());
        }
        bytes
    } else {
        vec![
            0x1b,
            b'[',
            b'M',
            (code + 32) as u8,
            (32 + column).min(255) as u8,
            (32 + line).min(255) as u8,
        ]
    }
}

fn button_code(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(mut check: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn shell_script(script: &str) -> TermSession {
        #[cfg(unix)]
        let (command, args) = ("/bin/sh", vec!["-c".to_string(), script.to_string()]);
        #[cfg(windows)]
        let (command, args) = (
            "powershell.exe",
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                script.into(),
            ],
        );
        let spec = TermSpec {
            command: command.to_string(),
            args,
            ..TermSpec::shell(std::env::temp_dir(), TermSize::new(80, 24))
        };
        TermSession::spawn(spec).expect("spawn")
    }

    #[test]
    fn a_child_gets_a_utf8_locale_only_when_the_app_has_none() {
        assert_eq!(fallback_locale(|_| None), Some(("LANG", "en_US.UTF-8")));
        let c = |_: &str| Some(std::ffi::OsString::from("C"));
        assert_eq!(fallback_locale(c), None);
    }

    #[test]
    fn output_reaches_the_grid_and_typing_reaches_the_child() {
        let session = shell_script(if cfg!(windows) {
            "[Console]::WriteLine('hello world'); while ($null -ne ($line = [Console]::ReadLine())) { [Console]::WriteLine($line) }"
        } else {
            "printf 'hello world\\r\\n'; cat"
        });
        assert!(wait_for(|| session
            .snapshot()
            .text()
            .contains("hello world")));
        session.write(b"typed input\r").expect("write");
        assert!(wait_for(|| session
            .snapshot()
            .text()
            .contains("typed input")));
    }

    #[test]
    fn portable_exit_codes_keep_their_platform_meaning() {
        for code in [0, 3, 127, 255] {
            let status = std_status(portable_pty::ExitStatus::with_exit_code(code));
            assert_eq!(status.code(), Some(code as i32));
            assert_eq!(status.success(), code == 0);
        }
        #[cfg(windows)]
        assert_eq!(
            std_status(portable_pty::ExitStatus::with_exit_code(0xc0000005)).code(),
            Some(0xc0000005_u32 as i32)
        );
    }

    #[test]
    fn exit_status_is_reported_once() {
        let session = shell_script("exit 3");
        let mut exits = Vec::new();
        assert!(wait_for(|| {
            for event in session.drain_events() {
                if let TermEvent::Exit(status) = event {
                    exits.push(status);
                }
            }
            !exits.is_empty()
        }));
        assert_eq!(exits.len(), 1);
        assert_eq!(exits[0].code(), Some(3));
    }

    #[test]
    fn resize_reaches_the_grid() {
        let session = shell_script(if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        });
        session.resize(40, 10).expect("resize");
        assert_eq!(session.size(), TermSize::new(40, 10));
        assert_eq!(session.snapshot().columns, 40);
    }

    #[test]
    fn mouse_reports_follow_the_negotiated_protocol() {
        let session = shell_script(if cfg!(windows) {
            "[Console]::Write(([char]27) + '[?1002h' + ([char]27) + '[?1006h'); Start-Sleep -Seconds 30"
        } else {
            "printf '\\033[?1002h\\033[?1006h'; sleep 30"
        });
        assert!(wait_for(|| session.wants_mouse()));
        assert_eq!(
            session.wheel_report(true, 0, 0, MouseModifiers::default()),
            b"\x1b[<64;1;1M"
        );
        assert_eq!(
            session.mouse_report(
                MouseReport::Release(MouseButton::Left),
                3,
                2,
                MouseModifiers::default()
            ),
            b"\x1b[<0;4;3m"
        );
    }

    #[test]
    fn wide_characters_keep_their_columns() {
        let session = shell_script(if cfg!(windows) {
            "[Console]::WriteLine(([char]0x4E2D).ToString() + [char]0x6587)"
        } else {
            "printf '\\344\\270\\255\\346\\226\\207\\r\\n'"
        });
        assert!(wait_for(|| session.snapshot().text().contains("中文")));
        assert_eq!(session.snapshot().cursor, (1, 0));
    }
}
