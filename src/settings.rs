//! The settings dialog: interface language, the library location, and the services for
//! speech, image and video generation. Every edit takes effect at once: the form emits
//! [`SettingsChanged`] and the app applies and saves it.

use std::{cell::Cell, path::PathBuf, rc::Rc};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    group_box::GroupBoxVariant,
    h_flex,
    input::{Input, InputState},
    setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    switch::Switch,
    tag::Tag,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use rust_i18n::t;

use crate::{
    config::{Config, Service, Services, SpareService},
    i18n, library,
};

pub struct SettingsForm {
    locale: SharedString,
    library_dir: PathBuf,
    services: Services,
    /// The add/edit form, shown instead of the service list while open.
    editor: Option<Entity<ServiceEditor>>,
    /// Window x of the service groups' right edge and of the title row's left, from the last
    /// frame: the page header cannot right-align a title suffix, so the add button is laid out
    /// from these.
    groups_right: Rc<Cell<Pixels>>,
    title_left: Rc<Cell<Pixels>>,
}

/// Something in the form changed; read it back with [`SettingsForm::locale`] and
/// [`SettingsForm::services`].
pub struct SettingsChanged;

impl EventEmitter<SettingsChanged> for SettingsForm {}

/// Where a listed service lives in [`Services`].
#[derive(Clone, Copy)]
enum Slot {
    Active,
    Spare(usize),
}

const KINDS: [(&str, Lucide); 3] = [
    ("speech", Lucide::AudioLines),
    ("image", Lucide::Image),
    ("video", Lucide::Video),
];

impl SettingsForm {
    pub fn new(_: &mut Window, _: &mut Context<Self>, config: &Config) -> Self {
        Self {
            locale: i18n::current().into(),
            library_dir: config.library_dir.clone(),
            services: config.services.clone(),
            editor: None,
            groups_right: Rc::default(),
            title_left: Rc::default(),
        }
    }

    /// The language picked in the dialog.
    pub fn locale(&self) -> String {
        self.locale.to_string()
    }

    /// The services the dialog holds, ready to save.
    pub fn services(&self, _: &App) -> Services {
        self.services.clone()
    }

