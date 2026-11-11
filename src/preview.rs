use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::SystemTime,
};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_webview::WebView;
use rust_i18n::t;
use serde_json::Value;

use crate::{
    assets::{self, Asset, AssetKind},
    export::{ExportForm, ExportSettings},
    library::Project,
    prompt::{Reference, ShotInfo, format_refs},
    server::{RenderDone, RenderOptions, ServerHandle},
    watch::{self, ProjectWatcher},
};

enum Export {
    Idle,
    /// `progress` is frames written / expected, once the render has planned them.
    Running {
        fps: f64,
        progress: Option<(usize, usize)>,
    },
    Done(RenderDone),
    Failed(String),
}

pub enum PreviewEvent {
    SendToAgent(String),
    /// The film (re)loaded after a change: its cover may be out of date.
    Ready {
        /// The film's length, for the project list.
        duration: f64,
    },
}

impl EventEmitter<PreviewEvent> for PreviewPanel {}

#[derive(Clone, Debug)]
struct RegionRef {
    t: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    shot: Option<ShotInfo>,
}

pub struct PreviewPanel {
    webview: Entity<WebView>,
    /// The stage's second webview: a bare player for a sound or clip picked from the shelf.
    player: Entity<WebView>,
    /// The asset the player is showing, if any.
    media: Option<Asset>,
    loaded: bool,
    /// The project on stage: what an export renders.
    project: Option<String>,
    project_dir: Option<PathBuf>,
    /// The export dialog opens with what was used last.
    last_export: ExportSettings,
    export: Export,
    _export_task: Option<Task<()>>,
    server: ServerHandle,
    window_handle: AnyWindowHandle,

    duration: f32,
    current_t: f32,
    playing: bool,
    error: Option<String>,
    shots: Vec<ShotInfo>,
    width: u32,
    height: u32,

    /// The project's media shelf, shown under the stage.
    assets: Vec<Asset>,
    /// `None` means every kind: the shelf's filter.
    asset_filter: Option<AssetKind>,
    selected_asset: Option<PathBuf>,
    /// Image thumbnails, with the file's mtime they were read at.
    asset_thumbs: HashMap<PathBuf, (SystemTime, Arc<Image>)>,

    progress: Entity<SliderState>,
    range: Entity<SliderState>,
    range_mode: bool,
    /// The range the slider showed at its last change: tells which thumb moved, so the
    /// preview follows that edge while the user drags it.
    range_seen: (f32, f32),
    region: Option<RegionRef>,
    syncing_slider: bool,
    /// The user is dragging the progress bar: the page's time reports lag the thumb, so ignore them.
    scrubbing: bool,
    /// Numbers our seeks; the page tags each time report with the seek it follows, and reports
    /// older than the latest seek are dropped, so the thumb does not bounce back after a drag.
    seek_seq: u64,

    watcher: Option<ProjectWatcher>,
    watch_task: Option<Task<()>>,
}

