//! The canvas that shows and edits a project's workflow (see `workflow`). Drag the background or
//! scroll to pan, a title bar to move a node, an output dot onto another node to wire them; × cuts
//! a wire. Dropped files become nodes, or replace the content of the node they land on. ⌘/Ctrl +
//! scroll zooms the whole card, text too; Shift/⌘-click or Shift-drag selects.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Sizable as _, StyledExt as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::config::Services;
use crate::workflow::{Job, Node, NodeStatus, Workflow, poster};
use serde_json::{Value, json};

const NODE_W: f32 = 300.;
const HEAD_H: f32 = 34.;
const DOT: f32 = 12.;
/// Media box height when there is no picture to size it; the ports sit at its middle.
const MEDIA_H: f32 = 170.;
const GRID: f32 = 24.;
/// Cards are about this tall; used where the real height is not known (box-select, minimap).
const NODE_H: f32 = 260.;
const ZOOM: (f32, f32) = (0.3, 2.0);
const MINI: (f32, f32) = (180., 120.);
const SECONDS: [u32; 7] = [2, 4, 5, 6, 8, 10, 15];
const VIDEO_SIZES: [&str; 2] = ["1280x720", "720x1280"];
const IMAGE_SIZES: [&str; 3] = ["1024x1024", "1536x1024", "1024x1536"];

/// Lays out and paints `child` with another rem size, so rem-based sizes and text scale with it.
struct Scaled {
    rem: Pixels,
    child: AnyElement,
}

impl IntoElement for Scaled {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Scaled {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let child = &mut self.child;
        (
            window.with_rem_size(Some(self.rem), |w| child.request_layout(w, cx)),
            (),
        )
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let child = &mut self.child;
        window.with_rem_size(Some(self.rem), |w| child.prepaint(w, cx));
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let child = &mut self.child;
        window.with_rem_size(Some(self.rem), |w| child.paint(w, cx));
    }
}

/// The pan offset that keeps the canvas point under `at` in place when zoom goes `z` → `z2`.
fn zoom_about(offset: Point<Pixels>, at: Point<Pixels>, z: f32, z2: f32) -> Point<Pixels> {
    point(
        at.x - (at.x - offset.x) * (z2 / z),
        at.y - (at.y - offset.y) * (z2 / z),
    )
}

/// World-space box around all nodes: (min x, min y, max x, max y).
fn extent(nodes: &[Node]) -> Option<(f32, f32, f32, f32)> {
    nodes.iter().fold(None, |acc, n| {
        let (x, y) = (n.x as f32, n.y as f32);
        let (x2, y2) = (x + NODE_W, y + NODE_H);
        Some(match acc {
            None => (x, y, x2, y2),
            Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x2), d.max(y2)),
        })
    })
}

#[derive(Clone)]
struct Picture {
    /// The file itself for images, the first frame for videos.
    path: Option<PathBuf>,
    /// Height / width, if the header could be read.
    aspect: Option<f32>,
}

enum Drag {
    Pan {
        from: Point<Pixels>,
        start: Point<Pixels>,
    },
    /// The grabbed node and every other selected one, with where each started.
    Nodes {
        from: Point<Pixels>,
        start: Vec<(i64, (f64, f64))>,
    },
    /// A wire being pulled out of `from`'s output; `at` is the cursor (window coordinates).
    Wire { from: i64, at: Point<Pixels> },
    /// Shift-drag on the canvas: a selection box (window coordinates).
    Marquee {
        from: Point<Pixels>,
        at: Point<Pixels>,
    },
    /// Pressing on the minimap: the view follows the cursor.
    Minimap,
}

pub struct CanvasPanel {
    project: Option<PathBuf>,
    workflow: Workflow,
    status: HashMap<i64, NodeStatus>,
    /// Keyed by the output file, relative to the project.
    pictures: HashMap<String, Picture>,
    /// Jobs on their way, in any project: a run outlives switching projects.
    running: HashSet<(PathBuf, i64)>,
    /// Why a node's last run failed, with its input hash: Run all does not retry it until that
    /// changes.
    errors: HashMap<(PathBuf, i64), (String, String)>,
    /// Projects on Run all, whether on screen or not.
    auto: HashSet<PathBuf>,
    services: Services,
    offset: Point<Pixels>,
    zoom: f32,
    selected: HashSet<i64>,
    /// The canvas's bounds in the window, from the last paint.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    drag: Option<Drag>,
    texts: HashMap<i64, Entity<TextareaState>>,
    renaming: Option<(i64, Entity<InputState>)>,
    _subs: Vec<Subscription>,
}

/// The workflow of the project at this path was saved: its thumbnail may need redrawing.
pub struct WorkflowSaved(pub PathBuf);

impl EventEmitter<WorkflowSaved> for CanvasPanel {}

fn kind_icon(kind: &str) -> IconName {
    match kind {
        "text" => IconName::MessageSquare,
        "image" => IconName::Image,
        "video" => IconName::Video,
        "audio" => IconName::AudioLines,
        _ => IconName::Layers,
    }
}

impl CanvasPanel {
    pub fn new(services: Services) -> Self {
        Self {
            project: None,
            workflow: Workflow::default(),
            status: HashMap::new(),
            pictures: HashMap::new(),
            running: HashSet::new(),
            errors: HashMap::new(),
            auto: HashSet::new(),
            services,
            offset: point(px(60.), px(80.)),
            zoom: 1.0,
            selected: HashSet::new(),
            bounds: Rc::new(Cell::new(Bounds::default())),
            drag: None,
            texts: HashMap::new(),
            renaming: None,
            _subs: vec![],
        }
    }

    pub fn set_services(&mut self, services: Services) {
        self.services = services;
    }

    pub fn load(&mut self, path: Option<&Path>, cx: &mut Context<Self>) {
        if self.project.as_deref() == path {
            return;
        }
        self.project = path.map(Path::to_path_buf);
        self.workflow = path.map(Workflow::load).unwrap_or_default();
        self.pictures.clear();
        self.texts.clear();
        self.selected.clear();
        self.renaming = None;
        self.drag = None;
        self.offset = point(px(60.), px(80.));
        self.zoom = 1.0;
        self.reload();
        cx.notify();
    }

    fn busy(&self, id: i64) -> bool {
        self.project
            .as_ref()
            .is_some_and(|p| self.running.contains(&(p.clone(), id)))
    }