    fn change(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut Services)) {
        edit(&mut self.services);
        cx.emit(SettingsChanged);
        cx.notify();
    }

    /// `kind`'s services, the one in use first.
    fn listed(&self, kind: &str) -> Vec<(Slot, Service)> {
        let active = self.services.active(kind);
        let spares = self.services.spare.iter().enumerate();
        (!active.is_empty())
            .then(|| (Slot::Active, active.clone()))
            .into_iter()
            .chain(
                spares
                    .filter(|(_, spare)| spare.kind == kind)
                    .map(|(i, spare)| (Slot::Spare(i), spare.service.clone())),
            )
            .collect()
    }

    /// Store an edited service; a new one is put in use when nothing of its kind is.
    fn save(&mut self, kind: &str, slot: Option<Slot>, service: Service, cx: &mut Context<Self>) {
        self.change(cx, |services| match slot {
            Some(Slot::Active) => *services.active_mut(kind) = service,
            Some(Slot::Spare(i)) => services.spare[i].service = service,
            None if services.active(kind).is_empty() => *services.active_mut(kind) = service,
            None => services.spare.push(SpareService {
                kind: kind.to_string(),
                service,
            }),
        });
    }

    fn general_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let this = cx.entity();
        let languages = i18n::LANGUAGES
            .iter()
            .map(|(code, name)| (SharedString::from(*code), SharedString::from(*name)))
            .collect();
        let language = SettingField::dropdown(
            languages,
            {
                let this = this.clone();
                move |cx: &App| this.read(cx).locale.clone()
            },
            move |locale, cx: &mut App| {
                this.update(cx, |form, cx| {
                    form.locale = locale;
                    cx.emit(SettingsChanged);
                    cx.notify();
                })
            },
        );
        let dir = self.library_dir.clone();
        let reveal = SettingField::render(move |_, _, _| {
            let dir = dir.clone();
            Button::new("reveal-library")
                .small()
                .label(i18n::reveal())
                .on_click(move |_, _, _| library::reveal_in_file_manager(&dir))
        });

        SettingPage::new(t!("settings.general").to_string())
            .icon(IconName::Settings)
            .resettable(false)
            // One untitled group: titled groups would grow a sub-tree under the page in the sidebar.
            .group(
                SettingGroup::new()
                    .item(
                        SettingItem::new(t!("settings.language").to_string(), language)
                            .description(t!("settings.language_hint").to_string()),
                    )
                    .item(
                        SettingItem::new(t!("settings.library").to_string(), reveal)
                            .description(self.library_dir.display().to_string()),
                    ),
            )
    }

    fn services_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let form = cx.entity();
        let page = SettingPage::new(t!("settings.services").to_string())
            .icon(Lucide::Sparkles)
            .resettable(false);
        if let Some(editor) = self.editor.clone() {
            return page.group(
                SettingGroup::new().item(SettingItem::render(move |_, _, _| editor.clone())),
            );
        }
        let groups = KINDS.map(|(kind, icon)| {
            let listed = self.listed(kind);
            let count = listed.len();
            let right = self.groups_right.clone();
            let header = SettingItem::render(move |_, _, cx| {
                h_flex()
                    .relative()
                    .w_full()
                    .child(measure(right.clone(), |b| b.right()))
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(icon)
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .font_semibold()
                            .child(t!(format!("settings.{kind}")).to_string()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(count.to_string()),
                    )
            });
            let cards = listed.into_iter().map(|(slot, service)| {
                let form = form.clone();
                SettingItem::render(move |_, _, cx| {
                    service_card(&form, kind, icon, slot, &service, cx)
                })
            });
            let group = SettingGroup::new()
                .variant(GroupBoxVariant::Normal)
                .item(header);
            if count == 0 {
                group.item(SettingItem::render(|_, _, cx| {
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("settings.none").to_string())
                }))
            } else {
                group.items(cards)
            }
        });
        // The form starts on the first kind still missing a service; the cards switch it.
        let first = KINDS
            .iter()
            .map(|(kind, _)| *kind)
            .find(|kind| self.services.active(kind).is_empty())
            .unwrap_or("video");
        // In the page header, level with the title and right-aligned with the cards below.
        // The suffix sits in the title's own row, which is only as wide as the title, so it is
        // stretched from there to the groups' right edge, measured on the last frame.
        let (right, left) = (self.groups_right.clone(), self.title_left.clone());
        let width = right.get() - left.get();
        page.description(t!("settings.hint").to_string())
            .title_suffix(move |_, _| {
                let form = form.clone();
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(width.max(px(0.)))
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(measure(left.clone(), |b| b.left()))
                    // Until both edges are known it would land in the wrong place for a frame.
                    .when(width > px(0.), |el| {
                        el.child(
                            Button::new("add-service")
                                .small()
                                .primary()
                                .icon(Lucide::Plus)
                                .label(t!("settings.add").to_string())
                                .on_click(move |_, window, cx| {
                                    open_editor(
                                        form.clone(),
                                        first,
                                        None,
                                        Service::default(),
                                        window,
                                        cx,
                                    )
                                }),
                        )
                    })
            })
            .groups(groups)
    }
}