impl PreviewPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, server: ServerHandle) -> Self {
        let progress = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(1.0)
                .step(0.0001)
                .default_value(0.0)
        });
        let range = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(1.0)
                .step(0.0001)
                .default_value((0.0, 1.0))
        });

        cx.subscribe(&progress, |this, _, event: &SliderEvent, cx| {
            if this.syncing_slider {
                return;
            }
            match event {
                SliderEvent::Change(SliderValue::Single(value)) => {
                    this.scrubbing = true;
                    let t = *value * this.duration;
                    this.seek_to(t, cx);
                }
                SliderEvent::Release(_) => this.scrubbing = false,
                _ => {}
            }
        })
        .detach();

        cx.subscribe(&range, |this, _, event: &SliderEvent, cx| {
            match event {
                SliderEvent::Change(SliderValue::Range(start, end)) => {
                    let (start, end) = (*start, *end);
                    let (seen_start, seen_end) = this.range_seen;
                    this.range_seen = (start, end);
                    this.range_mode = true;
                    let moved_start = (start - seen_start).abs();
                    let moved_end = (end - seen_end).abs();
                    // Preview the thumb that moved. A click on the track moves one thumb
                    // too, and the bigger jump still picks it out.
                    let edge = if moved_start > moved_end {
                        start
                    } else if moved_end > 0.0 {
                        end
                    } else {
                        cx.notify();
                        return;
                    };
                    this.pause(cx);
                    this.seek_to(edge * this.duration, cx);
                    cx.notify();
                }
                SliderEvent::Release(SliderValue::Range(start, end)) => {
                    this.range_seen = (*start, *end);
                }
                _ => {}
            }
        })
        .detach();

        let (ipc_tx, ipc_rx) = flume::unbounded::<String>();
        let builder = wry::WebViewBuilder::new()
            .with_url("about:blank")
            .with_ipc_handler(move |request| {
                let _ = ipc_tx.send(request.body().to_string());
            });
        let webview =
            cx.new(|cx| WebView::build(builder, window, cx).expect("creating preview webview"));
        let player = cx.new(|cx| {
            let builder = wry::WebViewBuilder::new().with_url("about:blank");
            WebView::build(builder, window, cx).expect("creating media player")
        });
        player.update(cx, |player, _| player.hide());

        cx.spawn(async move |this, cx| {
            while let Ok(message) = ipc_rx.recv_async().await {
                if this
                    .update(cx, |this, cx| this.handle_ipc(&message, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        Self {
            webview,
            player,
            media: None,
            loaded: false,
            project: None,
            project_dir: None,
            last_export: ExportSettings::default(),
            export: Export::Idle,
            _export_task: None,
            server,
            window_handle: window.window_handle(),
            duration: 0.0,
            current_t: 0.0,
            playing: false,
            error: None,
            shots: Vec::new(),
            width: 1920,
            height: 1080,
            assets: Vec::new(),
            asset_filter: None,
            selected_asset: None,
            asset_thumbs: HashMap::new(),
            progress,
            range,
            range_mode: false,
            range_seen: (0.0, 1.0),
            region: None,
            syncing_slider: false,
            scrubbing: false,
            seek_seq: 0,
            watcher: None,
            watch_task: None,
        }
    }

    pub fn load(&mut self, project: &Project, cx: &mut Context<Self>) {
        self.stop_media(cx);
        let _ = crate::prompt::ensure_agents_md(&project.path);
        let url = self.server.page_url(&project.name);
        self.loaded = true;
        self.project = Some(project.name.clone());
        self.project_dir = Some(project.path.clone());
        // A finished export's banner belongs to the project it came from.
        if !matches!(self.export, Export::Running { .. }) {
            self.export = Export::Idle;
        }
        self.duration = 0.0;
        self.current_t = 0.0;
        self.playing = false;
        self.error = None;
        self.shots.clear();
        self.region = None;
        self.range_mode = false;
        self.scrubbing = false;
        self.seek_seq = 0;
        self.assets.clear();
        self.asset_thumbs.clear();
        self.asset_filter = None;
        self.selected_asset = None;
        self.webview.update(cx, |webview, _| webview.load_url(&url));
        self.sync_sliders(cx);
        self.install_watch(project.path.clone(), cx);
        self.refresh_assets(cx);
        cx.notify();
    }

    /// Nothing selected any more (its project went to the Trash): blank the stage and stop watching.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.stop_media(cx);
        self.loaded = false;
        self.project = None;
        self.project_dir = None;
        self.playing = false;
        self.duration = 0.0;
        self.current_t = 0.0;
        self.error = None;
        self.shots.clear();
        self.region = None;
        self.range_mode = false;
        self.assets.clear();
        self.asset_thumbs.clear();
        self.asset_filter = None;
        self.selected_asset = None;
        self.watch_task = None;
        self.watcher = None;
        self.webview
            .update(cx, |webview, _| webview.load_url("about:blank"));
        self.sync_sliders(cx);
        cx.notify();
    }

    /// Render the project on stage to MP4 off the UI thread, polling the server for progress.
    fn start_export(&mut self, options: RenderOptions, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        if matches!(self.export, Export::Running { .. }) {
            return;
        }
        self.pause(cx);
        self.export = Export::Running {
            fps: options.fps,
            progress: None,
        };
        let server = self.server.clone();
        let (done_tx, done_rx) = flume::bounded(1);
        let render_server = server.clone();
        // Blocking HTTP for the length of the render: its own thread, not an executor worker.
        std::thread::spawn(move || {
            let _ = done_tx.send(render_server.render(&project, &options));
        });
        self._export_task = Some(cx.spawn(async move |this, cx| {
            let result = loop {
                if let Ok(result) = done_rx.try_recv() {
                    break result;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                let progress = server.render_progress();
                let alive = this.update(cx, |this, cx| {
                    if let Export::Running { progress: p, .. } = &mut this.export
                        && progress.is_some()
                    {
                        *p = progress;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.export = match result {
                    Ok(done) => Export::Done(done),
                    Err(error) => Export::Failed(format!("{error:#}")),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn export_button(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Export::Running { fps, progress } = &self.export {
            let label = match progress {
                Some((written, expected)) if *expected > 0 => t!(
                    "preview.exporting_percent",
                    percent = written * 100 / expected
                )
                .to_string(),
                _ => t!("preview.exporting_fps", fps = fps).to_string(),
            };
            return Button::new("export")
                .xsmall()
                .loading(true)
                .label(label)
                .disabled(true)
                .into_any_element();
        }
        Button::new("export")
            .xsmall()
            .primary()
            .icon(gpui_kit::assets::IconName::Download)
            .label(t!("common.export").to_string())
            .disabled(!self.loaded)
            .on_click(cx.listener(|this, _, window, cx| this.open_export_dialog(window, cx)))
            .into_any_element()
    }

    fn open_export_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(project), Some(project_dir)) = (self.project.clone(), self.project_dir.clone())
        else {
            return;
        };
        self.pause(cx);
        self.hide_while_dialog_open(window, cx);
        let film = (self.width, self.height, self.duration);
        let last = self.last_export.clone();
        let form = cx.new(|cx| ExportForm::new(window, cx, &project, project_dir, film, &last));
        let preview = cx.entity().downgrade();
        let first_focus = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            // Focused once the dialog is up; opening it moves focus to the dialog itself.
            if first_focus.replace(false) {
                let focus = form.read(cx).focus_handle(cx);
                focus.focus(window, cx);
            }
            let form = form.clone();
            let preview = preview.clone();
            dialog
                .title(t!("preview.export_dialog_title").to_string())
                .w(px(520.))
                .child(form.clone())
                .footer(crate::ui::dialog_footer(
                    Button::new("export-confirm")
                        .primary()
                        .label(t!("common.export").to_string()),
                ))
                .on_ok(move |_, _, cx| {
                    let Some((options, settings)) = form.update(cx, |form, cx| form.submit(cx))
                    else {
                        return false;
                    };
                    if let Some(preview) = preview.upgrade() {
                        preview.update(cx, |preview, cx| {
                            preview.last_export = settings;
                            preview.start_export(options, cx);
                        });
                    }
                    true
                })
        });
    }

    /// The stage is a native view drawn above GPUI, so it would cover a dialog: hide it until the
    /// dialog is gone, however it closes.
    pub fn hide_while_dialog_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hide_webview(cx);
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let open = window_handle
                    .update(cx, |_, window, cx| window.has_active_dialog(cx))
                    .unwrap_or(false);
                if !open {
                    let _ = this.update(cx, |this, cx| this.show_webview(cx));
                    break;
                }
            }
        })
        .detach();
    }

    /// What the last export left behind: the file, its problems, or why it failed.
    fn export_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (color, text, file) = match &self.export {
            Export::Done(done) => {
                let name = done
                    .file
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                match done.problems.first() {
                    None => (
                        cx.theme().success,
                        t!("preview.exported", name = name).to_string(),
                        Some(done.file.clone()),
                    ),
                    Some(first) => (
                        cx.theme().warning,
                        t!(
                            "preview.exported_problems",
                            name = name,
                            count = done.problems.len(),
                            first = first.message.clone()
                        )
                        .to_string(),
                        Some(done.file.clone()),
                    ),
                }
            }
            Export::Failed(error) => (
                cx.theme().danger,
                t!("preview.export_failed", error = error.clone()).to_string(),
                None,
            ),
            _ => return None,
        };
        Some(
            h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .bg(color.opacity(0.12))
                .text_xs()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(color)
                        .child(text),
                )
                .when_some(file, |this, file| {
                    let reveal = file.clone();
                    this.child(
                        Button::new("export-reveal")
                            .xsmall()
                            .ghost()
                            .label(t!("common.reveal").to_string())
                            .on_click(move |_, _, _| crate::library::reveal_in_finder(&reveal)),
                    )
                    .child(
                        Button::new("export-open")
                            .xsmall()
                            .ghost()
                            .label(t!("common.open").to_string())
                            .on_click(move |_, _, cx| cx.open_with_system(&file)),
                    )
                })
                .child(
                    Button::new("export-dismiss")
                        .xsmall()
                        .ghost()
                        .icon(IconName::Close)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.export = Export::Idle;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn install_watch(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.watch_task = None;
        self.watcher = None;
        match watch::watch(&path) {
            Ok(watcher) => {
                let events = watcher.events.clone();
                self.watcher = Some(watcher);
                self.watch_task = Some(cx.spawn(async move |this, cx| {
                    while events.recv_async().await.is_ok() {
                        if this
                            .update(cx, |this, cx| {
                                if this.loaded {
                                    this.reload(cx);
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            Err(error) => {
                self.error = Some(format!("could not watch this folder: {error:#}"));
            }
        }
    }

    fn handle_ipc(&mut self, message: &str, cx: &mut Context<Self>) {
        let Ok(value) = serde_json::from_str::<Value>(message) else {
            return;
        };
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "ready" => {
                self.duration = value.get("duration").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                self.width = value.get("width").and_then(Value::as_u64).unwrap_or(1920) as u32;
                self.height = value.get("height").and_then(Value::as_u64).unwrap_or(1080) as u32;
                self.shots = value
                    .get("shots")
                    .and_then(Value::as_array)
                    .map(|shots| {
                        shots
                            .iter()
                            .filter_map(|shot| {
                                Some(ShotInfo {
                                    id: shot.get("id")?.as_str()?.to_string(),
                                    src: shot
                                        .get("src")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    at: shot.get("at")?.as_f64()?,
                                    duration: shot.get("duration")?.as_f64()?,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.error = None;
                self.playing = false;
                self.sync_sliders(cx);
                log::info!(
                    "preview ready: {:.2}s {}x{}, {} shots",
                    self.duration,
                    self.width,
                    self.height,
                    self.shots.len()
                );
                cx.emit(PreviewEvent::Ready {
                    duration: self.duration as f64,
                });
                cx.notify();
            }
            "time" if !self.scrubbing && self.current(&value) => {
                if let Some(t) = value.get("t").and_then(Value::as_f64) {
                    self.current_t = t as f32;
                    self.sync_sliders(cx);
                    cx.notify();
                }
            }
            "ended" if self.current(&value) => {
                self.playing = false;
                self.current_t = self.duration;
                self.sync_sliders(cx);
                cx.notify();
            }
            "region" => {
                let Some(t) = value.get("t").and_then(Value::as_f64) else {
                    return;
                };
                let Some(box_) = value.get("box") else {
                    return;
                };
                let shot = value.get("shot").and_then(|shot| {
                    Some(ShotInfo {
                        id: shot.get("id")?.as_str()?.to_string(),
                        src: shot.get("src").and_then(Value::as_str).map(str::to_string),
                        at: shot.get("at")?.as_f64()?,
                        duration: shot.get("duration")?.as_f64()?,
                    })
                });
                self.region = Some(RegionRef {
                    t,
                    x: box_.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                    y: box_.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                    w: box_.get("w").and_then(Value::as_f64).unwrap_or(0.0),
                    h: box_.get("h").and_then(Value::as_f64).unwrap_or(0.0),
                    shot,
                });
                // The page pauses itself when a drag starts.
                self.playing = false;
                cx.notify();
            }
            "region-send" => {
                // The page holds AppKit focus; give it back so the terminal can take it.
                self.webview.update(cx, |webview, _| {
                    let _ = webview.raw().focus_parent();
                });
                self.send_selection(cx);
            }
            "region-clear" => {
                self.region = None;
                cx.notify();
            }
            "error" => {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                log::warn!("preview error: {message}");
                self.error = Some(message.to_string());
                cx.notify();
            }
            _ => {}
        }
    }

    /// Whether a report from the page follows our latest seek (and is not left over from before it).
    fn current(&self, value: &Value) -> bool {
        value.get("seq").and_then(Value::as_u64).unwrap_or(0) >= self.seek_seq
    }

    fn sync_sliders(&mut self, cx: &mut Context<Self>) {
        let value = if self.duration > 0.0 {
            (self.current_t / self.duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.syncing_slider = true;
        let progress = self.progress.clone();
        let window_handle = self.window_handle;
        let _ = window_handle.update(cx, |_, window, cx| {
            progress.update(cx, |state, cx| state.set_value(value, window, cx));
        });
        self.syncing_slider = false;
    }

    /// A popup menu may reach over the stage, which is a native view drawn above it: hide the
    /// stage while `menu` is open. Not brought back if the menu opened a dialog — the stage would
    /// cover that too; the dialog's own watcher restores it when it closes.
    pub fn hide_under_menu(
        preview: Entity<Self>,
        window: &mut Window,
        cx: &mut Context<gpui_kit::component::menu::PopupMenu>,
    ) {
        preview.update(cx, |preview, cx| preview.hide_webview(cx));
        let menu = cx.entity();
        cx.subscribe_in(&menu, window, move |_, _, _: &DismissEvent, window, cx| {
            if !window.has_active_dialog(cx) {
                preview.update(cx, |preview, cx| preview.show_webview(cx));
            }
        })
        .detach();
    }

    pub fn hide_webview(&mut self, cx: &mut Context<Self>) {
        self.webview.update(cx, |webview, _| webview.hide());
        self.player.update(cx, |player, _| player.hide());
    }

    pub fn show_webview(&mut self, cx: &mut Context<Self>) {
        if self.media.is_some() {
            self.webview.update(cx, |webview, _| webview.hide());
            self.player.update(cx, |player, _| player.show());
        } else {
            self.player.update(cx, |player, _| player.hide());
            self.webview.update(cx, |webview, _| webview.show());
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let t = self.current_t;
        self.playing = false;
        let script = format!(
            "window.__fd && window.__fd.reload({t:.6}, {})",
            self.seek_seq
        );
        self.webview.update(cx, |webview, _| {
            let _ = webview.evaluate_script(&script);
        });
        self.refresh_assets(cx);
    }

    /// Rescan the shelf under the stage, decoding an image again only when its file changed.
    fn refresh_assets(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.project_dir.clone() else {
            return;
        };
        self.assets = assets::list(&dir);
        let paths: HashSet<PathBuf> = self.assets.iter().map(|asset| asset.path.clone()).collect();
        self.asset_thumbs.retain(|path, _| paths.contains(path));
        if self
            .media
            .as_ref()
            .is_some_and(|media| !paths.contains(&media.path))
        {
            self.stop_media(cx);
        }
        if self
            .selected_asset
            .as_ref()
            .is_some_and(|path| !paths.contains(path))
        {
            self.selected_asset = None;
        }
        for asset in &self.assets {
            if asset.kind != AssetKind::Image {
                continue;
            }
            let Some(format) = image_format(&asset.path) else {
                continue;
            };
            if self
                .asset_thumbs
                .get(&asset.path)
                .is_some_and(|(read_at, _)| *read_at == asset.modified)
            {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&asset.path) {
                self.asset_thumbs.insert(
                    asset.path.clone(),
                    (asset.modified, Arc::new(Image::from_bytes(format, bytes))),
                );
            }
        }
        cx.notify();
    }

    /// Show an audio or video asset on the stage, in the second webview over the film.
    fn play_asset(&mut self, asset: &Asset, cx: &mut Context<Self>) {
        if !matches!(asset.kind, AssetKind::Audio | AssetKind::Video) {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };
        let url = self.server.player_url(&project, &asset.relative);
        self.pause(cx);
        self.media = Some(asset.clone());
        self.player.update(cx, |player, _| {
            player.load_url(&url);
            player.show();
        });
        self.webview.update(cx, |webview, _| webview.hide());
        cx.notify();
    }

    /// Leave the media player and put the film back on the stage.
    fn stop_media(&mut self, cx: &mut Context<Self>) {
        if self.media.take().is_none() {
            return;
        }
        // Hiding the native view would keep the sound going; blank the page to stop it.
        self.player.update(cx, |player, _| {
            player.load_url("about:blank");
            player.hide();
        });
        self.webview.update(cx, |webview, _| webview.show());
        cx.notify();
    }

    fn asset_filter_button(
        &self,
        id: &'static str,
        label: String,
        kind: Option<AssetKind>,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .xsmall()
            .ghost()
            .label(label)
            .selected(self.asset_filter == kind)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.asset_filter = kind;
                cx.notify();
            }))
    }

    fn open_assets_folder(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.project_dir.clone() else {
            return;
        };
        let dir = dir.join("assets");
        // The folder is part of the project's shape; make it for older projects that lack one.
        let _ = std::fs::create_dir_all(&dir);
        cx.open_with_system(&dir);
    }

    /// Paste the file's path into the agent, which can then look at it in the project.
    fn send_asset(&mut self, asset: &Asset, cx: &mut Context<Self>) {
        let mut text = t!(
            "assets.send",
            path = asset.relative.clone(),
            kind = asset.kind.label(),
            size = assets::human_size(asset.size)
        )
        .to_string();
        text.push('\n');
        cx.emit(PreviewEvent::SendToAgent(text));
    }

    fn asset_card(&self, asset: &Asset, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected_asset.as_ref() == Some(&asset.path);
        let playable = matches!(asset.kind, AssetKind::Audio | AssetKind::Video);
        let preview = (asset.kind == AssetKind::Image)
            .then(|| {
                self.asset_thumbs
                    .get(&asset.path)
                    .map(|(_, image)| image.clone())
            })
            .flatten();

        let border = cx.theme().border;
        let mut hovered_border = cx.theme().primary;
        hovered_border.a = 0.55;
        let active_border = cx.theme().primary;
        let active_bg = cx.theme().primary.opacity(0.08);
        let resting_bg = cx.theme().muted.opacity(0.35);
        let muted = cx.theme().muted_foreground;
        let card_preview = cx.entity().downgrade();
        let card_path = asset.path.clone();
        let menu_preview = card_preview.clone();
        let menu_path = card_path.clone();
        let play_asset = asset.clone();

        v_flex()
            .id(("asset", index))
            .w(px(132.))
            .flex_none()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(if selected { active_border } else { border })
            .bg(if selected { active_bg } else { resting_bg })
            .overflow_hidden()
            .cursor_pointer()
            .hover(move |this| this.border_color(hovered_border))
            .child(
                div()
                    .h(px(74.))
                    .w_full()
                    .relative()
                    .overflow_hidden()
                    .bg(gpui_kit::black())
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|this| match &preview {
                        Some(image) => {
                            this.child(img(image.clone()).size_full().object_fit(ObjectFit::Cover))
                        }
                        None if asset.kind == AssetKind::Font => this.child(
                            div()
                                .text_size(px(26.))
                                .font_medium()
                                .text_color(muted.opacity(0.9))
                                .child("Aa"),
                        ),
                        None => this.child(
                            Icon::new(asset_icon(asset.kind))
                                .size(px(24.))
                                .text_color(muted.opacity(0.8)),
                        ),
                    })
                    .child(
                        div()
                            .absolute()
                            .left(px(4.))
                            .bottom(px(4.))
                            .px(px(4.))
                            .py(px(1.))
                            .rounded(px(2.))
                            .bg(gpui_kit::black().opacity(0.55))
                            .text_size(px(9.))
                            .text_color(gpui_kit::white())
                            .child(asset.kind.label()),
                    )
                    .when(playable, |this| {
                        this.child(
                            div()
                                .id(("asset-play", index))
                                .absolute()
                                .right(px(4.))
                                .bottom(px(4.))
                                .size(px(22.))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(gpui_kit::black().opacity(0.6))
                                .hover(|this| this.bg(gpui_kit::black().opacity(0.85)))
                                .cursor_pointer()
                                .child(
                                    Icon::new(gpui_kit::assets::IconName::Play)
                                        .size(px(12.))
                                        .text_color(gpui_kit::white()),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.play_asset(&play_asset, cx);
                                })),
                        )
                    }),
            )
            .child(
                v_flex()
                    .p_2()
                    .gap_0p5()
                    .child(div().text_xs().truncate().child(asset.name.clone()))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(muted)
                            .child(assets::human_size(asset.size)),
                    ),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                this.selected_asset = Some(card_path.clone());
                if event.click_count() == 2 {
                    cx.open_with_system(&card_path);
                }
                cx.notify();
            }))
            .context_menu(move |menu, _, _| {
                let open = menu_path.clone();
                let reveal = menu_path.clone();
                let send = menu_path.clone();
                let send_preview = menu_preview.clone();
                menu.item(
                    PopupMenuItem::new(t!("common.open").to_string())
                        .icon(gpui_kit::assets::IconName::ExternalLink)
                        .on_click(move |_, _, cx| cx.open_with_system(&open)),
                )
                .item(
                    PopupMenuItem::new(t!("common.reveal").to_string())
                        .icon(gpui_kit::assets::IconName::FolderOpen)
                        .on_click(move |_, _, _| crate::library::reveal_in_finder(&reveal)),
                )
                .separator()
                .item(
                    PopupMenuItem::new(t!("assets.send_to_agent").to_string())
                        .icon(gpui_kit::assets::IconName::SendHorizontal)
                        .on_click(move |_, _, cx| {
                            let Some(preview) = send_preview.upgrade() else {
                                return;
                            };
                            let path = send.clone();
                            preview.update(cx, |preview, cx| {
                                if let Some(asset) = preview
                                    .assets
                                    .iter()
                                    .find(|asset| asset.path == path)
                                    .cloned()
                                {
                                    preview.send_asset(&asset, cx);
                                }
                            });
                        }),
                )
            })
            .into_any_element()
    }

    fn assets_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let total = self.assets.len();
        let images = self
            .assets
            .iter()
            .filter(|asset| asset.kind == AssetKind::Image)
            .count();
        let audio = self
            .assets
            .iter()
            .filter(|asset| asset.kind == AssetKind::Audio)
            .count();
        let video = self
            .assets
            .iter()
            .filter(|asset| asset.kind == AssetKind::Video)
            .count();
        let fonts = self
            .assets
            .iter()
            .filter(|asset| asset.kind == AssetKind::Font)
            .count();
        let visible: Vec<_> = self
            .assets
            .iter()
            .enumerate()
            .filter(|(_, asset)| self.asset_filter.is_none_or(|kind| asset.kind == kind))
            .collect();
        let muted = cx.theme().muted_foreground;
        let cards = visible
            .iter()
            .map(|(index, asset)| self.asset_card(asset, *index, cx))
            .collect::<Vec<_>>();
        let reveal = Button::new("assets-reveal")
            .xsmall()
            .ghost()
            .icon(gpui_kit::assets::IconName::FolderOpen)
            .tooltip(t!("assets.reveal_folder").to_string())
            .on_click(cx.listener(|this, _, _, cx| this.open_assets_folder(cx)));
        let open_empty = Button::new("assets-open-empty")
            .xsmall()
            .ghost()
            .icon(IconName::FolderOpen)
            .label(t!("assets.open_folder").to_string())
            .on_click(cx.listener(|this, _, _, cx| this.open_assets_folder(cx)));

        v_flex()
            .flex_1()
            .min_h_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .h(px(34.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(muted)
                            .child(t!("assets.title").to_string()),
                    )
                    .when(total > 0, |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(muted.opacity(0.7))
                                .child(t!("assets.files", count = total).to_string()),
                        )
                    })
                    .child(div().flex_1())
                    .when(total > 0, |this| {
                        this.child(self.asset_filter_button(
                            "asset-filter-all",
                            t!("assets.all", count = total).to_string(),
                            None,
                            cx,
                        ))
                        .child(self.asset_filter_button(
                            "asset-filter-image",
                            t!("assets.image_count", count = images).to_string(),
                            Some(AssetKind::Image),
                            cx,
                        ))
                        .child(self.asset_filter_button(
                            "asset-filter-audio",
                            t!("assets.audio_count", count = audio).to_string(),
                            Some(AssetKind::Audio),
                            cx,
                        ))
                        .child(self.asset_filter_button(
                            "asset-filter-video",
                            t!("assets.video_count", count = video).to_string(),
                            Some(AssetKind::Video),
                            cx,
                        ))
                        .child(self.asset_filter_button(
                            "asset-filter-font",
                            t!("assets.font_count", count = fonts).to_string(),
                            Some(AssetKind::Font),
                            cx,
                        ))
                    })
                    .child(reveal),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .overflow_y_scrollbar()
                    .when(visible.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .items_center()
                                .gap_2()
                                .py_8()
                                .text_color(muted)
                                .child(
                                    Icon::new(gpui_kit::assets::IconName::Images)
                                        .size(px(24.))
                                        .text_color(muted.opacity(0.6)),
                                )
                                .child(div().text_xs().child(if total == 0 {
                                    t!("assets.empty").to_string()
                                } else {
                                    let kind = self
                                        .asset_filter
                                        .map(AssetKind::label)
                                        .unwrap_or_else(|| t!("common.asset").to_string());
                                    t!("assets.none", kind = kind).to_string()
                                }))
                                .when(total == 0, |this| {
                                    this.child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(muted.opacity(0.7))
                                            .child(t!("assets.empty_hint").to_string()),
                                    )
                                    .child(open_empty)
                                }),
                        )
                    })
                    .child(h_flex().flex_wrap().items_start().gap_2().children(cards)),
            )
            .into_any_element()
    }

    fn seek_to(&mut self, t: f32, cx: &mut Context<Self>) {
        let t = t.clamp(0.0, self.duration.max(0.0));
        self.current_t = t;
        self.seek_seq += 1;
        let script = format!("window.__fd && window.__fd.seek({t:.6}, {})", self.seek_seq);
        self.webview.update(cx, |webview, _| {
            let _ = webview.evaluate_script(&script);
        });
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if !self.loaded {
            return;
        }
        self.playing = !self.playing;
        let script = if self.playing {
            "window.__fd && window.__fd.play()"
        } else {
            "window.__fd && window.__fd.pause()"
        };
        self.webview.update(cx, |webview, _| {
            let _ = webview.evaluate_script(script);
        });
        cx.notify();
    }

    fn toggle_range(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.range_mode = !self.range_mode;
        if self.range_mode {
            self.pause(cx);
            if self.duration > 0.0 {
                let start = (self.current_t / self.duration).clamp(0.0, 0.98);
                let span = (2.0 / self.duration).clamp(0.01, 1.0);
                let end = (start + span).min(1.0);
                self.range_seen = (start, end);
                self.range.update(cx, |state, cx| {
                    state.set_value((start, end), window, cx);
                });
            }
        }
        cx.notify();
    }

    fn pause(&mut self, cx: &mut Context<Self>) {
        if self.playing {
            self.playing = false;
            self.webview.update(cx, |webview, _| {
                let _ = webview.evaluate_script("window.__fd && window.__fd.pause()");
            });
            cx.notify();
        }
    }

    fn send_selection(&mut self, cx: &mut Context<Self>) {
        let references = self.references_with_cx(cx);
        if references.is_empty() {
            return;
        }
        let mut text = format_refs(
            "index.html",
            self.width,
            self.height,
            &references,
            &self.shots,
        );
        text.push('\n');
        cx.emit(PreviewEvent::SendToAgent(text));
    }

    /// The play, seek and range row, kept directly under the stage.
    fn transport_controls(
        &self,
        play_icon: IconName,
        can_play: bool,
        range_times: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .px_3()
            .py_2()
            .gap_2()
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        h_flex()
                            .flex_none()
                            .w(px(CONTROL_LEADING))
                            .items_center()
                            .gap_3()
                            .child(
                                Button::new("play")
                                    .primary()
                                    .small()
                                    .icon(play_icon)
                                    .disabled(!can_play)
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx))),
                            )
                            .child(
                                h_flex()
                                    .flex_none()
                                    .px_2()
                                    .py_0p5()
                                    .gap_1()
                                    .rounded(cx.theme().radius)
                                    .bg(cx.theme().muted)
                                    .text_xs()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(format_time(self.current_t))
                                    .child(
                                        div()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!("/ {}", format_time(self.duration))),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Slider::new(&self.progress).w_full()),
                    )
                    .child(
                        Button::new("range")
                            .small()
                            .ghost()
                            .w(px(CONTROL_TRAILING))
                            .icon(gpui_kit::assets::IconName::Clock)
                            .label(t!("preview.range").to_string())
                            .tooltip(t!("preview.range_tooltip").to_string())
                            .tooltip_placement(gpui_kit::component::Placement::Bottom)
                            .selected(self.range_mode)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_range(window, cx)),
                            ),
                    ),
            )
            .when(self.range_mode, |this| {
                this.child(
                    h_flex()
                        .items_center()
                        .gap_3()
                        .child(div().flex_none().w(px(CONTROL_LEADING)).when_some(
                            range_times,
                            |this, times| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(cx.theme().foreground)
                                        .child(times),
                                )
                            },
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Slider::new(&self.range).w_full()),
                        )
                        .child(
                            Button::new("send-range")
                                .small()
                                .primary()
                                .w(px(CONTROL_TRAILING))
                                .icon(gpui_kit::assets::IconName::SendHorizontal)
                                .label(t!("preview.send_to_agent").to_string())
                                .tooltip(t!("preview.send_tooltip").to_string())
                                .tooltip_placement(gpui_kit::component::Placement::Bottom)
                                .on_click(cx.listener(|this, _, _, cx| this.send_selection(cx))),
                        ),
                )
            })
            .into_any_element()
    }

    /// What sits under the stage while a shelf asset plays in it.
    fn media_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(media) = &self.media else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        v_flex()
            .px_3()
            .py_2()
            .gap_2()
            .flex_none()
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Icon::new(asset_icon(media.kind))
                            .size(px(18.))
                            .text_color(cx.theme().primary),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(media.name.clone()))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(muted)
                                    .child(t!("preview.playing_asset").to_string()),
                            ),
                    )
                    .child(
                        Button::new("stop-media")
                            .small()
                            .ghost()
                            .w(px(CONTROL_TRAILING))
                            .icon(gpui_kit::assets::IconName::Close)
                            .label(t!("preview.back_to_video").to_string())
                            .tooltip(t!("preview.back_tooltip").to_string())
                            .tooltip_placement(gpui_kit::component::Placement::Bottom)
                            .on_click(cx.listener(|this, _, _, cx| this.stop_media(cx))),
                    ),
            )
            .into_any_element()
    }
}