    fn reload(&mut self) {
        self.status = self.workflow.status();
        let ids: HashSet<i64> = self.workflow.nodes.iter().map(|n| n.id).collect();
        self.selected.retain(|id| ids.contains(id));
        self.texts.retain(|id, _| ids.contains(id));
        let open = self.project.clone();
        self.errors
            .retain(|(dir, id), _| Some(dir) != open.as_ref() || ids.contains(id));
        let Some(dir) = self.project.clone() else {
            return;
        };
        for n in &self.workflow.nodes {
            let Some(out) = &n.out else { continue };
            if self.pictures.contains_key(&out.file) {
                continue;
            }
            let file = dir.join(&out.file);
            let path = match output_media(&n.kind) {
                "image" => Some(file),
                // shortcut: posters are made on the UI thread the first time; fine for a few
                // dozen clips, move to a background task if workflows get large.
                "video" => poster(&dir, &file),
                _ => None,
            };
            let aspect = path
                .as_deref()
                .and_then(|p| image::image_dimensions(p).ok())
                .filter(|(w, h)| *w > 0 && *h > 0)
                .map(|(w, h)| h as f32 / w as f32);
            self.pictures
                .insert(out.file.clone(), Picture { path, aspect });
        }
    }

    /// Saves and shows the workflow after a change; errors become notifications.
    fn done(&mut self, r: anyhow::Result<()>, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = r {
            window.push_notification(format!("{e:#}"), cx);
        }
        if let Some(dir) = self.project.clone() {
            match self.workflow.save(&dir) {
                Ok(()) => cx.emit(WorkflowSaved(dir)),
                Err(e) => window.push_notification(format!("{e:#}"), cx),
            }
        }
        self.reload();
        cx.notify();
    }

    /// Where a new node goes: the visible top-left area, in a 3 x 3 grid so cards (and the
    /// ports at their sides) do not cover each other.
    fn spot(&self) -> (f64, f64) {
        let n = self.workflow.nodes.len() % 9;
        let z = f64::from(self.zoom);
        (
            f64::from(f32::from(-self.offset.x)) / z
                + 80.
                + (n % 3) as f64 * f64::from(NODE_W + 60.),
            f64::from(f32::from(-self.offset.y)) / z
                + 60.
                + (n / 3) as f64 * f64::from(NODE_H + 40.),
        )
    }

    /// Zoom to `z2`, keeping the canvas point under `at` (canvas coordinates) where it is.
    fn zoom_to(&mut self, z2: f32, at: Point<Pixels>) {
        let z2 = z2.clamp(ZOOM.0, ZOOM.1);
        self.offset = zoom_about(self.offset, at, self.zoom, z2);
        self.zoom = z2;
    }

    fn canvas_center(&self) -> Point<Pixels> {
        let b = self.bounds.get();
        point(b.size.width / 2., b.size.height / 2.)
    }

    /// Everything in view, at most at 100 %.
    fn fit(&mut self) {
        let (Some((x, y, x2, y2)), b) = (extent(&self.workflow.nodes), self.bounds.get()) else {
            return;
        };
        let (bw, bh) = (f32::from(b.size.width), f32::from(b.size.height));
        if bw <= 0. {
            return;
        }
        let z = ((bw - 80.) / (x2 - x))
            .min((bh - 80.) / (y2 - y))
            .clamp(ZOOM.0, 1.0);
        self.zoom = z;
        self.offset = point(
            px(bw / 2. - (x + x2) / 2. * z),
            px(bh / 2. - (y + y2) / 2. * z),
        );
    }

    /// Shift/⌘-click adds or removes; a plain click selects just this one.
    fn click_select(&mut self, id: i64, add: bool) {
        if add {
            if !self.selected.remove(&id) {
                self.selected.insert(id);
            }
        } else if !self.selected.contains(&id) {
            self.selected = HashSet::from([id]);
        }
    }

    /// Nodes whose title bar touches the box between two window points.
    fn select_box(&mut self, a: Point<Pixels>, b: Point<Pixels>) {
        let origin = self.bounds.get().origin;
        let z = self.zoom;
        let world = |p: Point<Pixels>| {
            let l = p - origin - self.offset;
            (f32::from(l.x) / z, f32::from(l.y) / z)
        };
        let ((ax, ay), (bx, by)) = (world(a), world(b));
        let (x1, x2, y1, y2) = (ax.min(bx), ax.max(bx), ay.min(by), ay.max(by));
        self.selected = self
            .workflow
            .nodes
            .iter()
            .filter(|n| {
                let (x, y) = (n.x as f32, n.y as f32);
                x < x2 && x + NODE_W > x1 && y < y2 && y + HEAD_H > y1
            })
            .map(|n| n.id)
            .collect();
    }

    /// The minimap's world box and scale: (min x, min y, scale), covering nodes and the view.
    fn minimap_frame(&self) -> Option<(f32, f32, f32)> {
        let (x, y, x2, y2) = extent(&self.workflow.nodes)?;
        let b = self.bounds.get();
        let z = self.zoom;
        let (vx, vy) = (-f32::from(self.offset.x) / z, -f32::from(self.offset.y) / z);
        let (vx2, vy2) = (
            vx + f32::from(b.size.width) / z,
            vy + f32::from(b.size.height) / z,
        );
        let (x, y, x2, y2) = (x.min(vx), y.min(vy), x2.max(vx2), y2.max(vy2));
        let scale = (MINI.0 / (x2 - x)).min(MINI.1 / (y2 - y));
        Some((x, y, scale))
    }

    /// Centres the view on the world point under a window position on the minimap.
    fn minimap_jump(&mut self, at: Point<Pixels>) {
        let Some((x, y, scale)) = self.minimap_frame() else {
            return;
        };
        let b = self.bounds.get();
        let corner = b.origin
            + point(
                b.size.width - px(MINI.0 + 12.),
                b.size.height - px(MINI.1 + 12.),
            );
        let local = at - corner;
        let (wx, wy) = (
            x + f32::from(local.x) / scale,
            y + f32::from(local.y) / scale,
        );
        self.offset = point(
            b.size.width / 2. - px(wx * self.zoom),
            b.size.height / 2. - px(wy * self.zoom),
        );
    }

