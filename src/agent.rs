use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::{
    history, library,
    server::ServerHandle,
    terminal::{TermSession, TermSize, TermSpec, TerminalEvent, TerminalView},
};

const PRESETS: &[&str] = &["pi", "claude", "codex", "opencode", "$SHELL"];

pub struct AgentPanel {
    server: ServerHandle,
    /// The command a project gets before it has one of its own: the last one picked anywhere.
    command: String,
    /// Each project's own command (`.reel/agent`), read once.
    commands: HashMap<PathBuf, String>,
    sessions: HashMap<PathBuf, Entity<TerminalView>>,
    active: Option<PathBuf>,
    error: Option<String>,
}

impl AgentPanel {
    pub fn new(server: ServerHandle, command: String) -> Self {
        Self {
            server,
            command,
            commands: HashMap::new(),
            sessions: HashMap::new(),
            active: None,
            error: None,
        }
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    /// The command `dir` runs: its own, else the default — which is then pinned to it, so a later
    /// pick in another project does not change what this one starts.
    fn command_for(&mut self, dir: &Path) -> String {
        if let Some(command) = self.commands.get(dir) {
            return command.clone();
        }
        let command = library::saved_agent(dir).unwrap_or_else(|| {
            if let Err(error) = library::save_agent(dir, &self.command) {
                log::warn!("could not save the agent for {}: {error}", dir.display());
            }
            self.command.clone()
        });
        self.commands.insert(dir.to_path_buf(), command.clone());
        command
    }

    /// Switch the open project's agent: saved to the project, and restarted with it.
    fn set_command(&mut self, command: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.active.clone() else {
            return;
        };
        // Also the default for projects that have not picked one yet.
        self.command = command.clone();
        if self.command_for(&dir) == command {
            return;
        }
        if let Err(error) = library::save_agent(&dir, &command) {
            self.error = Some(format!("could not save the agent: {error}"));
        }
        self.commands.insert(dir, command);
        self.restart_active(window, cx);
    }

    /// The project went away: end its agent. Its history stays with the agent, so a project
    /// restored from the Trash still finds its sessions.
    pub fn forget(&mut self, dir: &Path, cx: &mut Context<Self>) {
        self.sessions.remove(dir);
        self.commands.remove(dir);
        if self.active.as_deref() == Some(dir) {
            self.active = None;
        }
        cx.notify();
    }

    /// Show the project's terminal, starting an agent the first time.
    pub fn show(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.active = Some(dir.to_path_buf());
        if !self.sessions.contains_key(dir) {
            self.start(dir, window, cx);
        }
        self.focus_active(window, cx);
    }

    fn restart_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dir) = self.active.clone() {
            self.start(&dir, window, cx);
            self.focus_active(window, cx);
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active.as_ref().and_then(|dir| self.sessions.get(dir)) {
            let focus = view.read(cx).focus_handle().clone();
            focus.focus(window, cx);
        }
    }

    /// (Re)start the agent for `dir`; the old view, and with it the old child, is dropped.
    fn start(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let command = self.command_for(dir);
        self.start_with(dir, &command, window, cx);
    }

