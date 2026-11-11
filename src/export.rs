//! The export dialog's form: file name, folder, frame rate, aspect and fit, with a live summary.

use std::path::{Path, PathBuf};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonGroup},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::server::{RenderOptions, export_size};

const FPS: [f64; 4] = [24.0, 25.0, 30.0, 60.0];
/// `None` is the film's own frame.
const ASPECTS: [(Option<[u32; 2]>, &str); 5] = [
    (None, ""),
    (Some([16, 9]), "16:9"),
    (Some([9, 16]), "9:16"),
    (Some([1, 1]), "1:1"),
    (Some([4, 5]), "4:5"),
];

/// What the last export used, offered again next time.
#[derive(Clone, Debug)]
pub struct ExportSettings {
    /// `None` is the project's own folder.
    pub dir: Option<PathBuf>,
    pub fps: f64,
    pub aspect: Option<[u32; 2]>,
    pub crop: bool,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            dir: None,
            fps: 30.0,
            aspect: None,
            crop: false,
        }
    }
}

pub struct ExportForm {
    project_dir: PathBuf,
    width: u32,
    height: u32,
    duration: f32,
    name: Entity<InputState>,
    dir: PathBuf,
    fps: f64,
    aspect: Option<[u32; 2]>,
    crop: bool,
    error: Option<String>,
}

