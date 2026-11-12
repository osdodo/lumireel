//! The settings dialog: interface language, the library location, and the services for
//! speech, image and video generation. Every edit takes effect at once: the form emits
//! [`SettingsChanged`] and the app applies and saves it.

use std::{collections::BTreeMap, path::PathBuf};

use gpui_kit::component::{
    AxisExt as _, IconName, Sizable as _,
    button::Button,
    group_box::GroupBoxVariant,
    input::{Input, InputEvent, InputState},
    setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings},
};
use gpui_kit::*;
use rust_i18n::t;

use crate::{
    config::{Account, Config, Service, Services},
    i18n, library,
};

pub struct SettingsForm {
    locale: SharedString,
    library_dir: PathBuf,
    speech: ServiceInputs,
    image: ServiceInputs,
    video: ServiceInputs,
    /// Each provider's URL and key as last filled in, so a switch restores them.
    accounts: BTreeMap<String, Account>,
}

/// Something in the form changed; read it back with [`SettingsForm::locale`] and
/// [`SettingsForm::services`].
pub struct SettingsChanged;

impl EventEmitter<SettingsChanged> for SettingsForm {}

/// One service's inputs, kept outside the settings state so the app can read them.
struct ServiceInputs {
    /// `speech`, `image` or `video`: picks the providers offered.
    kind: &'static str,
    provider: SharedString,
    /// The provider this replaced: on the next render its URL and key are put away and the
    /// new provider's filled in.
    switched_from: Option<SharedString>,
    url: Entity<InputState>,
    key: Entity<InputState>,
    model: Entity<InputState>,
}

impl ServiceInputs {
    fn new(
        window: &mut Window,
        cx: &mut Context<SettingsForm>,
        kind: &'static str,
        service: &Service,
    ) -> Self {
        let key_placeholder = t!("settings.key_placeholder").to_string();
        Self {
            kind,
            provider: service.provider().to_string().into(),
            switched_from: None,
            url: field(window, cx, &service.url, "", false),
            key: field(window, cx, &service.api_key, &key_placeholder, true),
            model: field(window, cx, &service.model, "", false),
        }
    }

    /// The picked provider's example URL and model.
    fn defaults(&self) -> (&'static str, &'static str) {
        PROVIDERS
            .iter()
            .find(|(kind, id, _, _)| *kind == self.kind && *id == self.provider.as_ref())
            .map_or(("", ""), |(_, _, url, model)| (*url, *model))
    }

    fn account(&self, cx: &App) -> Account {
        let service = self.service(cx);
        Account {
            url: service.url,
            api_key: service.api_key,
        }
    }

    fn service(&self, cx: &App) -> Service {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        Service {
            provider: self.provider.to_string(),
            url: value(&self.url),
            api_key: value(&self.key),
            model: value(&self.model),
        }
    }

    fn group(&self, title: String, form: Entity<SettingsForm>) -> SettingGroup {
        let kind = self.kind;
        let providers = PROVIDERS
            .iter()
            .filter(|(k, _, _, _)| *k == kind)
            .map(|(_, id, _, _)| {
                let id = SharedString::from(*id);
                let label = t!(format!("settings.provider_{id}")).to_string();
                (id, SharedString::from(label))
            })
            .collect();
        let provider = SettingField::dropdown(
            providers,
            {
                let form = form.clone();
                move |cx: &App| form.read(cx).inputs(kind).provider.clone()
            },
            move |provider, cx: &mut App| {
                form.update(cx, |form, cx| {
                    let inputs = form.inputs_mut(kind);
                    if inputs.provider != provider {
                        let old = std::mem::replace(&mut inputs.provider, provider);
                        inputs.switched_from.get_or_insert(old);
                        cx.emit(SettingsChanged);
                        cx.notify();
                    }
                })
            },
        );
        let (url, model) = self.defaults();
        SettingGroup::new()
            .title(title)
            .item(SettingItem::new(
                t!("settings.provider").to_string(),
                provider,
            ))
            .item(SettingItem::new(
                t!("settings.url").to_string(),
                input_field(self.url.clone(), false, Some(url.into())),
            ))
            .item(SettingItem::new(
                t!("settings.key").to_string(),
                input_field(self.key.clone(), true, None),
            ))
            .item(SettingItem::new(
                t!("settings.model").to_string(),
                input_field(self.model.clone(), false, Some(model.into())),
            ))
    }
}

