use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, Theme, ThemeMode, TitleBar,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    menu::{ContextMenuExt as _, PopupMenuItem},
    resizable::{ResizableState, h_resizable, resizable_panel},
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rand::RngCore;

use lumireel::{
    agent::AgentPanel,
    capture::CaptureView,
    config::Config,
    i18n,
    library::{self, Project},
    preview::{PreviewEvent, PreviewPanel},
    server::{self, CaptureJob, ServerHandle},
    settings::{SettingsChanged, SettingsForm},
    ui::{dialog_footer, panel_header},
    watch,
};

rust_i18n::i18n!("locales", fallback = "en");

use rust_i18n::t;

struct LumiReel {
    config: Config,
    projects: Vec<Project>,
    selected: Option<PathBuf>,
    preview: Entity<PreviewPanel>,
    agent: Entity<AgentPanel>,
    capture: Entity<CaptureView>,
    new_project_input: Entity<InputState>,
    /// The three columns: kept so a window resize can refit them to the 16:9 stage.
    main_columns: Entity<ResizableState>,
    /// The viewport the columns were last fitted to.
    fitted_to: Option<Size<Pixels>>,
    server: ServerHandle,
    error: Option<String>,
    /// Project list thumbnails, with the cover file's mtime they were read at.
    covers: HashMap<PathBuf, (SystemTime, Arc<Image>)>,
    /// Projects whose cover is being drawn, so one change does not queue it twice.
    covers_pending: HashSet<PathBuf>,
    _library_watcher: Option<watch::ProjectWatcher>,
}

