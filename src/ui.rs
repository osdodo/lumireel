use gpui_kit::component::{
    ActiveTheme as _, StyledExt as _,
    button::Button,
    dialog::{DialogAction, DialogClose, DialogFooter},
    h_flex,
};
use gpui_kit::*;
use rust_i18n::t;

/// The 36px strip every panel starts with: a quiet label on the left, actions on the right.
pub fn panel_header(title: impl Into<SharedString>, cx: &App) -> Div {
    let title: SharedString = title.into();
    h_flex()
        .h(px(36.))
        .flex_none()
        .px_3()
        .gap_2()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
}

/// Cancel and `confirm`: a plain `Dialog` draws no buttons of its own. `confirm` runs the dialog's `on_ok`.
pub fn dialog_footer(confirm: Button) -> DialogFooter {
    DialogFooter::new()
        .child(DialogClose::new().trigger(|button| button.label(t!("common.cancel").to_string())))
        .child(DialogAction::new().child(confirm))
}