impl PreviewPanel {
    fn references_with_cx(&self, cx: &Context<Self>) -> Vec<Reference> {
        let mut references = Vec::new();
        if self.range_mode
            && let SliderValue::Range(start, end) = self.range.read(cx).value()
        {
            let (a, b) = (start * self.duration, end * self.duration);
            if b - a > 0.001 {
                references.push(Reference::Range {
                    start: a as f64,
                    end: b as f64,
                });
            }
        }
        if let Some(region) = &self.region {
            references.push(Reference::Region {
                t: region.t,
                x: region.x,
                y: region.y,
                w: region.w,
                h: region.h,
                shot: region.shot.clone(),
            });
        }
        references
    }
}

/// Widths shared by the two control rows, so their sliders start and end on the same lines.
const CONTROL_LEADING: f32 = 200.;
const CONTROL_TRAILING: f32 = 104.;

fn format_time(t: f32) -> String {
    let t = t.max(0.0);
    let minutes = (t / 60.0).floor() as u32;
    let seconds = t - minutes as f32 * 60.0;
    format!("{minutes:02}:{seconds:05.2}")
}

fn asset_icon(kind: AssetKind) -> gpui_kit::assets::IconName {
    match kind {
        AssetKind::Image => gpui_kit::assets::IconName::FileImage,
        AssetKind::Audio => gpui_kit::assets::IconName::FileMusic,
        AssetKind::Video => gpui_kit::assets::IconName::FileVideoCamera,
        AssetKind::Font => gpui_kit::assets::IconName::Type,
    }
}