impl LumiReel {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        config: Config,
        server: ServerHandle,
        jobs: flume::Receiver<CaptureJob>,
    ) -> Self {
        let projects = library::list_projects(&config.library_dir);
        let preview = cx.new(|cx| PreviewPanel::new(window, cx, server.clone()));
        let agent = cx.new(|_| AgentPanel::new(server.clone(), config.agent_command.clone()));
        let capture = cx.new(|cx| CaptureView::new(window, cx));
        let new_project_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("projects.name_placeholder").to_string())
        });
        let main_columns = cx.new(|_| ResizableState::default());

        cx.subscribe_in(
            &preview,
            window,
            |this, _, event: &PreviewEvent, window, cx| match event {
                PreviewEvent::SendToAgent(text) => {
                    this.agent
                        .update(cx, |agent, cx| agent.paste(text, window, cx));
                }
                PreviewEvent::Ready { duration } => {
                    if let Some(path) = this.selected.clone() {
                        this.set_duration(&path, *duration, cx);
                        this.update_cover(&path, cx);
                    }
                }
            },
        )
        .detach();

        let capture_for_jobs = capture.clone();
        let server_for_jobs = server.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(job) = jobs.recv_async().await {
                let url = server_for_jobs.capture_url(&job.project, job.id);
                let capture = capture_for_jobs.clone();
                let result = this.update(cx, |_this, cx| {
                    capture.update(cx, |capture, cx| capture.load(job.id, &url, cx));
                });
                if result.is_err() {
                    break;
                }
                // One webview, one job: loading the next URL would kill this one mid-capture.
                while server_for_jobs.job_active(job.id) {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                }
            }
        })
        .detach();

        let library_watcher = match watch::watch_library(&config.library_dir) {
            Ok(watcher) => {
                let events = watcher.events.clone();
                cx.spawn(async move |this, cx| {
                    while events.recv_async().await.is_ok() {
                        if this
                            .update(cx, |this, cx| {
                                this.refresh_projects();
                                this.refresh_covers(cx);
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
                Some(watcher)
            }
            Err(error) => {
                log::warn!("could not watch library: {error:#}");
                None
            }
        };

        cx.on_app_quit(move |this, cx| {
            this.server.shutdown();
            this.agent.update(cx, |agent, _| agent.shutdown());
            let command = this.agent.read(cx).command().to_string();
            let mut config = this.config.clone();
            config.agent_command = command;
            let _ = config.save();
            async {}
        })
        .detach();

        let mut this = Self {
            config,
            projects,
            selected: None,
            preview,
            agent,
            capture,
            new_project_input,
            main_columns,
            fitted_to: None,
            server,
            error: None,
            covers: HashMap::new(),
            covers_pending: HashSet::new(),
            _library_watcher: library_watcher,
        };
        this.refresh_covers(cx);

        if let Some(project) = this.projects.first().cloned() {
            this.select_project(&project, window, cx);
        }
        this
    }

    fn refresh_projects(&mut self) {
        self.projects = library::list_projects(&self.config.library_dir);
        if let Some(selected) = &self.selected
            && !self
                .projects
                .iter()
                .any(|project| &project.path == selected)
        {
            self.selected = None;
        }
    }

    fn refresh_covers(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self.projects.iter().map(|p| p.path.clone()).collect();
        self.covers.retain(|path, _| paths.contains(path));
        for path in paths {
            self.update_cover(&path, cx);
        }
    }

    /// Remember the length the preview just reported, so the list shows it right away.
    fn set_duration(&mut self, path: &Path, seconds: f64, cx: &mut Context<Self>) {
        let seconds = (seconds * 1000.0).round() / 1000.0;
        if !seconds.is_finite() || seconds < 0.0 {
            return;
        }
        let duration = (seconds > 0.0).then_some(seconds);
        let current = self
            .projects
            .iter()
            .find(|project| project.path == path)
            .and_then(|project| project.duration);
        if current == duration {
            return;
        }
        if let Err(error) = library::save_duration(path, seconds) {
            log::warn!("saving duration for {}: {error:#}", path.display());
        }
        if let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.path == path)
        {
            project.duration = duration;
        }
        cx.notify();
    }

    /// Pick up a duration a capture just wrote for `path`.
    fn reload_duration(&mut self, path: &Path) {
        let duration = library::duration(path);
        if let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.path == path)
        {
            project.duration = duration;
        }
    }

    /// Show the project's cover, and redraw it (and measure the film) if either is missing.
    fn update_cover(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.load_cover(path);
        // A fresh cover is not proof the length is known: older projects have one but no duration.
        if self.covers_pending.contains(path)
            || (!library::cover_is_stale(path) && library::has_duration(path))
        {
            return;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            return;
        };
        self.covers_pending.insert(path.to_path_buf());
        let server = self.server.clone();
        let (done_tx, done_rx) = flume::bounded(1);
        // Blocks until the capture webview gets to it: its own thread.
        std::thread::spawn(move || {
            let _ = done_tx.send(server.cover(&name));
        });
        let path = path.to_path_buf();
        cx.spawn(async move |this, cx| {
            let result = done_rx.recv_async().await;
            let _ = this.update(cx, |this, cx| {
                this.covers_pending.remove(&path);
                match result {
                    Ok(Ok(_)) => {
                        this.load_cover(&path);
                        this.reload_duration(&path);
                    }
                    Ok(Err(error)) => log::warn!("cover for {}: {error:#}", path.display()),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// (Re)read the cover file if it changed since it was last read.
    fn load_cover(&mut self, path: &Path) {
        let file = library::cover_path(path);
        let Ok(modified) = file.metadata().and_then(|m| m.modified()) else {
            return;
        };
        if self
            .covers
            .get(path)
            .is_some_and(|(read_at, _)| *read_at == modified)
        {
            return;
        }
        if let Ok(bytes) = std::fs::read(&file) {
            let image = Arc::new(Image::from_bytes(ImageFormat::Png, bytes));
            self.covers.insert(path.to_path_buf(), (modified, image));
        }
    }

    fn select_project(&mut self, project: &Project, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(project.path.clone());
        self.preview
            .update(cx, |preview, cx| preview.load(project, cx));
        self.agent
            .update(cx, |agent, cx| agent.show(&project.path, window, cx));
        cx.notify();
    }

    fn open_new_project_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview
            .update(cx, |preview, cx| preview.hide_while_dialog_open(window, cx));
        self.new_project_input
            .update(cx, |input, cx| input.set_value("", window, cx));

        let weak = cx.entity().downgrade();
        let input = self.new_project_input.clone();
        let library_dir = self.config.library_dir.clone();
        let first_focus = Cell::new(true);

        window.open_dialog(cx, move |dialog, window, cx| {
            if first_focus.replace(false) {
                let focus = input.read(cx).focus_handle(cx).clone();
                focus.focus(window, cx);
            }
            let weak = weak.clone();
            let input = input.clone();
            dialog
                .title(t!("projects.new").to_string())
                .w(px(440.))
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(
                                    t!(
                                        "projects.create_in",
                                        dir = library_dir.display().to_string()
                                    )
                                    .to_string(),
                                ),
                        )
                        .child(Input::new(&input).bordered(false).focus_bordered(false)),
                )
                .footer(dialog_footer(
                    Button::new("create")
                        .primary()
                        .label(t!("projects.create").to_string()),
                ))
                .on_ok(move |_, window, cx| {
                    let name = input.read(cx).value().to_string();
                    let Some(this) = weak.upgrade() else {
                        return false;
                    };
                    this.update(cx, |this, cx| {
                        match library::create_project(&this.config.library_dir, &name) {
                            Ok(project) => {
                                this.refresh_projects();
                                this.error = None;
                                this.select_project(&project, window, cx);
                                true
                            }
                            Err(error) => {
                                this.error = Some(format!("{error:#}"));
                                cx.notify();
                                false
                            }
                        }
                    })
                })
        });
    }

    /// Language, library and the generation services; every edit is applied and saved at once.
    fn open_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview
            .update(cx, |preview, cx| preview.hide_while_dialog_open(window, cx));
        let form = cx.new(|cx| SettingsForm::new(window, cx, &self.config));
        cx.subscribe_in(&form, window, |this, form, _: &SettingsChanged, window, cx| {
            let services = form.read(cx).services(cx);
            let locale = form.read(cx).locale();
            this.config.services = services;
            this.set_locale(&locale, window, cx);
            match this.config.save() {
                Ok(()) => this.error = None,
                Err(error) => this.error = Some(format!("{error:#}")),
            }
            cx.notify();
        })
        .detach();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(t!("settings.title").to_string())
                .w(px(780.))
                .child(form.clone())
        });
    }

    /// Switch the interface language and make every panel draw the new one. The caller saves.
    fn set_locale(&mut self, locale: &str, window: &mut Window, cx: &mut Context<Self>) {
        if i18n::current() == locale {
            return;
        }
        i18n::apply(locale);
        self.config.locale = Some(locale.to_string());
        // The placeholder is built once with the input, so rebuild it for the new language.
        self.new_project_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("projects.name_placeholder").to_string())
        });
        // GPUI replays retained subtrees, so each view that draws text has to be told.
        self.preview.update(cx, |preview, cx| {
            preview.sync_locale(cx);
            cx.notify();
        });
        self.agent.update(cx, |_, cx| cx.notify());
        self.capture.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    fn confirm_trash(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.preview
            .update(cx, |preview, cx| preview.hide_while_dialog_open(window, cx));
        let weak = cx.entity().downgrade();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let title = t!("projects.trash_title", name = name).to_string();
        let body = t!("projects.trash_body").to_string();
        let trash_label = t!("projects.trash").to_string();
        window.open_dialog(cx, move |dialog, _, cx| {
            let weak = weak.clone();
            let path = path.clone();
            dialog
                .title(title.clone())
                .w(px(420.))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(body.clone()),
                )
                .footer(dialog_footer(
                    Button::new("trash").danger().label(trash_label.clone()),
                ))
                .on_ok(move |_, window, cx| {
                    if let Some(this) = weak.upgrade() {
                        this.update(cx, |this, cx| this.trash_project(&path, window, cx));
                    }
                    true
                })
        });
    }

    fn trash_project(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        // The agent's cwd is about to disappear: end it first.
        self.agent.update(cx, |agent, cx| agent.forget(path, cx));
        if let Err(error) = library::trash(path) {
            self.error = Some(format!("{error:#}"));
            cx.notify();
            return;
        }
        self.error = None;
        let was_selected = self.selected.as_deref() == Some(path);
        self.refresh_projects();
        if was_selected {
            match self.projects.first().cloned() {
                Some(next) => self.select_project(&next, window, cx),
                None => {
                    self.selected = None;
                    self.preview.update(cx, |preview, cx| preview.clear(cx));
                }
            }
        }
        cx.notify();
    }

    /// Run `f` on the project at `path`, if it is still in the list.
    fn with_project(
        weak: &WeakEntity<Self>,
        path: &Path,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut Self, Project, &mut Window, &mut Context<Self>),
    ) {
        let Some(this) = weak.upgrade() else {
            return;
        };
        this.update(cx, |this, cx| {
            if let Some(project) = this.projects.iter().find(|p| p.path == path).cloned() {
                f(this, project, window, cx);
            }
        });
    }

    fn projects_panel(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        type Action = fn(&mut LumiReel, Project, &mut Window, &mut Context<LumiReel>);
        let now = std::time::SystemTime::now();
        let rows = self.projects.iter().enumerate().map(|(index, project)| {
            let active = self.selected.as_ref() == Some(&project.path);
            let path = project.path.clone();
            let weak = cx.entity().downgrade();
            let menu_weak = weak.clone();
            let menu_path = path.clone();
            let preview = self.preview.clone();
            h_flex()
                .id(("project", index))
                .px_2()
                .py_1p5()
                .gap_2()
                .items_center()
                .rounded(cx.theme().radius)
                .cursor_pointer()
                .when(active, |this| this.bg(cx.theme().sidebar_accent))
                .when(!active, |this| {
                    this.hover(|this| this.bg(cx.theme().sidebar_accent.opacity(0.5)))
                })
                .child(
                    div()
                        .flex_none()
                        .w(px(64.))
                        .h(px(36.))
                        .rounded(px(4.))
                        .overflow_hidden()
                        .bg(cx.theme().muted)
                        .border_1()
                        .border_color(if active {
                            cx.theme().primary.opacity(0.6)
                        } else {
                            cx.theme().border
                        })
                        .relative()
                        .flex()
                        .items_center()
                        .justify_center()
                        .map(|this| match self.covers.get(&project.path) {
                            Some((_, image)) => this
                                .child(img(image.clone()).size_full().object_fit(ObjectFit::Cover)),
                            None => this.child(
                                Icon::new(IconName::Folder)
                                    .small()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                        })
                        .when_some(project.duration, |this, duration| {
                            this.child(
                                div()
                                    .absolute()
                                    .right(px(2.))
                                    .bottom(px(2.))
                                    .px(px(3.))
                                    .py(px(1.))
                                    .rounded(px(2.))
                                    .bg(cx.theme().background.opacity(0.78))
                                    .text_size(px(9.))
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().foreground)
                                    .child(library::duration_label(duration)),
                            )
                        }),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_sm()
                                .truncate()
                                .when(active, |this| this.font_medium())
                                .child(project.name.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(library::ago(project.modified, now)),
                        ),
                )
                .on_click(move |_, window, cx| {
                    Self::with_project(&weak, &path, window, cx, |this, project, window, cx| {
                        this.error = None;
                        this.select_project(&project, window, cx);
                    });
                })
                .context_menu(move |menu, window, cx| {
                    PreviewPanel::hide_under_menu(preview.clone(), window, cx);

                    let item = |label: String, icon: Option<IconName>, action: Action| {
                        let weak = menu_weak.clone();
                        let path = menu_path.clone();
                        let item = PopupMenuItem::new(label).on_click(move |_, window, cx| {
                            Self::with_project(&weak, &path, window, cx, action);
                        });
                        match icon {
                            Some(icon) => item.icon(icon),
                            None => item,
                        }
                    };
                    menu.item(item(
                        t!("common.reveal").to_string(),
                        Some(IconName::FolderOpen),
                        |_, project, _, _| library::reveal_in_finder(&project.path),
                    ))
                    .separator()
                    .item(item(
                        t!("projects.trash").to_string(),
                        Some(IconName::Delete),
                        |this, project, window, cx| this.confirm_trash(project.path, window, cx),
                    ))
                })
        });

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                panel_header(t!("projects.title").to_string(), cx).child(
                    Button::new("new-project")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip(t!("projects.new").to_string())
                        // The preview is a native view at the right edge; keep hints inside the list.
                        .tooltip_placement(gpui_kit::component::Placement::Left)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_new_project_dialog(window, cx);
                        })),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .overflow_y_scrollbar()
                    .when(self.projects.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .items_center()
                                .gap_3()
                                .pt_8()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("projects.empty").to_string())
                                .child(
                                    Button::new("new-project-empty")
                                        .small()
                                        .primary()
                                        .icon(IconName::Plus)
                                        .label(t!("projects.new").to_string())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_new_project_dialog(window, cx);
                                        })),
                                ),
                        )
                    })
                    .child(v_flex().gap_0p5().children(rows)),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .mx_2()
                        .mb_2()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().danger.opacity(0.12))
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .into_any_element()
    }
}