    fn confirm_delete(&mut self, ids: Vec<i64>, window: &mut Window, cx: &mut Context<Self>) {
        let title = match ids.as_slice() {
            [] => return,
            [id] => match self.workflow.node(*id) {
                Some(n) => t!("canvas.delete_one", name = n.title.clone()).to_string(),
                None => return,
            },
            _ => t!("canvas.delete_many", count = ids.len()).to_string(),
        };
        let weak = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (weak, ids) = (weak.clone(), ids.clone());
            alert
                .title(title.clone())
                .description(t!("canvas.delete_body").to_string())
                .show_cancel(true)
                .cancel_text(t!("common.cancel").to_string())
                .ok_text(t!("canvas.delete").to_string())
                .on_ok(move |_, window, cx| {
                    if let Some(this) = weak.upgrade() {
                        this.update(cx, |this, cx| {
                            for id in &ids {
                                this.workflow.delete_node(*id);
                            }
                            this.selected.clear();
                            this.done(Ok(()), window, cx)
                        });
                    }
                    true
                })
        });
    }

    fn add(&mut self, kind: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.project.is_none() {
            return;
        }
        let (x, y) = self.spot();
        let r = self.workflow.add_node(kind, x, y).map(|_| ());
        self.done(r, window, cx);
    }

    fn drop_files(
        &mut self,
        node: Option<i64>,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.project.clone() else {
            return;
        };
        let mut r = Ok(());
        for (i, p) in paths.iter().enumerate() {
            let one = match node {
                Some(id) if i == 0 => self.workflow.import_file(&dir, id, p),
                _ => {
                    let (x, y) = self.spot();
                    let at = i as f64 * 30.;
                    self.workflow
                        .add_file_node(&dir, p, x + at, y + at)
                        .map(|_| ())
                }
            };
            r = r.and(one);
        }
        self.done(r, window, cx);
    }

    fn pick_file(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("canvas.import").to_string().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.drop_files(Some(id), &paths[..paths.len().min(1)], window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn export_cut(
        &mut self,
        file: PathBuf,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let home = std::env::home_dir().unwrap_or_default();
        let picked = cx.prompt_for_new_path(&home, Some(&format!("{title}.mp4")));
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                return;
            };
            let target = path.clone();
            let r = cx
                .background_spawn(async move { std::fs::copy(&file, &target) })
                .await;
            cx.update(|window, cx| {
                let name = path.display().to_string();
                window.push_notification(
                    match r {
                        Ok(_) => t!("preview.exported", name = name).to_string(),
                        Err(e) => t!("preview.export_failed", error = e.to_string()).to_string(),
                    },
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    /// Turns a node's title into an input; Enter or leaving it saves, an empty name is refused.
    fn start_rename(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(n) = self.workflow.node(id) else {
            return;
        };
        let state = cx.new(|cx| InputState::new(window, cx).default_value(n.title.clone()));
        state.update(cx, |s, cx| s.focus(window, cx));
        self._subs.push(cx.subscribe_in(
            &state,
            window,
            move |this, state, e: &InputEvent, w, cx| {
                if !matches!(e, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    return;
                }
                if this.renaming.as_ref().is_none_or(|(rid, _)| *rid != id) {
                    return;
                }
                this.renaming = None;
                let title = state.read(cx).value().trim().to_string();
                match this.workflow.node_mut(id) {
                    Some(n) if !title.is_empty() && n.title != title => n.title = title,
                    _ => return cx.notify(),
                }
                this.done(Ok(()), w, cx);
            },
        ));
        self.renaming = Some((id, state));
        cx.notify();
    }

    fn params_row(&self, n: &Node, this: WeakEntity<Self>) -> Div {
        let id = n.id;
        let auto = t!("canvas.auto").to_string();
        let mut row = h_flex().gap_2().items_center();
        if n.kind == "video" {
            let current = n.seconds;
            let this = this.clone();
            let auto = auto.clone();
            row = row.child(
                Button::new(ElementId::Name(format!("secs-{id}").into()))
                    .xsmall()
                    .outline()
                    .label(match current {
                        Some(s) => format!("{} ▾", t!("canvas.seconds", n = s)),
                        None => format!("{} ▾", t!("canvas.length_auto")),
                    })
                    .dropdown_menu(move |mut menu, _, _| {
                        for s in std::iter::once(None).chain(SECONDS.map(Some)) {
                            let this = this.clone();
                            let label = match s {
                                Some(s) => t!("canvas.seconds", n = s).to_string(),
                                None => auto.clone(),
                            };
                            menu = menu.item(
                                PopupMenuItem::new(label).checked(s == current).on_click(
                                    move |_, w, cx| {
                                        let _ = this.update(cx, |p, cx| {
                                            p.edit(id, |n| n.seconds = s, w, cx)
                                        });
                                    },
                                ),
                            );
                        }
                        menu
                    }),
            );
        }
        let current = n.size.clone();
        let sizes: &'static [&'static str] = if n.kind == "video" {
            &VIDEO_SIZES
        } else {
            &IMAGE_SIZES
        };
        row.child(
            Button::new(ElementId::Name(format!("size-{id}").into()))
                .xsmall()
                .outline()
                .label(format!(
                    "{} ▾",
                    current
                        .clone()
                        .unwrap_or(t!("canvas.size_auto").to_string())
                ))
                .dropdown_menu(move |mut menu, _, _| {
                    for s in std::iter::once(None).chain(sizes.iter().map(|s| Some(s.to_string())))
                    {
                        let this = this.clone();
                        let label = s.clone().unwrap_or(auto.clone());
                        menu = menu.item(PopupMenuItem::new(label).checked(s == current).on_click(
                            move |_, w, cx| {
                                let s = s.clone();
                                let _ = this.update(cx, |p, cx| p.edit(id, |n| n.size = s, w, cx));
                            },
                        ));
                    }
                    menu
                }),
        )
    }

    fn edit(
        &mut self,
        id: i64,
        f: impl FnOnce(&mut Node),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(n) = self.workflow.node_mut(id) {
            f(n);
            self.done(Ok(()), window, cx);
        }
    }

    fn save_prompt(&mut self, id: i64, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(small) = self.texts.get(&id).cloned() {
            small.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
        }
        self.edit(id, |n| n.text = text, window, cx);
    }

    /// The prompt in a big window: Save writes it back, closing discards it. Image / video nodes
    /// get their settings and Generate here too.
    fn open_editor(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(n) = self.workflow.node(id) else {
            return;
        };
        let current = self
            .texts
            .get(&id)
            .map_or(n.text.clone(), |t| t.read(cx).value().to_string());
        let title = n.title.clone();
        let generates = ["image", "video"].contains(&n.kind.as_str());
        let big = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(18)
                .placeholder(t!("canvas.prompt_hint").to_string())
                .default_value(current)
        });
        let big_focus = big.clone();
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let (big2, big3, weak2) = (big.clone(), big.clone(), weak.clone());
            // Built on every frame: settings and the button follow the node as it changes.
            let page = weak.upgrade();
            let page = page.as_ref().map(|p| p.read(cx));
            let node = page.and_then(|p| p.workflow.node(id));
            let params = page
                .zip(node)
                .filter(|(_, n)| n.kind != "text")
                .map(|(p, n)| p.params_row(n, weak.clone()));
            let status = page.and_then(|p| p.status.get(&id).copied());
            let busy = page.is_some_and(|p| p.busy(id));
            let run = generates.then(|| {
                let weak = weak.clone();
                Button::new(ElementId::Name(format!("gen-{id}").into()))
                    .primary()
                    .icon(IconName::Sparkles)
                    .label(match (busy, status) {
                        (true, _) => t!("canvas.generating").to_string(),
                        (_, Some(NodeStatus::Stale | NodeStatus::Done)) => {
                            t!("canvas.regenerate").to_string()
                        }
                        _ => t!("canvas.generate").to_string(),
                    })
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        let text = big3.read(cx).value().to_string();
                        if let Some(this) = weak.upgrade() {
                            this.update(cx, |this, cx| {
                                this.save_prompt(id, text, window, cx);
                                this.generate(id, window, cx);
                            });
                        }
                        window.close_dialog(cx);
                    })
            });
            let theme = cx.theme();
            dialog
                .title(t!("canvas.edit_title", name = title.clone()).to_string())
                .w(px(760.))
                .child(
                    div()
                        .w_full()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.input)
                        .child(Textarea::new(&big).bordered(false)),
                )
                .children(params.map(|p| p.mt_3()))
                .footer(
                    DialogFooter::new()
                        // A plain button, not DialogAction: that one stretches to fill the row.
                        .child(
                            Button::new("save-prompt")
                                .when(!generates, |b| b.primary())
                                .when(generates, |b| b.outline())
                                .label(t!("common.save").to_string())
                                .on_click(move |_, window, cx| {
                                    let text = big2.read(cx).value().to_string();
                                    if let Some(this) = weak2.upgrade() {
                                        this.update(cx, |this, cx| {
                                            this.save_prompt(id, text, window, cx)
                                        });
                                    }
                                    window.close_dialog(cx);
                                }),
                        )
                        .children(run),
                )
        });
        // Into the big box once the dialog is up (opening it takes the focus).
        window.defer(cx, move |window, cx| {
            big_focus.update(cx, |s, cx| s.focus(window, cx))
        });
    }

    fn wire(&mut self, from: i64, to: i64, window: &mut Window, cx: &mut Context<Self>) {
        let r = self.workflow.connect(from, to);
        self.done(r, window, cx);
    }

    /// Saves a prompt still being typed (it saves on blur), so a run uses what is on screen.
    fn flush_text(&mut self, id: i64, cx: &mut Context<Self>) {
        if let Some(state) = self.texts.get(&id) {
            let text = state.read(cx).value().to_string();
            if let Some(n) = self.workflow.node_mut(id) {
                n.text = text;
            }
        }
    }

    fn generate(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_text(id, cx);
        self.done(Ok(()), window, cx);
        match self.status.get(&id) {
            Some(NodeStatus::Ready | NodeStatus::Stale) => {}
            Some(NodeStatus::Done) => return,
            _ => {
                let merge = self.workflow.node(id).is_some_and(|n| n.kind == "merge");
                let why = if merge {
                    t!("canvas.merge_waiting")
                } else {
                    t!("canvas.needs_input")
                };
                window.push_notification(why.to_string(), cx);
                return;
            }
        }
        if let (Some(dir), Some(job)) = (self.project.clone(), self.workflow.job(id)) {
            self.start(dir, job, window, cx);
        }
        cx.notify();
    }

    /// A `reel workflow` request for the project at `dir`. The open project is edited on screen, so
    /// the person sees each change and nothing they do overwrites it.
    pub fn serve(
        &mut self,
        dir: &Path,
        op: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Value> {
        if !crate::library::is_generated(dir) {
            anyhow::bail!(
                "this project is a coded video, not one generated by video models: make it in index.html"
            );
        }
        let here = self.project.as_deref() == Some(dir);
        if here {
            // Keep what the person is typing: the agent edits the workflow as it is on screen.
            let ids: Vec<i64> = self.workflow.nodes.iter().map(|n| n.id).collect();
            for id in ids {
                self.flush_text(id, cx);
            }
        }
        match op["op"].as_str().unwrap_or("show") {
            "show" => Ok(self.describe(dir)),
            "run" => self.serve_run(dir, op, window, cx),
            _ => {
                let mut workflow = self.workflow_of(dir);
                let answer = workflow.apply(dir, op)?;
                if here {
                    self.workflow = workflow;
                    self.done(Ok(()), window, cx);
                } else {
                    workflow.save(dir)?;
                    cx.emit(WorkflowSaved(dir.to_path_buf()));
                }
                Ok(answer)
            }
        }
    }

    fn describe(&self, dir: &Path) -> Value {
        let mut shown = self.workflow_of(dir).describe();
        for n in shown["nodes"].as_array_mut().into_iter().flatten() {
            let id = n["id"].as_i64().unwrap_or_default();
            let key = (dir.to_path_buf(), id);
            if self.running.contains(&key) {
                n["status"] = json!("generating");
            } else if let Some((_, error)) = self.errors.get(&key)
                && matches!(n["status"].as_str(), Some("ready" | "stale"))
            {
                n["status"] = json!("failed");
                n["error"] = json!(error);
            }
        }
        shown["run_all"] = json!(self.auto.contains(dir));
        shown
    }

    /// The listed nodes, or with none everything ready and then what follows. Answers at once.
    fn serve_run(
        &mut self,
        dir: &Path,
        op: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Value> {
        let workflow = self.workflow_of(dir);
        let ids: Vec<i64> = op["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect();
        let mut started = Vec::new();
        let mut skipped = serde_json::Map::new();
        if ids.is_empty() {
            for job in self.ready(dir, &workflow) {
                let id = job.node;
                if self.start(dir.to_path_buf(), job, window, cx) {
                    started.push(id);
                }
            }
            if !started.is_empty() {
                self.auto.insert(dir.to_path_buf());
            }
        } else {
            let status = workflow.status();
            for id in ids {
                let why = match (workflow.job(id), status.get(&id)) {
                    (_, None) => Some("no such node"),
                    _ if self.running.contains(&(dir.to_path_buf(), id)) => {
                        Some("already generating")
                    }
                    (Some(job), _) => {
                        self.start(dir.to_path_buf(), job, window, cx);
                        started.push(id);
                        None
                    }
                    (None, Some(NodeStatus::Done)) => Some("up to date"),
                    (None, Some(NodeStatus::Idle)) => Some("nothing to generate"),
                    (None, _) => Some(
                        "needs its inputs first: a prompt, or the nodes wired into it generated",
                    ),
                };
                if let Some(why) = why {
                    skipped.insert(id.to_string(), json!(why));
                }
            }
        }
        cx.notify();
        Ok(json!({ "started": started, "skipped": skipped }))
    }

    /// The one on screen, or the saved one.
    fn workflow_of(&self, dir: &Path) -> Workflow {
        match &self.project {
            Some(open) if open == dir => self.workflow.clone(),
            _ => Workflow::load(dir),
        }
    }

    /// False if it is already running.
    fn start(
        &mut self,
        dir: PathBuf,
        job: Job,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.running.insert((dir.clone(), job.node)) {
            return false;
        }
        self.errors.remove(&(dir.clone(), job.node));
        let (tx, rx) = flume::bounded(1);
        let (services, run, at) = (self.services.clone(), job.clone(), dir.clone());
        // Blocks for minutes on a video service: its own thread, not the executor's pool.
        std::thread::spawn(move || {
            let _ = tx.send(run.run(&services, &at));
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx
                .recv_async()
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the generation stopped")));
            this.update_in(cx, |this, window, cx| {
                this.finished(dir, job, result, window, cx)
            })
            .ok();
        })
        .detach();
        true
    }

    fn finished(
        &mut self,
        dir: PathBuf,
        job: Job,
        result: anyhow::Result<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.running.remove(&(dir.clone(), job.node));
        let here = self.project.as_ref() == Some(&dir);
        match result {
            Ok(file) if here => {
                self.workflow.finish(&job, file);
                self.done(Ok(()), window, cx);
            }
            // The user moved on to another project: put the result in that one's workflow.
            Ok(file) => {
                let mut workflow = Workflow::load(&dir);
                workflow.finish(&job, file);
                match workflow.save(&dir) {
                    Ok(()) => cx.emit(WorkflowSaved(dir.clone())),
                    Err(e) => window.push_notification(format!("{e:#}"), cx),
                }
            }
            Err(e) => {
                let title = self
                    .workflow_of(&dir)
                    .node(job.node)
                    .map(|n| n.title.clone())
                    .unwrap_or_default();
                let message = format!("{e:#}");
                window.push_notification(
                    t!("canvas.failed_named", name = title, error = message.clone()).to_string(),
                    cx,
                );
                self.errors
                    .insert((dir.clone(), job.node), (job.hash.clone(), message));
            }
        }
        if self.auto.contains(&dir) {
            self.run_ready(dir, window, cx);
        }
        cx.notify();
    }

    /// Jobs that can run now, skipping those running or just failed with the same inputs.
    fn ready(&self, dir: &Path, workflow: &Workflow) -> Vec<Job> {
        let status = workflow.status();
        workflow
            .nodes
            .iter()
            .filter(|n| {
                matches!(
                    status.get(&n.id),
                    Some(NodeStatus::Ready | NodeStatus::Stale)
                )
            })
            .filter(|n| !self.running.contains(&(dir.to_path_buf(), n.id)))
            .filter_map(|n| workflow.job(n.id))
            .filter(|job| {
                let failed = self.errors.get(&(dir.to_path_buf(), job.node));
                failed.is_none_or(|(hash, _)| *hash != job.hash)
            })
            .collect()
    }

    fn running_in(&self, dir: &Path) -> usize {
        self.running.iter().filter(|(d, _)| d == dir).count()
    }

    /// Run all: start everything ready; the rest follows as upstream finishes.
    fn run_ready(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let workflow = self.workflow_of(&dir);
        for job in self.ready(&dir, &workflow) {
            self.start(dir.clone(), job, window, cx);
        }
        if self.running_in(&dir) == 0 {
            self.auto.remove(&dir);
        }
        cx.notify();
    }

    /// The prompt box of a node; saved when it loses focus.
    fn text_state(
        &mut self,
        n: &Node,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        if let Some(t) = self.texts.get(&n.id).cloned() {
            // Changed elsewhere (the big editor): show the saved text again, unless it is being
            // typed in right now.
            let editing = t.read(cx).focus_handle(cx).is_focused(window);
            if !editing && t.read(cx).value() != n.text {
                t.update(cx, |s, cx| s.set_value(n.text.clone(), window, cx));
            }
            return t;
        }
        let state = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(5)
                .placeholder(t!("canvas.prompt_hint").to_string())
                .default_value(n.text.clone())
        });
        let id = n.id;
        self._subs.push(cx.subscribe_in(
            &state,
            window,
            move |this, state, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::Blur) {
                    let text = state.read(cx).value().to_string();
                    if this.workflow.node(id).is_some_and(|n| n.text != text) {
                        this.edit(id, |n| n.text = text, window, cx);
                    }
                }
            },
        ));
        self.texts.insert(n.id, state.clone());
        state
    }

    fn picture(&self, n: &Node) -> Option<&Picture> {
        self.pictures.get(&n.out.as_ref()?.file)
    }

    /// The whole picture at the card's width, else the fixed preview height.
    fn media_h(&self, n: &Node) -> f32 {
        // The panel's 1px border on each side.
        self.picture(n)
            .and_then(|p| p.aspect)
            .map_or(MEDIA_H, |a| ((NODE_W - 2.) * a).clamp(60., NODE_W * 2.))
    }

    fn port_at(&self, n: &Node, output: bool) -> Point<Pixels> {
        let z = self.zoom;
        point(
            self.offset.x + px((n.x as f32 + if output { NODE_W } else { 0. }) * z),
            self.offset.y + px((n.y as f32 + HEAD_H + self.media_h(n) / 2.) * z),
        )
    }

    fn render_node(&mut self, n: Node, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let z = self.zoom;
        let chosen = self.selected.contains(&n.id);
        let muted = theme.muted_foreground;
        let id = n.id;
        let status = self.status.get(&id).copied().unwrap_or(NodeStatus::Idle);
        let busy = self.busy(id);
        let failed = self
            .project
            .clone()
            .and_then(|dir| self.errors.get(&(dir, id)))
            .filter(|_| matches!(status, NodeStatus::Ready | NodeStatus::Stale))
            .map(|(_, e)| e.clone());
        let file = self
            .project
            .as_ref()
            .zip(n.out.as_ref())
            .map(|(dir, o)| dir.join(&o.file));
        let imported = n.out.as_ref().is_some_and(|o| o.imported);
        let picture = self.picture(&n).cloned();
        let picked = matches!(self.drag, Some(Drag::Wire { from, .. }) if from == id);
        let wired_in = self.workflow.edges.iter().any(|e| e.to == id);
        let wiring_from = match self.drag {
            Some(Drag::Wire { from, .. }) => Some(from),
            _ => None,
        };
        let wired_out = self.workflow.edges.iter().any(|e| e.from == id);
        let media_h = self.media_h(&n);
        let dot = |wired: bool, eid: ElementId| {
            div()
                .id(eid)
                .absolute()
                .top(px((HEAD_H + media_h / 2. - DOT / 2.) * z))
                .size(px(DOT * z))
                .rounded_full()
                .border_2()
                .border_color(theme.primary)
                .bg(if wired {
                    theme.primary
                } else {
                    theme.background
                })
                .cursor_crosshair()
        };
        let input_dot = (n.kind != "text" && n.kind != "audio").then(|| {
            dot(wired_in, ElementId::Name(format!("in-{id}").into())).left(px(-DOT / 2. * z))
        });
        let output_dot = dot(
            wired_out || picked,
            ElementId::Name(format!("out-{id}").into()),
        )
        .right(px(-DOT / 2. * z))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.drag = Some(Drag::Wire {
                    from: id,
                    at: e.position,
                });
                cx.notify();
            }),
        );

        let (badge, color) = match (busy, status) {
            (true, _) => (t!("canvas.status_generating").to_string(), theme.info),
            _ if failed.is_some() => (t!("canvas.status_failed").to_string(), theme.danger),
            (_, NodeStatus::Ready) => (t!("canvas.status_ready").to_string(), theme.primary),
            (_, NodeStatus::Stale) => (t!("canvas.status_stale").to_string(), theme.warning),
            (_, NodeStatus::Waiting) => (t!("canvas.status_waiting").to_string(), muted),
            _ => (String::new(), muted),
        };
        // An imported file is used as is. Some(true) = merge, which needs videos wired in.
        let action = match n.kind.as_str() {
            "image" | "video" if !imported || wired_in => Some(false),
            "merge" if wired_in => Some(true),
            _ => None,
        };
        let header =
            h_flex()
                .id(ElementId::Name(format!("node-{id}").into()))
                .h(px(HEAD_H * z))
                .gap_2()
                .items_center()
                .cursor_move()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let add = e.modifiers.shift || e.modifiers.platform;
                        this.click_select(id, add);
                        cx.notify();
                        if add {
                            return;
                        }
                        let start = this
                            .workflow
                            .nodes
                            .iter()
                            .filter(|n| this.selected.contains(&n.id))
                            .map(|n| (n.id, (n.x, n.y)))
                            .collect();
                        this.drag = Some(Drag::Nodes {
                            from: e.position,
                            start,
                        });
                    }),
                )
                .child(Icon::new(kind_icon(&n.kind)).size_4().text_color(muted))
                .child(match &self.renaming {
                    Some((rid, state)) if *rid == id => div()
                        .flex_1()
                        .min_w_0()
                        // Selecting text in the box must not drag the node.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(Input::new(state).xsmall().focus_ring(false))
                        .into_any_element(),
                    _ => div()
                        .id(ElementId::Name(format!("title-{id}").into()))
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .font_semibold()
                        .truncate()
                        .child(n.title.clone())
                        .tooltip(|w, cx| {
                            Tooltip::new(t!("canvas.rename_hint").to_string()).build(w, cx)
                        })
                        .on_click(cx.listener(move |this, e: &ClickEvent, w, cx| {
                            if e.click_count() >= 2 {
                                this.start_rename(id, w, cx)
                            }
                        }))
                        .into_any_element(),
                })
                .child(
                    div()
                        .id(ElementId::Name(format!("badge-{id}").into()))
                        .text_xs()
                        .text_color(color)
                        .child(badge)
                        .when_some(failed, |el, why| {
                            el.tooltip(move |w, cx| Tooltip::new(why.clone()).build(w, cx))
                        }),
                )
                .child(
                    h_flex()
                        // A prompt node's big editor; up here, the box below keeps its full width.
                        .when(n.kind == "text", |el| {
                            el.child(
                                Button::new(ElementId::Name(format!("expand-{id}").into()))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Maximize2)
                                    .tooltip(t!("canvas.expand").to_string())
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        this.open_editor(id, w, cx)
                                    })),
                            )
                        })
                        .when_some(action, |el, merge| {
                            el.child(
                                // Merging is local ffmpeg work, not AI: no prompt, it just runs.
                                Button::new(ElementId::Name(
                                    format!("{}-{id}", if merge { "gen" } else { "edit" }).into(),
                                ))
                                .ghost()
                                .xsmall()
                                .icon(if merge {
                                    IconName::Combine
                                } else {
                                    IconName::Sparkles
                                })
                                .tooltip(match (merge, status) {
                                    (false, _) => t!("canvas.edit_generate").to_string(),
                                    (true, NodeStatus::Done) => t!("canvas.up_to_date").to_string(),
                                    (true, NodeStatus::Waiting) => {
                                        t!("canvas.merge_waiting").to_string()
                                    }
                                    (true, _) => t!("canvas.merge").to_string(),
                                })
                                .on_click(cx.listener(
                                    move |this, _, w, cx| match merge {
                                        true if !busy => this.generate(id, w, cx),
                                        true => {}
                                        false => this.open_editor(id, w, cx),
                                    },
                                )),
                            )
                        })
                        .when(
                            ["image", "video", "audio"].contains(&n.kind.as_str()),
                            |el| {
                                el.child(
                                    Button::new(ElementId::Name(format!("import-{id}").into()))
                                        .xsmall()
                                        .ghost()
                                        .icon(IconName::Upload)
                                        .tooltip(if n.out.is_some() {
                                            t!("canvas.replace").to_string()
                                        } else {
                                            t!("canvas.import").to_string()
                                        })
                                        .on_click(cx.listener(move |this, _, w, cx| {
                                            this.pick_file(id, w, cx)
                                        })),
                                )
                            },
                        )
                        .when_some(file.clone().filter(|_| n.kind == "merge"), |el, file| {
                            let title = n.title.clone();
                            el.child(
                                Button::new(ElementId::Name(format!("export-{id}").into()))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Download)
                                    .tooltip(t!("common.export").to_string())
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        this.export_cut(file.clone(), &title, w, cx)
                                    })),
                            )
                        })
                        .child(
                            Button::new(ElementId::Name(format!("remove-{id}").into()))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Trash)
                                .tooltip(t!("canvas.delete").to_string())
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    this.confirm_delete(vec![id], w, cx)
                                })),
                        ),
                );

        let preview = |hint: String| {
            div()
                .h(px((MEDIA_H - 16.) * z))
                .m_2()
                .rounded(theme.radius)
                .border_1()
                .border_dashed()
                .border_color(theme.border)
                .flex()
                .items_center()
                .justify_center()
                .p_3()
                .text_xs()
                .text_center()
                .text_color(muted)
                .child(div().w_full().min_w_0().child(hint))
        };
        let body: AnyElement = match n.kind.as_str() {
            "text" => {
                let state = self.text_state(&n, window, cx);
                div()
                    .p_1()
                    .child(
                        div()
                            .rounded(theme.radius)
                            .bg(theme.background)
                            .child(Textarea::new(&state).appearance(false)),
                    )
                    .into_any_element()
            }
            kind => match file {
                Some(file) => {
                    let playable = kind != "image";
                    let name = file
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let picture = picture.unwrap_or(Picture {
                        path: None,
                        aspect: None,
                    });
                    div()
                        .id(ElementId::Name(format!("media-{id}").into()))
                        .relative()
                        .h(px(media_h * z))
                        .cursor_pointer()
                        .on_click(move |_, _, _| {
                            let _ = open::that(&file);
                        })
                        // An audio clip has no picture: its name stands in.
                        .when(picture.path.is_none(), |el| el.child(preview(name)))
                        .children(picture.path.map(|p| {
                            img(p)
                                .size_full()
                                .object_fit(if picture.aspect.is_some() {
                                    ObjectFit::Contain
                                } else {
                                    ObjectFit::Cover
                                })
                                .rounded(theme.radius_lg)
                        }))
                        .when(playable, |el| {
                            el.child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .size(px(36. * z))
                                            .rounded_full()
                                            .bg(black().opacity(0.55))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(Icon::new(IconName::Play).text_color(white())),
                                    ),
                            )
                        })
                        .into_any_element()
                }
                None => preview(
                    match kind {
                        "image" | "video" | "merge" if busy => t!("canvas.hint_generating"),
                        "image" | "video" | "merge"
                            if wired_in && status == NodeStatus::Waiting =>
                        {
                            t!("canvas.hint_upstream")
                        }
                        "merge" if wired_in => t!("canvas.hint_merge_ready"),
                        "image" | "video" if wired_in => t!("canvas.hint_ready"),
                        "image" => t!("canvas.hint_image"),
                        "video" => t!("canvas.hint_video"),
                        "audio" => t!("canvas.hint_audio"),
                        _ => t!("canvas.hint_merge"),
                    }
                    .to_string(),
                )
                .into_any_element(),
            },
        };

        let card = v_flex()
            .id(("canvas-node", id as u64))
            .absolute()
            .left(self.offset.x + px(n.x as f32 * z))
            .top(self.offset.y + px(n.y as f32 * z))
            .w(px(NODE_W * z))
            // Clicks on a node are for the node, not for panning the canvas.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // A wire let go anywhere over the card connects to it.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, w, cx| {
                    if let Some(Drag::Wire { from, .. }) = this.drag
                        && from != id
                    {
                        this.drag = None;
                        this.wire(from, id, w, cx);
                    }
                }),
            )
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, w, cx| {
                cx.stop_propagation();
                this.drop_files(Some(id), paths.paths(), w, cx);
            }))
            // The header floats on the canvas; only the body is a panel.
            .child(header)
            .child(
                div()
                    .rounded(theme.radius_lg)
                    .overflow_hidden()
                    .border_1()
                    .when(chosen, |el| el.border_2())
                    .border_color(if picked || chosen {
                        theme.primary
                    } else {
                        transparent_black()
                    })
                    .when(wiring_from.is_some_and(|f| f != id), |el| {
                        el.hover(|s| s.border_color(cx.theme().primary))
                    })
                    .drag_over::<ExternalPaths>(|style, _, _, cx| {
                        style.border_color(cx.theme().primary)
                    })
                    .bg(theme.secondary)
                    .shadow_lg()
                    .child(body),
            )
            .children(input_dot)
            .child(output_dot);
        Scaled {
            rem: window.rem_size() * z,
            child: card.into_any_element(),
        }
        .into_any_element()
    }
}