/// The format GPUI should decode this picture as. `None` for formats it cannot show.
fn image_format(path: &std::path::Path) -> Option<ImageFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "webp" => Some(ImageFormat::Webp),
        "gif" => Some(ImageFormat::Gif),
        "bmp" => Some(ImageFormat::Bmp),
        "tif" | "tiff" => Some(ImageFormat::Tiff),
        "ico" => Some(ImageFormat::Ico),
        "svg" => Some(ImageFormat::Svg),
        _ => None,
    }
}

impl Render for PreviewPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let play_icon = if self.playing {
            IconName::Pause
        } else {
            IconName::Play
        };
        let can_play = self.loaded;

        let export_button = self.export_button(cx);
        let export_banner = self.export_banner(cx);
        let info = self.loaded.then(|| {
            t!(
                "preview.info",
                width = self.width,
                height = self.height,
                shots = self.shots.len()
            )
            .to_string()
        });
        let range_times = self.range_mode.then(|| {
            let (start, end) = match self.range.read(cx).value() {
                SliderValue::Range(start, end) => (start, end),
                SliderValue::Single(value) => (0.0, value),
            };
            t!(
                "preview.range_times",
                start = format_time(start * self.duration),
                end = format_time(end * self.duration)
            )
            .to_string()
        });

        let stage: AnyElement = if self.media.is_some() {
            self.player.clone().into_any_element()
        } else {
            self.webview.clone().into_any_element()
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                crate::ui::panel_header(t!("preview.title").to_string(), cx).child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .when_some(info, |this, info| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(info),
                            )
                        })
                        .child(export_button),
                ),
            )
            .children(export_banner)
            .child(div().flex_1().min_h_0().bg(gpui_kit::black()).child(stage))
            .child(if self.media.is_some() {
                self.media_bar(cx)
            } else {
                self.transport_controls(play_icon, can_play, range_times, cx)
            })
            .child(self.assets_panel(cx))
            // The stage is a native view, so the hint for an empty film sits under it, not on it.
            .when(
                self.loaded
                    && self.error.is_none()
                    && self.duration == 0.0
                    && self.shots.is_empty(),
                |this| {
                    this.child(
                        div()
                            .px_3()
                            .py_2()
                            .bg(cx.theme().muted)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("preview.empty_hint").to_string()),
                    )
                },
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .bg(cx.theme().danger.opacity(0.12))
                        .text_color(cx.theme().danger)
                        .text_sm()
                        .child(error),
                )
            })
    }
}