impl SettingsForm {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, config: &Config) -> Self {
        let services = &config.services;
        let mut accounts = services.accounts.clone();
        for service in [&services.speech, &services.image, &services.video] {
            remember(
                &mut accounts,
                service.provider(),
                &service.url,
                &service.api_key,
            );
        }
        Self {
            locale: i18n::current().into(),
            library_dir: config.library_dir.clone(),
            speech: ServiceInputs::new(window, cx, "speech", &services.speech),
            image: ServiceInputs::new(window, cx, "image", &services.image),
            video: ServiceInputs::new(window, cx, "video", &services.video),
            accounts,
        }
    }

    /// After a provider switch: put the old provider's URL and key away, then fill in the new
    /// one's, from another service using it now, else as last saved, else its default URL.
    fn apply_switches(&mut self, window: &mut Window, cx: &mut App) {
        for kind in ["speech", "image", "video"] {
            let Some(old) = self.inputs_mut(kind).switched_from.take() else {
                continue;
            };
            let current = self.inputs(kind).account(cx);
            remember(&mut self.accounts, &old, &current.url, &current.api_key);
            let inputs = self.inputs(kind);
            let provider = inputs.provider.clone();
            let (default_url, model) = inputs.defaults();
            let live = ["speech", "image", "video"]
                .into_iter()
                .filter(|other| *other != kind)
                .map(|other| self.inputs(other))
                .find(|other| other.provider == provider)
                .map(|other| other.account(cx));
            let account = live
                .or_else(|| self.accounts.get(provider.as_ref()).cloned())
                .unwrap_or_else(|| Account {
                    url: default_url.into(),
                    api_key: String::new(),
                });
            let inputs = self.inputs(kind);
            for (input, value) in [
                (&inputs.url, account.url.as_str()),
                (&inputs.key, account.api_key.as_str()),
                (&inputs.model, model),
            ] {
                input.update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
    }

    fn inputs(&self, kind: &str) -> &ServiceInputs {
        match kind {
            "speech" => &self.speech,
            "image" => &self.image,
            _ => &self.video,
        }
    }

    fn inputs_mut(&mut self, kind: &str) -> &mut ServiceInputs {
        match kind {
            "speech" => &mut self.speech,
            "image" => &mut self.image,
            _ => &mut self.video,
        }
    }

    /// The language picked in the dialog.
    pub fn locale(&self) -> String {
        self.locale.to_string()
    }

    /// The services the dialog holds, ready to save.
    pub fn services(&self, cx: &App) -> Services {
        let mut accounts = self.accounts.clone();
        for inputs in [&self.speech, &self.image, &self.video] {
            let account = inputs.account(cx);
            remember(
                &mut accounts,
                &inputs.provider,
                &account.url,
                &account.api_key,
            );
        }
        Services {
            speech: self.speech.service(cx),
            image: self.image.service(cx),
            video: self.video.service(cx),
            accounts,
        }
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
            .group(
                SettingGroup::new()
                    .title(t!("settings.interface").to_string())
                    .item(
                        SettingItem::new(t!("settings.language").to_string(), language)
                            .description(t!("settings.language_hint").to_string()),
                    ),
            )
            .group(
                SettingGroup::new()
                    .title(t!("settings.library").to_string())
                    .item(
                        SettingItem::new(t!("settings.library_location").to_string(), reveal)
                            .description(self.library_dir.display().to_string()),
                    ),
            )
    }

    fn services_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let form = cx.entity();
        SettingPage::new(t!("settings.services").to_string())
            .icon(gpui_kit::assets::IconName::Sparkles)
            .description(t!("settings.hint").to_string())
            .resettable(false)
            .group(
                self.speech
                    .group(t!("settings.speech").to_string(), form.clone()),
            )
            .group(
                self.image
                    .group(t!("settings.image").to_string(), form.clone()),
            )
            .group(self.video.group(t!("settings.video").to_string(), form))
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

/// Keep a provider's URL and key; a blank one is not worth keeping.
fn remember(accounts: &mut BTreeMap<String, Account>, provider: &str, url: &str, key: &str) {
    if !key.is_empty() {
        accounts.insert(
            provider.to_string(),
            Account {
                url: url.to_string(),
                api_key: key.to_string(),
            },
        );
    }
}

/// A text field seeded with `value`; secrets start masked. Each edit reports a change.
fn field(
    window: &mut Window,
    cx: &mut Context<SettingsForm>,
    value: &str,
    placeholder: &str,
    secret: bool,
) -> Entity<InputState> {
    let state = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder.to_string())
            .default_value(value.to_string())
            .masked(secret)
    });
    cx.subscribe(&state, |_, _, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            cx.emit(SettingsChanged);
        }
    })
    .detach();
    state
}

/// An input as a settings row field. `placeholder` follows the picked provider.
fn input_field(
    state: Entity<InputState>,
    secret: bool,
    placeholder: Option<SharedString>,
) -> SettingField<SharedString> {
    SettingField::render(move |options, window, cx| {
        if let Some(placeholder) = &placeholder
            && state.read(cx).presentation().placeholder() != placeholder
        {
            state.update(cx, |input, cx| {
                input.set_placeholder(placeholder.clone(), window, cx)
            });
        }
        let input = Input::new(&state)
            .small()
            .bordered(false)
            .focus_bordered(false);
        let input = if secret { input.mask_toggle() } else { input };
        if options.layout().is_horizontal() {
            input.w(px(300.))
        } else {
            input.w_full()
        }
    })
}

impl Render for SettingsForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_switches(window, cx);
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