    /// The same, with the exact shell command to run: a resume is `start` with the agent's
    /// resume flags appended.
    fn start_with(
        &mut self,
        dir: &Path,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        self.sessions.remove(dir);
        let session = match TermSession::spawn(self.spec(dir, command)) {
            Ok(session) => session,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        let view = cx.new(|cx| TerminalView::new(cx, session));
        let dir_for_events = dir.to_path_buf();
        cx.subscribe_in(
            &view,
            window,
            move |this, _, event: &TerminalEvent, window, cx| match event {
                TerminalEvent::Exited(_) => cx.notify(),
                TerminalEvent::RestartRequested => {
                    this.start(&dir_for_events, window, cx);
                    this.focus_active(window, cx);
                }
            },
        )
        .detach();
        self.sessions.insert(dir.to_path_buf(), view);
        cx.notify();
    }

    /// Re-enter a past session: the project switches to the session's agent, and the terminal
    /// starts with that agent's resume flags. The next plain start stays a new session.
    fn resume_session(
        &mut self,
        dir: &Path,
        session: &history::Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.command_for(dir);
        let base = history::agent_base(&current, session.agent);
        if let Err(error) = library::save_agent(dir, &base) {
            self.error = Some(format!("could not save the agent: {error}"));
        }
        self.commands.insert(dir.to_path_buf(), base.clone());
        let command = history::resume_command(&base, session);
        self.start_with(dir, &command, window, cx);
        self.focus_active(window, cx);
    }

    /// `$SHELL -i -l -c <command>` (or a bare login shell for `$SHELL`), with lumireel's env and `reel` on PATH.
    fn spec(&self, dir: &Path, command: &str) -> TermSpec {
        let mut spec = TermSpec::shell(dir, TermSize::new(100, 30));
        let trimmed = command.trim();
        if trimmed.is_empty() || trimmed == "$SHELL" {
            spec.args = vec!["-l".into()];
        } else {
            // -i: tools installed via nvm etc. are only on PATH after ~/.zshrc, which -l alone skips.
            spec.args = vec!["-i".into(), "-l".into(), "-c".into(), trimmed.into()];
        }
        let bin_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let path = std::env::var("PATH").unwrap_or_default();
        spec.env = vec![
            ("COLORTERM".into(), "truecolor".into()),
            ("LUMIREEL_URL".into(), self.server.base_url.clone()),
            ("LUMIREEL_TOKEN".into(), self.server.token.clone()),
            ("LUMIREEL_PROJECT".into(), dir.display().to_string()),
            ("PATH".into(), format!("{}:{path}", bin_dir.display())),
            ("LUMIREEL_BIN".into(), bin_dir.display().to_string()),
        ];
        spec
    }

    /// Paste a reference block into the active agent without submitting it.
    pub fn paste(&mut self, text: &str, window: &mut Window, cx: &mut App) {
        let Some(view) = self.active.as_ref().and_then(|dir| self.sessions.get(dir)) else {
            return;
        };
        view.read(cx).insert_text(text);
        let focus = view.read(cx).focus_handle().clone();
        focus.focus(window, cx);
    }

    pub fn shutdown(&mut self) {
        self.sessions.clear();
    }

    fn is_exited(&self, dir: &Path, cx: &App) -> bool {
        self.sessions
            .get(dir)
            .is_some_and(|view| view.read(cx).exit_status().is_some())
    }
}

impl Render for AgentPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let current = match self.active.clone() {
            Some(dir) => self.command_for(&dir),
            None => self.command.clone(),
        };
        let active = self.active.clone();
        let exited = active.as_deref().is_some_and(|dir| self.is_exited(dir, cx));
        let error = self.error.clone();
        let history_button = {
            let weak = weak.clone();
            let dir = active.clone();
            Button::new("history")
                .xsmall()
                .ghost()
                .icon(IconName::Inbox)
                .tooltip(t!("agent.history").to_string())
                .disabled(dir.is_none())
                .dropdown_menu(move |mut menu, _, _| {
                    let Some(dir) = dir.clone() else {
                        return menu;
                    };
                    let sessions = history::list(&dir);
                    let now = SystemTime::now();
                    if sessions.is_empty() {
                        return menu.item(
                            PopupMenuItem::new(t!("agent.no_history").to_string()).disabled(true),
                        );
                    }
                    for session in sessions {
                        let weak = weak.clone();
                        let dir = dir.clone();
                        let title = session.title.clone();
                        let preview = session.preview.clone();
                        let meta = format!(
                            "{} · {}",
                            session.agent.display(),
                            library::ago(session.updated, now)
                        );
                        let session_for_click = session;
                        let item = PopupMenuItem::element(move |_, cx| {
                            v_flex()
                                .w(px(320.))
                                .gap_0p5()
                                .child(div().text_sm().truncate().child(title.clone()))
                                .when(!preview.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .truncate()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(preview.clone()),
                                    )
                                })
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(meta.clone()),
                                )
                        })
                        .on_click(move |_, window, cx| {
                            if let Some(this) = weak.upgrade() {
                                this.update(cx, |this, cx| {
                                    this.resume_session(&dir, &session_for_click, window, cx)
                                });
                            }
                        });
                        menu = menu.item(item);
                    }
                    menu
                })
                .into_any_element()
        };

        let new_session_button = {
            let weak = weak.clone();
            let dir = active.clone();
            let command = current.clone();
            Button::new("new-session")
                .xsmall()
                .ghost()
                .icon(IconName::Plus)
                .tooltip(t!("agent.new_session").to_string())
                .disabled(dir.is_none())
                .on_click(move |_, window, cx| {
                    let Some(dir) = dir.clone() else {
                        return;
                    };
                    if let Some(this) = weak.upgrade() {
                        this.update(cx, |this, cx| {
                            this.start_with(&dir, &command, window, cx);
                            this.focus_active(window, cx);
                        });
                    }
                })
                .into_any_element()
        };

        let command_picker = {
            let weak = weak.clone();
            let current_for_menu = current.clone();
            Button::new("command")
                .xsmall()
                .ghost()
                .label(current.clone())
                .dropdown_caret(true)
                .tooltip(t!("agent.choose").to_string())
                .dropdown_menu(move |mut menu, _, _| {
                    for preset in PRESETS {
                        let weak = weak.clone();
                        menu = menu.item(
                            PopupMenuItem::new(*preset)
                                .checked(*preset == current_for_menu)
                                .on_click(move |_, window, cx| {
                                    if let Some(this) = weak.upgrade() {
                                        this.update(cx, |this, cx| {
                                            this.set_command(preset.to_string(), window, cx);
                                            cx.notify();
                                        });
                                    }
                                }),
                        );
                    }
                    menu
                })
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .h(px(36.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(command_picker)
                    .child(div().flex_1())
                    .child(history_button)
                    .child(new_session_button)
                    .child(
                        Button::new("restart")
                            .xsmall()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .tooltip(t!("agent.restart").to_string())
                            .disabled(active.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.restart_active(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .bg(cx.theme().danger.opacity(0.12))
                        .text_color(cx.theme().danger)
                        .text_xs()
                        .child(error),
                )
            })
            .when(exited, |this| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .bg(cx.theme().warning.opacity(0.14))
                        .text_color(cx.theme().warning)
                        .text_xs()
                        .child(t!("agent.exited_hint").to_string())
                        .child(
                            Button::new("restart-exited")
                                .xsmall()
                                .label(t!("common.restart").to_string())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.restart_active(window, cx);
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(if let Some(dir) = active {
                match self.sessions.get(&dir).cloned() {
                    Some(view) => {
                        let weak = weak.clone();
                        let has_selection = view.read(cx).selection_text().is_some();
                        let view_for_menu = view.clone();
                        div()
                            .id("agent-terminal")
                            .flex_1()
                            .min_h_0()
                            .child(view)
                            .context_menu(move |menu, _, _| {
                                let copy_view = view_for_menu.clone();
                                let paste_view = view_for_menu.clone();
                                let weak = weak.clone();
                                menu.item(
                                    PopupMenuItem::new(t!("agent.copy").to_string())
                                        .icon(IconName::Copy)
                                        .disabled(!has_selection)
                                        .on_click(move |_, _, cx| {
                                            copy_view
                                                .update(cx, |view, cx| view.copy_selection(cx));
                                        }),
                                )
                                .item(PopupMenuItem::new(t!("agent.paste").to_string()).on_click(
                                    move |_, _, cx| {
                                        paste_view.update(cx, |view, cx| view.paste(cx));
                                    },
                                ))
                                .separator()
                                .item(
                                    PopupMenuItem::new(t!("agent.restart").to_string())
                                        .icon(IconName::RefreshCw)
                                        .on_click(move |_, window, cx| {
                                            if let Some(this) = weak.upgrade() {
                                                this.update(cx, |this, cx| {
                                                    this.restart_active(window, cx)
                                                });
                                            }
                                        }),
                                )
                            })
                            .into_any_element()
                    }
                    None => div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("agent.starting", command = current).to_string())
                        .into_any_element(),
                }
            } else {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("agent.pick_project").to_string())
                    .into_any_element()
            })
    }
}