impl ExportForm {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        project: &str,
        project_dir: PathBuf,
        (width, height, duration): (u32, u32, f32),
        last: &ExportSettings,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).default_value(project.to_string()));
        cx.observe(&name, |this, _, cx| {
            this.error = None;
            cx.notify();
        })
        .detach();
        // An aspect that is the film's own frame is the same as "原始".
        let aspect = last
            .aspect
            .filter(|_| export_size(width, height, last.aspect).is_some());
        Self {
            dir: last.dir.clone().unwrap_or_else(|| project_dir.clone()),
            project_dir,
            width,
            height,
            duration,
            name,
            fps: last.fps,
            aspect,
            crop: last.crop,
            error: None,
        }
    }

    /// The file the export will write, or why it cannot.
    fn file(&self, cx: &App) -> Result<PathBuf, String> {
        let name = self.name.read(cx).value().trim().to_string();
        let stem = name.strip_suffix(".mp4").unwrap_or(&name).trim();
        if stem.is_empty() {
            return Err(t!("export.name_required").to_string());
        }
        if stem.contains('/') || stem.starts_with('.') {
            return Err(t!("export.name_invalid").to_string());
        }
        Ok(self.dir.join(format!("{stem}.mp4")))
    }

    /// The options to export with, and the settings to offer next time.
    pub fn submit(&mut self, cx: &mut Context<Self>) -> Option<(RenderOptions, ExportSettings)> {
        match self.file(cx) {
            Ok(file) => Some((
                RenderOptions {
                    fps: self.fps,
                    out: Some(file),
                    aspect: self.aspect,
                    crop: self.crop,
                },
                ExportSettings {
                    dir: (self.dir != self.project_dir).then(|| self.dir.clone()),
                    fps: self.fps,
                    aspect: self.aspect,
                    crop: self.crop,
                },
            )),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                None
            }
        }
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.read(cx).focus_handle(cx).clone()
    }

    fn choose_dir(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("common.choose").to_string().into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(dir) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| {
                    this.dir = dir;
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

/// `~/Movies/x` rather than `/Users/me/Movies/x`.
fn tidy(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn clock(t: f32) -> String {
    let t = t.max(0.0);
    format!("{:02}:{:05.2}", (t / 60.0).floor() as u32, t % 60.0)
}

/// A labelled row: the label column is wide enough for the longest translation and never
/// wraps, so every control starts on the same vertical line.
fn row(label: impl Into<SharedString>, control: impl IntoElement, cx: &App) -> Div {
    let label: SharedString = label.into();
    h_flex()
        .gap_4()
        .items_center()
        .child(
            div()
                .w(px(96.))
                .flex_none()
                .whitespace_nowrap()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(control))
}

impl Render for ExportForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (out_w, out_h) =
            export_size(self.width, self.height, self.aspect).unwrap_or((self.width, self.height));
        let frames = (self.duration as f64 * self.fps).ceil().max(1.0) as u64;
        let file = self.file(cx);
        let exists = file.as_ref().is_ok_and(|file| file.exists());
        let mono = cx.theme().mono_font_family.clone();
        let fps_now = self.fps;
        let aspect_now = self.aspect;
        let crop_now = self.crop;
        // Aspects that are the film's own frame collapse into "原始".
        let aspects: Vec<_> = ASPECTS
            .iter()
            .filter(|(aspect, _)| {
                aspect.is_none() || export_size(self.width, self.height, *aspect).is_some()
            })
            .copied()
            .collect();
        let aspect_values: Vec<_> = aspects.iter().map(|(aspect, _)| *aspect).collect();

        v_flex()
            .gap_4()
            .pt_1()
            .child(row(
                t!("export.filename").to_string(),
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.name)
                                .small()
                                .bordered(false)
                                .focus_bordered(false),
                        ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .font_family(mono.clone())
                            .child(".mp4"),
                    ),
                cx,
            ))
            .child(row(
                t!("export.save_to").to_string(),
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .h(px(28.))
                            .px_2()
                            .gap_2()
                            .items_center()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().muted)
                            .child(
                                Icon::new(IconName::Folder)
                                    .xsmall()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(div().text_sm().truncate().child(tidy(&self.dir))),
                    )
                    .child(
                        Button::new("choose-dir")
                            .small()
                            .outline()
                            .label(t!("common.choose").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.choose_dir(cx))),
                    ),
                cx,
            ))
            .child(row(
                t!("export.fps").to_string(),
                ButtonGroup::new("fps")
                    .small()
                    .outline()
                    .children(FPS.iter().map(|fps| {
                        Button::new(SharedString::from(format!("fps-{fps}")))
                            .label(format!("{fps}"))
                            .selected(*fps == fps_now)
                    }))
                    .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                        if let Some(fps) = clicked.first().and_then(|i| FPS.get(*i)) {
                            this.fps = *fps;
                            cx.notify();
                        }
                    })),
                cx,
            ))
            .child(row(
                t!("export.aspect").to_string(),
                ButtonGroup::new("aspect")
                    .small()
                    .outline()
                    .children(aspects.iter().enumerate().map(|(i, (aspect, label))| {
                        Button::new(("aspect", i))
                            .label(if aspect.is_none() {
                                t!("export.original").to_string()
                            } else {
                                (*label).to_string()
                            })
                            .selected(*aspect == aspect_now)
                    }))
                    .on_click(cx.listener(move |this, clicked: &Vec<usize>, _, cx| {
                        if let Some(aspect) = clicked.first().and_then(|i| aspect_values.get(*i)) {
                            this.aspect = *aspect;
                            cx.notify();
                        }
                    })),
                cx,
            ))
            .child(row(
                t!("export.fit").to_string(),
                ButtonGroup::new("fit")
                    .small()
                    .outline()
                    .disabled(aspect_now.is_none())
                    .child(
                        Button::new("fit-pad")
                            .label(t!("export.pad").to_string())
                            .selected(!crop_now),
                    )
                    .child(
                        Button::new("fit-crop")
                            .label(t!("export.crop").to_string())
                            .selected(crop_now),
                    )
                    .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                        if let Some(i) = clicked.first() {
                            this.crop = *i == 1;
                            cx.notify();
                        }
                    })),
                cx,
            ))
            .child(
                v_flex()
                    .gap_1p5()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(t!("export.output").to_string()),
                            )
                            .child(
                                div().text_sm().font_family(mono.clone()).child(
                                    t!(
                                        "export.output_summary",
                                        width = out_w,
                                        height = out_h,
                                        fps = self.fps,
                                        time = clock(self.duration),
                                        frames = frames
                                    )
                                    .to_string(),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(match (self.aspect, self.crop) {
                                (None, _) => t!("export.codec_original").to_string(),
                                (Some(_), false) => t!("export.codec_pad").to_string(),
                                (Some(_), true) => t!("export.codec_crop").to_string(),
                            }),
                    )
                    .when_some(file.as_ref().ok(), |this, file| {
                        this.child(
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(cx.theme().muted_foreground)
                                .font_family(mono.clone())
                                .child(tidy(file)),
                        )
                    }),
            )
            .when(exists, |this| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .text_color(cx.theme().warning)
                        .child(Icon::new(IconName::TriangleAlert).xsmall())
                        .child(t!("export.overwrite").to_string()),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_xs().text_color(cx.theme().danger).child(error))
            })
    }
}