/// An invisible layer over its parent that keeps one edge of it, in window coordinates, and
/// draws the next frame when that edge moved.
fn measure(cell: Rc<Cell<Pixels>>, edge: fn(&Bounds<Pixels>) -> Pixels) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            let at = edge(&bounds);
            if cell.replace(at) != at {
                window.refresh();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

/// A service at a glance, with its on switch, edit and delete.
fn service_card(
    form: &Entity<SettingsForm>,
    kind: &'static str,
    icon: Lucide,
    slot: Slot,
    service: &Service,
    cx: &App,
) -> impl IntoElement + use<> {
    let active = matches!(slot, Slot::Active);
    let id = match slot {
        Slot::Active => format!("{kind}-active"),
        Slot::Spare(i) => format!("spare-{i}"),
    };
    let provider = t!(format!("settings.provider_{}", service.provider())).to_string();
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let on_toggle = {
        let form = form.clone();
        move |_: &bool, _: &mut Window, cx: &mut App| {
            form.update(cx, |form, cx| {
                form.change(cx, |services| match slot {
                    Slot::Active => services.deactivate(kind),
                    Slot::Spare(i) => services.activate(i),
                })
            })
        }
    };
    let on_edit = {
        let form = form.clone();
        let service = service.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            open_editor(form.clone(), kind, Some(slot), service.clone(), window, cx)
        }
    };
    let on_delete = {
        let form = form.clone();
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            form.update(cx, |form, cx| {
                form.change(cx, |services| match slot {
                    Slot::Active => *services.active_mut(kind) = Service::default(),
                    Slot::Spare(i) => {
                        services.spare.remove(i);
                    }
                })
            })
        }
    };
    h_flex()
        .w_full()
        .items_start()
        .gap_3()
        .p_4()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_none()
                .size_9()
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme.radius)
                .bg(theme.muted)
                .child(Icon::new(icon).small()),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_base()
                                .font_semibold()
                                .child(service.model.clone()),
                        )
                        .child(Tag::secondary().small().child(provider))
                        .when(active, |this| {
                            this.child(
                                Tag::success()
                                    .outline()
                                    .small()
                                    .child(t!("settings.in_use").to_string()),
                            )
                        }),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .font_family(theme.mono_font_family.clone())
                        .truncate()
                        .child(service.url.clone()),
                ),
        )
        .child(
            h_flex()
                .flex_none()
                .gap_2()
                .items_center()
                .child(
                    Switch::new(SharedString::from(format!("{id}-on")))
                        .checked(active)
                        .tooltip(t!("settings.in_use").to_string())
                        .on_click(on_toggle),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-edit")))
                        .small()
                        .ghost()
                        .icon(Lucide::Pencil)
                        .tooltip(t!("settings.edit").to_string())
                        .on_click(on_edit),
                )
                .child(
                    Button::new(SharedString::from(format!("{id}-delete")))
                        .small()
                        .ghost()
                        .icon(Lucide::Trash)
                        .tooltip(t!("settings.delete").to_string())
                        .on_click(on_delete),
                ),
        )
}

/// The add/edit form, shown in the services page in place of the list.
struct ServiceEditor {
    form: WeakEntity<SettingsForm>,
    /// Where the edited service is; `None` adds one.
    slot: Option<Slot>,
    kind: &'static str,
    provider: SharedString,
    url: Entity<InputState>,
    key: Entity<InputState>,
    model: Entity<InputState>,
}

