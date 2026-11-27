//! The settings dialog: interface language, the library location, and the services for
//! speech, image and video generation. Every edit takes effect at once: the form emits
//! [`SettingsChanged`] and the app applies and saves it.

use std::path::PathBuf;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    group_box::GroupBoxVariant,
    h_flex,
    input::{Input, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
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
    ui::dialog_footer,
};

pub struct SettingsForm {
    locale: SharedString,
    library_dir: PathBuf,
    services: Services,
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
        let groups = KINDS.map(|(kind, icon)| {
            let listed = self.listed(kind);
            let count = listed.len();
            let header = SettingItem::render({
                let form = form.clone();
                move |_, _, cx| {
                    let form = form.clone();
                    h_flex()
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
                        .child(div().flex_1())
                        .child(
                            Button::new(SharedString::from(format!("add-{kind}")))
                                .small()
                                .outline()
                                .icon(Lucide::Plus)
                                .label(t!("settings.add").to_string())
                                .on_click(move |_, window, cx| {
                                    open_editor(
                                        form.clone(),
                                        kind,
                                        None,
                                        Service::default(),
                                        window,
                                        cx,
                                    )
                                }),
                        )
                }
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
        SettingPage::new(t!("settings.services").to_string())
            .icon(Lucide::Sparkles)
            .description(t!("settings.hint").to_string())
            .resettable(false)
            .groups(groups)
    }
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

/// The add/edit form, shown in its own dialog over the settings.
struct ServiceEditor {
    kind: &'static str,
    provider: SharedString,
    url: Entity<InputState>,
    key: Entity<InputState>,
    model: Entity<InputState>,
}

impl ServiceEditor {
    fn new(
        kind: &'static str,
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
}

impl Render for ServiceEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let kind = self.kind;
        let current = self.provider.clone();
        let label = t!(format!("settings.provider_{current}")).to_string();
        let provider = Button::new("provider")
            .w_full()
            .outline()
            .label(label)
            .dropdown_caret(true)
            .dropdown_menu(move |menu, _, _| {
                PROVIDERS
                    .iter()
                    .filter(|(k, ..)| *k == kind)
                    .fold(menu, |menu, (_, id, _, _)| {
                        let this = this.clone();
                        menu.item(
                            PopupMenuItem::new(t!(format!("settings.provider_{id}")).to_string())
                                .checked(current.as_ref() == *id)
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |editor, cx| {
                                        editor.provider = (*id).into();
                                        editor.sync_placeholders(window, cx);
                                        cx.notify();
                                    })
                                }),
                        )
                    })
            });
        let row = |label: SharedString, field: AnyElement| {
            v_flex()
                .gap_1()
                .child(div().text_sm().child(label))
                .child(field)
        };
        v_flex()
            .gap_3()
            .child(row(
                t!("settings.provider").to_string().into(),
                provider.into_any_element(),
            ))
            .child(row(
                t!("settings.url").to_string().into(),
                Input::new(&self.url).into_any_element(),
            ))
            .child(row(
                t!("settings.key").to_string().into(),
                Input::new(&self.key).mask_toggle().into_any_element(),
            ))
            .child(row(
                t!("settings.model").to_string().into(),
                Input::new(&self.model).into_any_element(),
            ))
    }
}

/// Add (`slot` is `None`) or edit a service in a dialog; saving writes it back to `form`.
fn open_editor(
    form: Entity<SettingsForm>,
    kind: &'static str,
    slot: Option<Slot>,
    service: Service,
    window: &mut Window,
    cx: &mut App,
) {
    let editor = cx.new(|cx| ServiceEditor::new(kind, &service, window, cx));
    let title = t!(if slot.is_some() {
        "settings.edit"
    } else {
        "settings.add"
    })
    .to_string();
    window.open_dialog(cx, move |dialog, _, _| {
        let editor = editor.clone();
        let form = form.clone();
        dialog
            .title(title.clone())
            .w(px(460.))
            .child(editor.clone())
            .footer(dialog_footer(
                Button::new("save-service")
                    .primary()
                    .label(t!("common.save").to_string()),
            ))
            .on_ok(move |_, _, cx| {
                let service = editor.read(cx).service(cx);
                form.update(cx, |form, cx| form.save(kind, slot, service, cx));
                true
            })
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let general = self.general_page(cx);
        div().h(px(480.)).child(
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
];
