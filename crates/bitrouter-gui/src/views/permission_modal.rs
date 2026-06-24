//! [`render_permission_modal`] — overlay for pending permission requests.
//!
//! When the focused session has `pending: Some(Permission)`, renders a centered
//! dialog overlay above the center pane showing the permission summary, optional
//! diff, and allow-once / allow-always / deny buttons.

use bitrouter_gui_core::{
    protocol::{Command, PermissionOutcome, SessionId, Target},
    state::Permission,
};
use gpui::{
    div, prelude::FluentBuilder as _, ClickEvent, Entity, IntoElement, ParentElement, Styled,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex, v_flex, ActiveTheme, StyledExt,
};

use crate::app_model::AppModel;

/// Render a full-size scrim + centered modal when `pending` is `Some`.
///
/// This is a free function so it can be composed inside any parent view's
/// render output without requiring a separate entity.
///
/// The caller is responsible for placing the returned element as an overlay
/// (e.g., using `.absolute()` or a z-ordered child at the end of the stack).
pub fn render_permission_modal(
    pending: &Permission,
    session_id: &SessionId,
    model: Entity<AppModel>,
    cx: &mut gpui::Context<impl gpui::Render>,
) -> impl IntoElement {
    let id = session_id.clone();
    let request_id = pending.request_id.clone();
    let summary = pending.summary.clone();
    let diff = pending.diff.clone();

    // Build button handlers — each captures the data it needs.
    let id_allow_once = id.clone();
    let req_allow_once = request_id.clone();
    let model_allow_once = model.clone();

    let id_allow_always = id.clone();
    let req_allow_always = request_id.clone();
    let model_allow_always = model.clone();

    let id_deny = id.clone();
    let req_deny = request_id.clone();
    let model_deny = model.clone();

    let allow_once_btn = Button::new(gpui::ElementId::Name("perm-allow-once".into()))
        .label("[y] allow once")
        .on_click(move |_: &ClickEvent, _window, cx| {
            model_allow_once.update(cx, |m, cx| {
                m.dispatch(Command::ResolvePending {
                    target: Target::Session {
                        id: id_allow_once.clone(),
                    },
                    request_id: Some(req_allow_once.clone()),
                    outcome: PermissionOutcome::AllowOnce,
                });
                m.resolve_pending(&id_allow_once);
                cx.notify();
            });
        });

    let allow_always_btn = Button::new(gpui::ElementId::Name("perm-allow-always".into()))
        .label("[a] allow always")
        .on_click(move |_: &ClickEvent, _window, cx| {
            model_allow_always.update(cx, |m, cx| {
                m.dispatch(Command::ResolvePending {
                    target: Target::Session {
                        id: id_allow_always.clone(),
                    },
                    request_id: Some(req_allow_always.clone()),
                    outcome: PermissionOutcome::AllowAlways,
                });
                m.resolve_pending(&id_allow_always);
                cx.notify();
            });
        });

    let deny_btn = Button::new(gpui::ElementId::Name("perm-deny".into()))
        .ghost()
        .label("[n] deny")
        .on_click(move |_: &ClickEvent, _window, cx| {
            model_deny.update(cx, |m, cx| {
                m.dispatch(Command::ResolvePending {
                    target: Target::Session {
                        id: id_deny.clone(),
                    },
                    request_id: Some(req_deny.clone()),
                    outcome: PermissionOutcome::Deny,
                });
                m.resolve_pending(&id_deny);
                cx.notify();
            });
        });

    // Optional diff block.
    let diff_element = diff.map(|d| {
        div()
            .w_full()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .text_xs()
            .font_family("monospace")
            .text_color(cx.theme().muted_foreground)
            .child(d)
    });

    // Modal card: v_flex with summary, optional diff, buttons.
    let modal_card = v_flex()
        .w(gpui::px(480.0))
        .gap_y_3()
        .p_4()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .shadow_md()
        .child(
            div()
                .text_sm()
                .font_semibold()
                .text_color(cx.theme().foreground)
                .child("Permission required"),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(summary),
        )
        .when_some(diff_element, |el, block| el.child(block))
        .child(
            h_flex()
                .w_full()
                .gap_x_2()
                .justify_end()
                .child(deny_btn)
                .child(allow_once_btn)
                .child(allow_always_btn),
        );

    // Scrim + centered modal overlay.
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(cx.theme().background.opacity(0.75))
        .child(modal_card)
}