impl ServiceEditor {
    fn new(
        form: WeakEntity<SettingsForm>,
        kind: &'static str,
        slot: Option<Slot>,
        service: &Service,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut input = |value: &str, secret: bool, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(value.to_string())
                    .masked(secret)
            })
        };
        let mut editor = Self {
            form,
            slot,
            kind,
            provider: service.provider().to_string().into(),
            url: input(&service.url, false, cx),
            key: input(&service.api_key, true, cx),
            model: input(&service.model, false, cx),
        };
        if !PROVIDERS
            .iter()
            .any(|(k, id, _, _)| *k == kind && *id == editor.provider.as_ref())
        {
            editor.provider = "openai".into();
        }
        editor.key.update(cx, |key, cx| {
            key.set_placeholder(t!("settings.key_placeholder").to_string(), window, cx)
        });
        editor.sync_placeholders(window, cx);
        editor
    }

    /// The picked provider's example URL and model.
    fn defaults(&self) -> (&'static str, &'static str) {
        defaults(self.kind, &self.provider)
    }

    fn sync_placeholders(&self, window: &mut Window, cx: &mut App) {
        let (url, model) = self.defaults();
        self.url
            .update(cx, |input, cx| input.set_placeholder(url, window, cx));
        self.model
            .update(cx, |input, cx| input.set_placeholder(model, window, cx));
    }

    /// The service as filled in; blank URL and model take the provider's examples.
    fn service(&self, cx: &App) -> Service {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let (url, model) = self.defaults();
        let or = |value: String, default: &str| {
            if value.is_empty() {
                default.to_string()
            } else {
                value
            }
        };
        Service {
            provider: self.provider.to_string(),
            url: or(value(&self.url), url),
            api_key: value(&self.key),
            model: or(value(&self.model), model),
        }
    }

    /// Back to the list, saving the service first when `save`.
    fn close(&mut self, save: bool, cx: &mut Context<Self>) {
        let (kind, slot, service) = (self.kind, self.slot, self.service(cx));
        self.form
            .update(cx, |form, cx| {
                if save {
                    form.save(kind, slot, service, cx);
                }
                form.editor = None;
                cx.notify();
            })
            .ok();
    }

    /// One provider to pick: its name and what it is for.
    fn card(&self, kind: &'static str, id: &'static str, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme();
        let on = self.kind == kind && self.provider.as_ref() == id;
        let icon = KINDS
            .iter()
            .find(|(k, _)| *k == kind)
            .map_or(Lucide::Sparkles, |(_, icon)| *icon);
        v_flex()
            .id(SharedString::from(format!("provider-{kind}-{id}")))
            .px_3()
            .py_1p5()
            .gap_0p5()
            .rounded(theme.radius)
            .border_1()
            .cursor_pointer()
            .when(on, |d| {
                d.border_color(theme.primary)
                    .bg(theme.primary.opacity(0.08))
            })
            .when(!on, |d| {
                d.border_color(theme.border)
                    .hover(|d| d.bg(theme.secondary.opacity(0.5)))
            })
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .font_medium()
                    .child(Icon::new(icon).size_3p5())
                    .child(
                        div()
                            .truncate()
                            .child(t!(format!("settings.provider_{id}")).to_string()),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!(format!("settings.{kind}")).to_string()),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.kind = kind;
                this.provider = id.into();
                this.sync_placeholders(window, cx);
                cx.notify();
            }))
    }
}

/// What to know about a provider's URL or model beyond the example.
fn provider_note(provider: &str) -> Option<String> {
    match provider {
        "h3" => Some(t!("settings.note_h3").to_string()),
        "doubao" => Some(t!("settings.note_doubao").to_string()),
        "mimo" => Some(t!("settings.note_mimo").to_string()),
        _ => None,
    }
}

impl Render for ServiceEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let editing = self.slot.is_some();
        let field = |label: String, control: AnyElement, hint: Option<String>| {
            v_flex()
                .gap_1p5()
                .child(div().text_sm().font_medium().child(label))
                .child(control)
                .when_some(hint, |el, h| {
                    el.child(div().text_xs().text_color(theme.muted_foreground).child(h))
                })
        };
        // A service keeps its kind once added: only its own providers are offered then.
        let mut providers = v_flex().gap_2();
        for (kind, _) in KINDS {
            if editing && kind != self.kind {
                continue;
            }
            let cards: Vec<_> = PROVIDERS
                .iter()
                .filter(|(k, ..)| *k == kind)
                .map(|(_, id, _, _)| self.card(kind, id, cx))
                .collect();
            // The kind in a narrow left column keeps the picker short.
            providers = providers.child(
                h_flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .w(px(48.))
                            .flex_none()
                            .pt_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!(format!("settings.{kind}")).to_string()),
                    )
                    .child(div().flex_1().grid().grid_cols(3).gap_2().children(cards)),
            );
        }
        let (url, model) = self.defaults();
        let example =
            (!url.is_empty()).then(|| t!("settings.example", url = url, model = model).to_string());
        let url_hint = match (example, provider_note(&self.provider)) {
            (Some(example), Some(note)) => Some(format!("{example} · {note}")),
            (example, note) => example.or(note),
        };
        v_flex()
            .w_full()
            .gap_4()
            .child(
                h_flex()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div().text_lg().font_semibold().child(
                                    t!(if editing {
                                        "settings.edit"
                                    } else {
                                        "settings.add"
                                    })
                                    .to_string(),
                                ),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(t!("settings.form_hint").to_string()),
                            ),
                    )
                    .child(
                        Button::new("service-back")
                            .small()
                            .ghost()
                            .icon(Lucide::ArrowLeft)
                            .label(t!("settings.back").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.close(false, cx))),
                    ),
            )
            .child(field(
                t!("settings.provider").to_string(),
                providers.into_any_element(),
                None,
            ))
            .child(field(
                t!("settings.url").to_string(),
                Input::new(&self.url).into_any_element(),
                url_hint,
            ))
            .child(field(
                t!("settings.model").to_string(),
                Input::new(&self.model).into_any_element(),
                None,
            ))
            .child(field(
                t!("settings.key").to_string(),
                Input::new(&self.key).mask_toggle().into_any_element(),
                Some(t!("settings.key_hint").to_string()),
            ))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("service-cancel")
                            .small()
                            .outline()
                            .label(t!("common.cancel").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.close(false, cx))),
                    )
                    .child(
                        Button::new("service-save")
                            .small()
                            .primary()
                            .label(t!("common.save").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.close(true, cx))),
                    ),
            )
    }
}