fn output_media(kind: &str) -> &str {
    if kind == "merge" { "video" } else { kind }
}

impl Render for CanvasPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        if self.project.is_none() {
            return div()
                .id("short-canvas")
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(t!("canvas.no_project").to_string());
        }

        // Dot grid and wires, painted under the nodes.
        let node = |id: i64| self.workflow.node(id);
        let wires: Vec<(Point<Pixels>, Point<Pixels>)> = self
            .workflow
            .edges
            .iter()
            .filter_map(|e| {
                Some((
                    self.port_at(node(e.from)?, true),
                    self.port_at(node(e.to)?, false),
                ))
            })
            .collect();
        // The wire being pulled: from its output dot (canvas coordinates) to the cursor (window).
        let live = match self.drag {
            Some(Drag::Wire { from, at }) => node(from).map(|n| (self.port_at(n, true), at)),
            _ => None,
        };
        let (wire, grid, offset) = (
            theme.primary.opacity(0.7),
            theme.border.opacity(0.6),
            self.offset,
        );
        let z = self.zoom;
        // Dots stay readable when zoomed far out.
        let step = GRID * if z < 0.6 { z * 2. } else { z };
        let seen = self.bounds.clone();
        let surface_paint = canvas(
            move |bounds, _, _| seen.set(bounds),
            move |bounds, _, window, _| {
                let (ox, oy) = (
                    f32::from(offset.x).rem_euclid(step),
                    f32::from(offset.y).rem_euclid(step),
                );
                let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                let mut y = oy;
                while y < h {
                    let mut x = ox;
                    while x < w {
                        window.paint_quad(fill(
                            Bounds::new(
                                bounds.origin + point(px(x), px(y)),
                                size(px(1.5), px(1.5)),
                            ),
                            grid,
                        ));
                        x += step;
                    }
                    y += step;
                }
                let pulled = live.map(|(a, b)| (a + bounds.origin, b));
                for (a, b) in wires
                    .iter()
                    .map(|(a, b)| (*a + bounds.origin, *b + bounds.origin))
                    .chain(pulled)
                {
                    let dx = ((b.x - a.x).abs() / 2.).max(px(60. * z));
                    let mut path = PathBuilder::stroke(px(2.));
                    path.move_to(a);
                    path.cubic_bezier_to(b, point(a.x + dx, a.y), point(b.x - dx, b.y));
                    if let Ok(p) = path.build() {
                        window.paint_path(p, wire);
                    }
                }
            },
        )
        .absolute()
        .size_full();

        // A small × in the middle of each wire cuts it.
        let cuts: Vec<AnyElement> = self
            .workflow
            .edges
            .iter()
            .filter_map(|e| {
                let (p, q) = (
                    self.port_at(node(e.from)?, true),
                    self.port_at(node(e.to)?, false),
                );
                let mid = point((p.x + q.x) / 2., (p.y + q.y) / 2.);
                let eid = e.id;
                Some(
                    div()
                        .id(("cut", eid as u64))
                        .absolute()
                        .left(mid.x - px(8.))
                        .top(mid.y - px(8.))
                        .size(px(16.))
                        .rounded_full()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.primary.opacity(0.7))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(muted)
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.workflow.disconnect(eid);
                            this.done(Ok(()), w, cx)
                        }))
                        .child("×")
                        .into_any_element(),
                )
            })
            .collect();

        let nodes: Vec<AnyElement> = self
            .workflow
            .nodes
            .clone()
            .into_iter()
            .map(|n| self.render_node(n, window, cx))
            .collect();

        let origin = self.bounds.get().origin;
        let marquee = match self.drag {
            Some(Drag::Marquee { from, at }) => {
                let (a, b) = (from - origin, at - origin);
                Some(
                    div()
                        .absolute()
                        .left(a.x.min(b.x))
                        .top(a.y.min(b.y))
                        .w((a.x - b.x).abs())
                        .h((a.y - b.y).abs())
                        .border_1()
                        .border_color(theme.primary)
                        .bg(theme.primary.opacity(0.08)),
                )
            }
            _ => None,
        };
        let minimap = self.minimap_frame().map(|(mx, my, scale)| {
            let b = self.bounds.get();
            let rect = |x: f32, y: f32, w: f32, h: f32| {
                div()
                    .absolute()
                    .left(px((x - mx) * scale))
                    .top(px((y - my) * scale))
                    .w(px((w * scale).max(2.)))
                    .h(px((h * scale).max(2.)))
            };
            let view = rect(
                -f32::from(self.offset.x) / z,
                -f32::from(self.offset.y) / z,
                f32::from(b.size.width) / z,
                f32::from(b.size.height) / z,
            )
            .border_1()
            .border_color(theme.primary);
            div()
                .id("minimap")
                .absolute()
                .right(px(12.))
                .bottom(px(12.))
                .w(px(MINI.0))
                .h(px(MINI.1))
                .overflow_hidden()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background.opacity(0.9))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, e: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.drag = Some(Drag::Minimap);
                        this.minimap_jump(e.position);
                        cx.notify();
                    }),
                )
                .children(self.workflow.nodes.iter().map(|n| {
                    rect(n.x as f32, n.y as f32, NODE_W, NODE_H).bg(
                        if self.selected.contains(&n.id) {
                            theme.primary
                        } else {
                            theme.muted_foreground.opacity(0.4)
                        },
                    )
                }))
                .child(view)
        });

        let add = |kind: &'static str, icon: IconName| {
            Button::new(ElementId::Name(format!("add-{kind}").into()))
                .small()
                .ghost()
                .icon(icon)
                .label(crate::workflow::kind_name(kind))
                .on_click(cx.listener(move |this, _, w, cx| this.add(kind, w, cx)))
        };
        let open = self.project.clone().unwrap_or_default();
        let ready = self.ready(&open, &self.workflow).len();
        let running = self
            .workflow
            .nodes
            .iter()
            .filter(|n| self.busy(n.id))
            .count();
        let toolbar = h_flex()
            .absolute()
            .top_3()
            .left_3()
            .gap_1()
            .p_1()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_md()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(add("text", IconName::MessageSquare))
            .child(add("image", IconName::Image))
            .child(add("video", IconName::Video))
            .child(add("audio", IconName::AudioLines))
            .child(add("merge", IconName::Layers))
            .child(div().w(px(1.)).h(px(18.)).mx_1().bg(theme.border))
            .child(
                Button::new("zoom-out")
                    .small()
                    .ghost()
                    .icon(IconName::Minus)
                    .tooltip(t!("canvas.zoom_out").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        let c = this.canvas_center();
                        this.zoom_to(this.zoom / 1.25, c);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("zoom-reset")
                    .small()
                    .ghost()
                    .label(format!("{:.0}%", self.zoom * 100.))
                    .tooltip(t!("canvas.zoom_reset").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        let c = this.canvas_center();
                        this.zoom_to(1.0, c);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("zoom-in")
                    .small()
                    .ghost()
                    .icon(IconName::Plus)
                    .tooltip(t!("canvas.zoom_in").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        let c = this.canvas_center();
                        this.zoom_to(this.zoom * 1.25, c);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("zoom-fit")
                    .small()
                    .ghost()
                    .icon(IconName::Maximize2)
                    .tooltip(t!("canvas.fit").to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.fit();
                        cx.notify();
                    })),
            )
            .when(self.selected.len() > 1, |el| {
                el.child(
                    Button::new("delete-selected")
                        .small()
                        .ghost()
                        .icon(IconName::Trash)
                        .label(
                            t!("canvas.delete_selected", count = self.selected.len()).to_string(),
                        )
                        .on_click(cx.listener(|this, _, w, cx| {
                            let ids = this.selected.iter().copied().collect();
                            this.confirm_delete(ids, w, cx)
                        })),
                )
            })
            .child(div().w(px(1.)).h(px(18.)).mx_1().bg(theme.border))
            .child(
                Button::new("run-all")
                    .small()
                    .primary()
                    .icon(IconName::Play)
                    .label(if running > 0 {
                        t!("canvas.running", count = running).to_string()
                    } else {
                        t!("canvas.run_all", count = ready).to_string()
                    })
                    .disabled(ready == 0)
                    .tooltip(t!("canvas.run_all_hint").to_string())
                    .on_click(cx.listener(|this, _, w, cx| {
                        let ids: Vec<i64> = this.workflow.nodes.iter().map(|n| n.id).collect();
                        for id in ids {
                            this.flush_text(id, cx);
                        }
                        this.done(Ok(()), w, cx);
                        if let Some(dir) = this.project.clone() {
                            this.auto.insert(dir.clone());
                            this.run_ready(dir, w, cx);
                        }
                    })),
            );
        let empty = self.workflow.nodes.is_empty().then(|| {
            v_flex()
                .absolute()
                .inset_0()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Icon::new(IconName::Workflow).size_8().text_color(muted))
                .child(
                    div()
                        .font_semibold()
                        .child(t!("canvas.empty_title").to_string()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(t!("canvas.empty_hint").to_string()),
                )
        });

        div()
            .id("short-canvas")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(theme.secondary.opacity(0.25))
            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                style.bg(cx.theme().primary.opacity(0.05))
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, w, cx| {
                this.drop_files(None, paths.paths(), w, cx)
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.drag = Some(if e.modifiers.shift {
                        Drag::Marquee {
                            from: e.position,
                            at: e.position,
                        }
                    } else {
                        this.selected.clear();
                        Drag::Pan {
                            from: e.position,
                            start: this.offset,
                        }
                    });
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                let z = f64::from(this.zoom);
                match &mut this.drag {
                    Some(Drag::Pan { from, start }) => this.offset = *start + (e.position - *from),
                    Some(Drag::Nodes { from, start }) => {
                        let d = e.position - *from;
                        let (dx, dy) =
                            (f64::from(f32::from(d.x)) / z, f64::from(f32::from(d.y)) / z);
                        for (id, (x, y)) in start.clone() {
                            if let Some(n) = this.workflow.node_mut(id) {
                                (n.x, n.y) = (x + dx, y + dy);
                            }
                        }
                    }
                    Some(Drag::Wire { at, .. } | Drag::Marquee { at, .. }) => *at = e.position,
                    Some(Drag::Minimap) => this.minimap_jump(e.position),
                    None => return,
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, w, cx| {
                    // A wire let go over empty canvas is dropped.
                    cx.notify();
                    match this.drag.take() {
                        Some(Drag::Nodes { start, .. }) => {
                            let moved = start.iter().any(|(id, was)| {
                                this.workflow.node(*id).is_some_and(|n| (n.x, n.y) != *was)
                            });
                            if moved {
                                this.done(Ok(()), w, cx);
                            }
                        }
                        Some(Drag::Marquee { from, at }) => this.select_box(from, at),
                        _ => {}
                    }
                }),
            )
            .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| {
                let d = e.delta.pixel_delta(px(16.));
                if e.modifiers.platform || e.modifiers.control {
                    let at = e.position - this.bounds.get().origin;
                    let z = this.zoom * (1.0 + f32::from(d.y) / 300.);
                    this.zoom_to(z, at);
                } else {
                    this.offset += d;
                }
                cx.notify();
            }))
            .child(surface_paint)
            // Wire cutters under the nodes, so a wire passing behind a card never covers it.
            .children(cuts)
            .children(nodes)
            .children(marquee)
            .children(empty)
            .children(minimap)
            .child(toolbar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::std::prelude::v1::test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let (offset, at) = (point(px(60.), px(80.)), point(px(400.), px(300.)));
        let o2 = zoom_about(offset, at, 1.0, 2.0);
        // The world point under `at` is (at - offset) / zoom, before and after.
        assert_eq!((at.x - o2.x) / 2., at.x - offset.x);
        assert_eq!((at.y - o2.y) / 2., at.y - offset.y);
    }
}