/// Height the title bar, the preview header and the playback controls take before the stage:
/// the stage and the asset shelf split what is left.
const STAGE_CHROME: f32 = 34. + 36. + 40.;

/// A 16:9 film should fill the stage exactly: size the side panels around the width that
/// height needs, so the first frame opens with no letterboxing.
fn default_panel_sizes(viewport: Size<Pixels>) -> (Pixels, Pixels) {
    let width = f32::from(viewport.width);
    let height = f32::from(viewport.height);
    let stage_height = ((height - STAGE_CHROME) * 0.5).max(160.0);
    let centre = stage_height * 16.0 / 9.0;
    let sides = (width - centre).max(360.0);
    // The project list stays narrow; the Agent gets twice the remaining side space.
    let projects = sides / 3.0;
    (px(projects), px(sides - projects))
}

impl Render for LumiReel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let project = self
            .selected
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string());
        let viewport = window.viewport_size();
        let (projects_width, agent_width) = default_panel_sizes(viewport);
        // Resizing the window scales the columns proportionally, but the title bar, header and
        // controls keep their height: refit the sides so the stage stays 16:9.
        if self.fitted_to != Some(viewport) {
            self.fitted_to = Some(viewport);
            let columns = self.main_columns.clone();
            window.defer(cx, move |window, cx| {
                columns.update(cx, |state, cx| {
                    if state.sizes().len() == 3 {
                        state.resize_panel(0, projects_width, window, cx);
                        state.resize_panel(2, agent_width, window, cx);
                    }
                });
                // The state changed after this frame drew: ask for the one that lays it out.
                window.refresh();
            });
        }
        // Top-right of the title bar, aligned with the window controls.
        let settings_button = Button::new("settings")
            .ghost()
            .xsmall()
            .icon(IconName::Settings)
            .tooltip(t!("settings.title").to_string())
            .on_click(cx.listener(|this, _, window, cx| this.open_settings_dialog(window, cx)));

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .flex_1()
                        .h_full()
                        .pr_2()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .text_sm()
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap_2()
                                .items_center()
                                .child(div().font_semibold().child("lumireel"))
                                .when_some(project, |this, project| {
                                    this.child(
                                        div().text_color(cx.theme().muted_foreground).child("/"),
                                    )
                                    .child(div().truncate().child(project))
                                }),
                        )
                        .child(div().flex_none().child(settings_button)),
                ),
            )
            .child(
                h_resizable("main")
                    .with_state(&self.main_columns)
                    .child(
                        resizable_panel()
                            .size(projects_width)
                            .child(self.projects_panel(window, cx)),
                    )
                    .child(resizable_panel().child(self.preview.clone()))
                    .child(
                        resizable_panel()
                            .size(agent_width)
                            .child(self.agent.clone()),
                    ),
            )
            .child(self.capture.clone())
    }
}