/// Add (`slot` is `None`) or edit a service: the services page shows the form until it closes.
fn open_editor(
    form: Entity<SettingsForm>,
    kind: &'static str,
    slot: Option<Slot>,
    service: Service,
    window: &mut Window,
    cx: &mut App,
) {
    let weak = form.downgrade();
    let editor = cx.new(|cx| ServiceEditor::new(weak, kind, slot, &service, window, cx));
    form.update(cx, |form, cx| {
        form.editor = Some(editor);
        cx.notify();
    });
}

/// `provider`'s example URL and model for `kind`.
fn defaults(kind: &str, provider: &str) -> (&'static str, &'static str) {
    PROVIDERS
        .iter()
        .find(|(k, id, _, _)| *k == kind && *id == provider)
        .map_or(("", ""), |(_, _, url, model)| (*url, *model))
}

impl Render for SettingsForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let general = self.general_page(cx);
        // The dialog's title and padding take about 160px of the window.
        let height = (window.viewport_size().height - px(160.)).min(px(720.));
        div().h(height).child(
            Settings::new("settings")
                .sidebar_width(px(180.))
                .with_group_variant(GroupBoxVariant::Outline)
                .page(general)
                .page(self.services_page(cx)),
        )
    }
}

/// Providers each service can use: (kind, id, example URL, example model). Ids match
/// `Service::provider` and pick the API shape in `generate`.
const PROVIDERS: &[(&str, &str, &str, &str)] = &[
    ("speech", "openai", "https://api.openai.com/v1", "tts-1"),
    (
        "speech",
        "mimo",
        "https://api.xiaomimimo.com/v1",
        "mimo-v2.5-tts",
    ),
    (
        "speech",
        "doubao",
        "https://openspeech.bytedance.com/api/v3",
        "seed-tts-2.0",
    ),
    (
        "speech",
        "qwen",
        "https://dashscope.aliyuncs.com/api/v1",
        "qwen3-tts-flash",
    ),
    (
        "image",
        "openai",
        "https://api.openai.com/v1",
        "gpt-image-1",
    ),
    (
        "image",
        "ark",
        "https://ark.cn-beijing.volces.com/api/v3",
        "doubao-seedream-4-0-250828",
    ),
    (
        "image",
        "minimax",
        "https://api.minimaxi.com/v1",
        "image-01",
    ),
    ("video", "openai", "https://api.openai.com/v1", "sora-2"),
    (
        "video",
        "ark",
        "https://ark.cn-beijing.volces.com/api/v3",
        "doubao-seedance-2-0-260128",
    ),
    (
        "video",
        "minimax",
        "https://api.minimaxi.com/v1",
        "MiniMax-Hailuo-2.3",
    ),
    // A relay of the user's choosing: no address to suggest.
    ("video", "h3", "", "MiniMax-H3"),
];