/// `app::Quit` — `cmd-q`. gpui-kit leaves quitting to the application: the close button only
/// hides, so without this nothing would end the process.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = app, no_json)]
pub struct Quit;

/// What the close button does. On macOS it hides the app instead of closing the window, so the
/// agent terminals keep running and the Dock icon brings the window back. Linux and Windows have
/// no Dock icon for a hidden window: hiding there would leave a process with no window and no way
/// to end it, so the window closes and — being the last one — quits the app.
fn hide_on_close(window: &mut Window, cx: &mut App) {
    if cfg!(target_os = "macos") {
        window.on_window_should_close(cx, |_, cx| {
            cx.hide();
            false
        });
    }
}

/// The Dock icon (or a second launch) while the window is not on screen: bring it back.
fn reopen_windows(cx: &mut App) {
    cx.activate(true);
    for window in cx.windows() {
        let _ = window.update(cx, |_, window, _| window.activate_window());
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    // Finder starts apps with PATH=/usr/bin:/bin:..., so Homebrew's ffmpeg would be missing.
    // shortcut: fixed Homebrew dirs only; read the login shell's PATH if users install ffmpeg elsewhere.
    let path = std::env::var("PATH").unwrap_or_default();
    // SAFETY: first statement of main, before any other thread exists.
    unsafe { std::env::set_var("PATH", format!("{path}:/opt/homebrew/bin:/usr/local/bin")) };
    env_logger::init();
    let config = Config::load();
    lumireel::i18n::init(config.locale.as_deref());
    let token = random_token();
    let (server, jobs) = match server::start(config.library_dir.clone(), token) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("lumireel: could not start local server: {error:#}");
            std::process::exit(1);
        }
    };
    let port = server.port;
    log::info!("lumireel server on 127.0.0.1:{port}");

    let app = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);
    app.on_reopen(reopen_windows);
    app.run(move |cx| {
        gpui_kit::init(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        // Footage reads best on a dark surround, as in every editing suite.
        Theme::change(ThemeMode::Dark, None, cx);
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1440.), px(900.)), cx)),
                window_min_size: Some(size(px(960.), px(600.))),
                ..TitleBar::window_options()
            },
            cx,
            move |window, cx| {
                hide_on_close(window, cx);
                let config = config.clone();
                let server = server.clone();
                let jobs = jobs.clone();
                cx.new(|cx| LumiReel::new(window, cx, config, server, jobs))
            },
        )
        .expect("Failed to open window");
    });
}
