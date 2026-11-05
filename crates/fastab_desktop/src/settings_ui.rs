//! Native GPUI settings window. Replaces the dashboard WKWebView.

#![allow(unexpected_cfgs)]

use std::path::PathBuf;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    AnchoredPositionMode, App, Bounds, ClipboardItem, Context, Entity, FocusHandle, MouseButton, ScrollHandle,
    SharedString, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions, anchored, deferred, div, point, px, rgb,
    size,
};
use tracing::error;

use crate::EventLoopProxy;
use crate::event::Event;
use crate::permissions::{self, PermId, PermReady, PermissionSnapshot};
#[cfg(target_os = "macos")]
use crate::webview::menu::{Minimize, Zoom};

pub const SETTINGS_WINDOW_TITLE: &str = "Settings";

const SIDEBAR_W: f32 = 196.0;
const SETTINGS_TITLEBAR_H: f32 = 44.0;
const SETTINGS_TRAFFIC_LIGHT_X: f32 = 12.0;
const SETTINGS_TRAFFIC_LIGHT_Y: f32 = 18.0;
// `traffic_light_position` is the close button's left edge. Reserve the full
// close/minimize/zoom cluster, then leave a visible gap before the title.
const SETTINGS_TRAFFIC_LIGHT_CLUSTER_W: f32 = 54.0;
const SETTINGS_TITLE_GAP: f32 = 16.0;
const SETTINGS_TITLE_LEFT: f32 = SETTINGS_TRAFFIC_LIGHT_X + SETTINGS_TRAFFIC_LIGHT_CLUSTER_W + SETTINGS_TITLE_GAP;
// GPUI's traffic-light Y is the 14px button's top edge, so its center is at
// 25px. The 44px title row centers at 22px; shift only the text down 3px so
// both share the same horizontal centerline without moving the navigation.
const SETTINGS_TITLE_Y_OFFSET: f32 = 3.0;
const WIN_W: f32 = 820.0;
const WIN_H: f32 = 640.0;
// Temporarily hide the AI settings surface without changing saved AI consent
// or configuration. Avoid constructing its credential/save tasks while hidden.
const AI_SETTINGS_VISIBLE: bool = false;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Appearance,
    Behavior,
    Ai,
    About,
}

mod ai;
mod input;
mod theme;
#[cfg(test)]
use theme::ThemeAppearance;
use theme::{Chrome, THEMES};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeTarget {
    Interface,
    Completion,
}

impl ThemeTarget {
    fn key(self) -> &'static str {
        match self {
            Self::Interface => "dashboard.theme",
            Self::Completion => "autocomplete.theme",
        }
    }
}

#[derive(Clone, Copy)]
struct OpenThemeMenu {
    target: ThemeTarget,
    highlighted: usize,
}

#[derive(Clone)]
struct ThemeControls {
    menu: Option<OpenThemeMenu>,
    focus: [FocusHandle; 2],
    scroll: ScrollHandle,
}

impl ThemeControls {
    fn new(cx: &mut App) -> Self {
        Self {
            menu: None,
            focus: [cx.focus_handle().tab_stop(true), cx.focus_handle().tab_stop(true)],
            scroll: ScrollHandle::new(),
        }
    }

    fn open(&mut self, target: ThemeTarget, current: &str) {
        let highlighted = THEMES
            .iter()
            .position(|theme| theme.id.eq_ignore_ascii_case(current))
            .unwrap_or(0);
        self.menu = Some(OpenThemeMenu { target, highlighted });
        self.scroll.scroll_to_item(highlighted);
    }
}

fn shows_permission_gate(gate: &PermissionSnapshot) -> bool {
    !gate.still_checking() && !gate.all_ready()
}

const FONTS: &[&str] = &["Menlo", "Monaco", "Hack", "SF Mono", "JetBrains Mono"];

pub struct SettingsWindow {
    section: Section,
    proxy: EventLoopProxy,
    gate: PermissionSnapshot,
    repairing: Option<PermId>,
    permission_merge: PermissionMergeState,
    copied_doctor: bool,
    theme_controls: ThemeControls,
    ai: Option<Entity<ai::AiSettings>>,
}

pub type SettingsHandle = WindowHandle<SettingsWindow>;

impl SettingsWindow {
    fn clear_ai_draft(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(ai) = &self.ai {
            ai.update(cx, |ai, cx| ai.clear_draft(cx));
        }
    }

    fn zh() -> bool {
        locale_is_zh()
    }

    fn set_bool(&mut self, key: &str, value: bool, cx: &mut Context<'_, Self>) {
        if let Err(err) = fastab_settings::settings::set_value(key, value) {
            error!(%err, key, "Failed to write setting");
            return;
        }
        self.proxy.send_event(Event::ReloadCredentials).ok();
        cx.notify();
    }

    fn set_string(&mut self, key: &str, value: impl Into<serde_json::Value>, cx: &mut Context<'_, Self>) {
        if let Err(err) = fastab_settings::settings::set_value(key, value) {
            error!(%err, key, "Failed to write setting");
            return;
        }
        self.proxy.send_event(Event::ReloadCredentials).ok();
        cx.notify();
    }

    fn set_int(&mut self, key: &str, value: i64, cx: &mut Context<'_, Self>) {
        self.set_string(key, value, cx);
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let chrome = Chrome::current();
        let entity = cx.entity();
        let zh = Self::zh();
        let section = self.section;
        let theme_controls = self.theme_controls.clone();
        let root = div()
            .id("ec-settings")
            .flex()
            .flex_row()
            .w_full()
            .h_full()
            .overflow_hidden()
            .bg(rgb(chrome.bg))
            .text_color(rgb(chrome.text))
            .text_size(px(13.))
            .font_family(".AppleSystemUIFont")
            .on_key_down(|event, window, cx| {
                if event.keystroke.key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                }
            });
        #[cfg(target_os = "macos")]
        let root = root
            .on_action(|_: &Minimize, window, _| window.minimize_window())
            .on_action(|_: &Zoom, window, _| window.zoom_window());

        if self.gate.still_checking() {
            return root.child(permission_checking_page(zh, chrome, entity));
        }
        if shows_permission_gate(&self.gate) {
            return root.child(permission_gate_page(
                zh,
                chrome,
                self.gate.clone(),
                self.repairing,
                entity,
            ));
        }

        root.child(sidebar(section, zh, chrome, entity.clone())).child(
            div()
                .id(("ec-settings-main", section as u32))
                .flex_1()
                .min_w(px(0.))
                .min_h(px(0.))
                .flex()
                .flex_col()
                .pt(px(48.))
                .px(px(28.))
                .pb(px(24.))
                .overflow_y_scroll()
                .child(
                    div()
                        .w_full()
                        .max_w(px(720.))
                        .min_w(px(0.))
                        .flex_none()
                        .mx_auto()
                        .child(page_header(section, zh, chrome))
                        .child(match section {
                            Section::Appearance => {
                                appearance_page(zh, chrome, entity.clone(), theme_controls).into_any_element()
                            },
                            Section::Behavior => {
                                behavior_page(zh, chrome, entity, self.gate.input_method, self.repairing)
                                    .into_any_element()
                            },
                            Section::Ai => self
                                .ai
                                .as_ref()
                                .map_or_else(|| div().into_any_element(), |ai| ai.clone().into_any_element()),
                            Section::About => about_page(zh, chrome, entity, self.copied_doctor).into_any_element(),
                        }),
                ),
        )
    }
}

fn page_header(section: Section, zh: bool, chrome: Chrome) -> impl IntoElement {
    let (title, description) = match (section, zh) {
        (Section::Appearance, true) => ("外观", "分别设置界面与终端补全提示的外观"),
        (Section::Appearance, false) => ("Appearance", "Personalize settings and your terminal completions"),
        (Section::Behavior, true) => ("行为", "调整启动方式、补全习惯与键盘操作"),
        (Section::Behavior, false) => ("Behavior", "Choose how FasTab starts and responds as you type"),
        (Section::Ai, true) => ("AI", "配置 Jev 推荐、服务商和密钥"),
        (Section::Ai, false) => ("AI", "Jev recommendations, providers, and API keys"),
        (Section::About, true) => ("关于", "版本、更新与支持"),
        (Section::About, false) => ("About", "Version, updates, and support"),
    };
    div()
        .mb(px(24.))
        .child(
            div()
                .text_size(px(24.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            div()
                .mt(px(6.))
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(rgb(chrome.muted))
                .child(description),
        )
}

fn sidebar(section: Section, zh: bool, chrome: Chrome, entity: Entity<SettingsWindow>) -> impl IntoElement {
    let items = [
        (Section::Appearance, if zh { "外观" } else { "Appearance" }),
        (Section::Behavior, if zh { "行为" } else { "Behavior" }),
        (Section::Ai, "AI"),
        (Section::About, if zh { "关于" } else { "About" }),
    ];
    let mut nav = div().flex().flex_col().mt(px(14.)).px(px(12.)).gap(px(5.));
    for (id, label) in items {
        if id == Section::Ai && !AI_SETTINGS_VISIBLE {
            continue;
        }
        let active = section == id;
        let entity = entity.clone();
        nav = nav.child(
            div()
                .id(("ec-settings-nav", id as u32))
                .h(px(38.))
                .flex_none()
                .px(px(10.))
                .gap(px(10.))
                .rounded(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .cursor_pointer()
                .bg(rgb(if active { chrome.selection } else { chrome.sidebar }))
                .text_color(rgb(if active { chrome.accent } else { chrome.text }))
                .hover(|style| style.bg(rgb(chrome.selection)))
                .font_weight(if active {
                    gpui::FontWeight::MEDIUM
                } else {
                    gpui::FontWeight::NORMAL
                })
                .child(
                    div()
                        .w(px(22.))
                        .h(px(22.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(16.))
                        .child(match id {
                            Section::Appearance => "◐",
                            Section::Behavior => "⌘",
                            Section::Ai => "✦",
                            Section::About => "ⓘ",
                        }),
                )
                .child(label.to_string())
                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                    entity.update(cx, |this, cx| {
                        if this.section != id {
                            this.clear_ai_draft(cx);
                        }
                        this.section = id;
                        this.theme_controls.menu = None;
                        cx.notify();
                    });
                }),
        );
    }
    div()
        .id("ec-settings-sidebar")
        .w(px(SIDEBAR_W))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(chrome.sidebar))
        .border_r_1()
        .border_color(rgb(chrome.sidebar_border))
        .child(
            div()
                .h(px(SETTINGS_TITLEBAR_H))
                .flex_none()
                .flex()
                .items_center()
                .pl(px(SETTINGS_TITLE_LEFT))
                .pr(px(16.))
                .text_size(px(11.))
                .text_color(rgb(chrome.muted))
                .child(
                    div()
                        .relative()
                        .top(px(SETTINGS_TITLE_Y_OFFSET))
                        .child(if zh { "设置" } else { "Settings" }.to_string()),
                ),
        )
        .child(nav)
        .child(
            div()
                .mt_auto()
                .px(px(22.))
                .py(px(20.))
                .text_size(px(11.))
                .text_color(rgb(chrome.muted))
                .child("FasTab")
                .child(div().mt(px(3.)).child(env!("CARGO_PKG_VERSION"))),
        )
}

fn card(title: &str, chrome: Chrome, children: impl IntoElement) -> impl IntoElement {
    div()
        .w_full()
        .min_w(px(0.))
        .flex_none()
        .mb(px(22.))
        .child(
            div()
                .mb(px(9.))
                .text_size(px(13.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(chrome.text))
                .child(title.to_string()),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .rounded(px(12.))
                .overflow_hidden()
                .bg(rgb(chrome.card))
                .border_1()
                .border_color(rgb(chrome.separator))
                .child(children),
        )
}

fn stacked_row(
    label: &str,
    description: Option<&str>,
    chrome: Chrome,
    last: bool,
    control: impl IntoElement,
) -> impl IntoElement {
    let mut body = div().w_full().min_w(px(0.)).px(px(16.)).py(px(14.));
    if !last {
        body = body.border_b_1().border_color(rgb(chrome.separator));
    }
    body.child(div().text_color(rgb(chrome.text)).child(label.to_string()))
        .when_some(description.map(str::to_string), |this, desc| {
            this.child(
                div()
                    .mt(px(3.))
                    .text_size(px(12.))
                    .text_color(rgb(chrome.muted))
                    .child(desc),
            )
        })
        .child(div().mt(px(10.)).w_full().min_w(px(0.)).child(control))
}

fn row(
    label: &str,
    description: Option<&str>,
    chrome: Chrome,
    last: bool,
    control: impl IntoElement,
) -> impl IntoElement {
    let mut body = div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .px(px(16.))
        .py(px(14.));
    if !last {
        body = body.border_b_1().border_color(rgb(chrome.separator));
    }
    let mut left = div().flex().flex_col().flex_1().min_w(px(0.));
    left = left.child(div().text_color(rgb(chrome.text)).child(label.to_string()));
    if let Some(description) = description {
        left = left.child(
            div()
                .mt(px(3.))
                .text_size(px(12.))
                .line_height(px(18.))
                .whitespace_normal()
                .text_color(rgb(chrome.muted))
                .child(description.to_string()),
        );
    }
    body.child(left).child(div().flex_none().child(control))
}

fn toggle(id: SharedString, checked: bool, chrome: Chrome, on_click: impl Fn(&mut App) + 'static) -> impl IntoElement {
    div()
        .id(id)
        .w(px(38.))
        .flex_none()
        .h(px(22.))
        .rounded(px(11.))
        .bg(rgb(if checked { chrome.accent } else { chrome.track_off }))
        .flex()
        .flex_row()
        .items_center()
        .px(px(2.))
        .cursor_pointer()
        .child(
            div()
                .w(px(18.))
                .flex_none()
                .h(px(18.))
                .rounded(px(10.))
                .bg(rgb(0xffffff))
                .when(checked, |this| this.ml(px(16.))),
        )
        .on_mouse_down(MouseButton::Left, move |_e, _w, cx| on_click(cx))
}

fn stepper(
    id: &'static str,
    value: i64,
    min: i64,
    max: i64,
    step: i64,
    chrome: Chrome,
    on_set: impl Fn(i64, &mut App) + 'static,
) -> impl IntoElement {
    let on_set = std::rc::Rc::new(on_set);
    let dec = {
        let on_set = on_set.clone();
        move |_e: &gpui::MouseDownEvent, _w: &mut gpui::Window, cx: &mut App| {
            on_set((value - step).clamp(min, max), cx);
        }
    };
    let inc = {
        let on_set = on_set;
        move |_e: &gpui::MouseDownEvent, _w: &mut gpui::Window, cx: &mut App| {
            on_set((value + step).clamp(min, max), cx);
        }
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .id((id, 0u32))
                .w(px(22.))
                .h(px(22.))
                .rounded(px(6.))
                .bg(rgb(chrome.separator))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child("−")
                .on_mouse_down(MouseButton::Left, dec),
        )
        .child(div().min_w(px(36.)).flex().justify_center().child(value.to_string()))
        .child(
            div()
                .id((id, 1u32))
                .w(px(22.))
                .h(px(22.))
                .rounded(px(6.))
                .bg(rgb(chrome.separator))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child("+")
                .on_mouse_down(MouseButton::Left, inc),
        )
}

fn select_chips(
    id_prefix: &'static str,
    options: &[(&'static str, &'static str)],
    current: &str,
    chrome: Chrome,
    on_pick: impl Fn(&'static str, &mut App) + 'static,
) -> impl IntoElement {
    let on_pick = std::rc::Rc::new(on_pick);
    let mut row = div().min_w(px(0.)).flex().flex_row().flex_wrap().gap(px(6.));
    for (i, (label, value)) in options.iter().enumerate() {
        let selected = *value == current;
        let value = *value;
        let on_pick = on_pick.clone();
        row = row.child(
            div()
                .id((id_prefix, i as u32))
                .flex_none()
                .whitespace_nowrap()
                .px(px(10.))
                .py(px(5.))
                .rounded(px(7.))
                .border_1()
                .border_color(rgb(if selected { chrome.accent } else { chrome.separator }))
                .cursor_pointer()
                .bg(rgb(if selected { chrome.selection } else { chrome.card }))
                .text_color(rgb(if selected { chrome.accent } else { chrome.text }))
                .child((*label).to_string())
                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| on_pick(value, cx)),
        );
    }
    row
}

fn theme_label(current: &str, zh: bool) -> String {
    THEMES
        .iter()
        .find(|theme| theme.id.eq_ignore_ascii_case(current))
        .map_or(current, |theme| if zh { theme.label_zh } else { theme.label_en })
        .to_string()
}

fn theme_color_dot(color: u32, border: u32) -> impl IntoElement {
    div()
        .size(px(12.))
        .flex_none()
        .rounded(px(6.))
        .border_1()
        .border_color(rgb(border))
        .bg(rgb(color))
}

fn completion_theme_preview(current: &str, zh: bool) -> impl IntoElement {
    let palette = crate::overlay::overlay_theme_by_name(current);
    let mut list = div()
        .w_full()
        .max_w(px(360.))
        .rounded(px(9.))
        .border_1()
        .border_color(rgb(palette.border))
        .bg(rgb(palette.background))
        .text_color(rgb(palette.text))
        .font_family("Menlo")
        .text_size(px(12.))
        .overflow_hidden()
        .child(
            div()
                .px(px(12.))
                .py(px(9.))
                .text_color(rgb(palette.muted))
                .border_b_1()
                .border_color(rgb(palette.border))
                .child("$ git ch"),
        );
    for (index, (command, description)) in [
        ("checkout", if zh { "切换分支" } else { "Switch branches" }),
        ("cherry-pick", if zh { "应用指定提交" } else { "Apply a commit" }),
        ("check-ref-format", if zh { "检查引用名称" } else { "Validate a ref" }),
    ]
    .iter()
    .enumerate()
    {
        let selected = index == 0;
        list = list.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .px(px(12.))
                .py(px(8.))
                .bg(rgb(if selected { palette.selected } else { palette.background }))
                .text_color(rgb(if selected { palette.selected_text } else { palette.text }))
                .child((*command).to_string())
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(if selected { palette.selected_text } else { palette.muted }))
                        .child((*description).to_string()),
                ),
        );
    }
    list
}

fn theme_selector(
    target: ThemeTarget,
    current: &str,
    zh: bool,
    chrome: Chrome,
    controls: &ThemeControls,
    entity: Entity<SettingsWindow>,
) -> impl IntoElement {
    let opened = controls.menu.is_some_and(|menu| menu.target == target);
    let focus = controls.focus[target as usize].clone();
    let click_entity = entity.clone();
    let key_entity = entity.clone();
    let click_current = current.to_string();
    let key_current = current.to_string();
    let palette = crate::overlay::overlay_theme_by_name(current);
    let mut root = div().relative().w(px(220.)).flex_none().child(
        div()
            .id(("ec-theme-select", target as u32))
            .track_focus(&focus)
            .tab_stop(true)
            .w_full()
            .h(px(34.))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(px(7.))
            .border_1()
            .border_color(rgb(if opened { chrome.accent } else { chrome.separator }))
            .bg(rgb(chrome.card))
            .cursor_pointer()
            .hover(|style| style.border_color(rgb(chrome.accent)))
            .focus(|style| style.border_color(rgb(chrome.accent)))
            .child(theme_color_dot(palette.background, palette.border))
            .child(div().flex_1().min_w(px(0.)).truncate().child(theme_label(current, zh)))
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(chrome.muted))
                    .child(if opened { "⌃" } else { "⌄" }),
            )
            .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                focus.focus(window);
                click_entity.update(cx, |this, cx| {
                    if opened {
                        this.theme_controls.menu = None;
                    } else {
                        this.theme_controls.open(target, &click_current);
                    }
                    cx.notify();
                });
                cx.stop_propagation();
            })
            .on_key_down(move |event, window, cx| {
                let key = event.keystroke.key.as_str();
                if !matches!(key, "up" | "down" | "enter" | "space" | "escape" | "tab") {
                    return;
                }
                key_entity.update(cx, |this, cx| {
                    let menu = &mut this.theme_controls.menu;
                    match (key, *menu) {
                        ("escape" | "tab", _) => *menu = None,
                        ("enter" | "space", Some(menu)) if menu.target == target => {
                            let id = THEMES[menu.highlighted].id;
                            this.theme_controls.menu = None;
                            this.set_string(target.key(), id, cx);
                        },
                        ("up" | "down", Some(menu)) if menu.target == target => {
                            let next = if key == "up" {
                                menu.highlighted.saturating_sub(1)
                            } else {
                                (menu.highlighted + 1).min(THEMES.len() - 1)
                            };
                            this.theme_controls.menu = Some(OpenThemeMenu {
                                target,
                                highlighted: next,
                            });
                            this.theme_controls.scroll.scroll_to_item(next);
                        },
                        ("up" | "down" | "enter" | "space", _) => this.theme_controls.open(target, &key_current),
                        _ => {},
                    }
                    cx.notify();
                });
                if key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                }
                cx.stop_propagation();
            }),
    );
    if let Some(open) = controls.menu.filter(|menu| menu.target == target) {
        let outside_entity = entity.clone();
        let mut menu = div()
            .id(("ec-theme-menu", target as u32))
            .w(px(240.))
            .max_h(px(330.))
            .p(px(5.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(9.))
            .border_1()
            .border_color(rgb(chrome.separator))
            .bg(rgb(chrome.card))
            .shadow_lg()
            .occlude()
            .overflow_y_scroll()
            .track_scroll(&controls.scroll)
            .on_mouse_down_out(move |_event, _window, cx| {
                outside_entity.update(cx, |this, cx| {
                    if this.theme_controls.menu.is_some_and(|menu| menu.target == target) {
                        this.theme_controls.menu = None;
                        cx.notify();
                    }
                });
            });
        for (index, theme) in THEMES.iter().enumerate() {
            let entity = entity.clone();
            let id = theme.id;
            let selected = current.eq_ignore_ascii_case(id);
            let highlighted = index == open.highlighted;
            let palette = crate::overlay::overlay_theme_by_name(id);
            menu = menu.child(
                div()
                    .id(("ec-theme-option", index as u32))
                    .h(px(29.))
                    .flex_none()
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .bg(rgb(if highlighted { chrome.selection } else { chrome.card }))
                    .text_color(rgb(chrome.text))
                    .hover(|style| style.bg(rgb(chrome.selection)))
                    .child(theme_color_dot(palette.background, palette.border))
                    .child(div().flex_1().child(if zh { theme.label_zh } else { theme.label_en }))
                    .child(
                        div()
                            .w(px(12.))
                            .flex_none()
                            .text_color(rgb(chrome.accent))
                            .child(if selected { "✓" } else { "" }),
                    )
                    .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
                        entity.update(cx, |this, cx| {
                            this.theme_controls.menu = None;
                            this.set_string(target.key(), id, cx);
                        });
                        cx.stop_propagation();
                    }),
            );
        }
        root = root.child(
            deferred(
                anchored()
                    .position_mode(AnchoredPositionMode::Local)
                    .position(point(px(0.), px(39.)))
                    .snap_to_window_with_margin(px(12.))
                    .child(menu),
            )
            .with_priority(1),
        );
    }
    root
}

fn language_options(zh: bool) -> &'static [(&'static str, &'static str)] {
    if zh {
        &[("跟随系统", "system"), ("English", "en"), ("简体中文", "zh-CN")]
    } else {
        &[("Follow System", "system"), ("English", "en"), ("简体中文", "zh-CN")]
    }
}

fn language_select(
    id_prefix: &'static str,
    zh: bool,
    chrome: Chrome,
    entity: Entity<SettingsWindow>,
) -> impl IntoElement {
    let lang = fastab_settings::settings::get_string_or("dashboard.language", "system".into());
    // select_chips captures `current` for the life of the returned element.
    // A local String cannot leave this function, so the stored value is mapped
    // onto one of the static option ids.
    let current = match lang.as_str() {
        "en" => "en",
        "zh-CN" | "zh" => "zh-CN",
        _ => "system",
    };
    select_chips(id_prefix, language_options(zh), current, chrome, move |value, cx| {
        entity.update(cx, |this, cx| this.set_string("dashboard.language", value, cx));
    })
}

/// The permission pages render before the sidebar, so this is the only language
/// control a first-run window can reach. The chips sit under the titlebar band:
/// that strip drags the window, and clicks there do not reach this view.
fn permission_language_bar(
    id_prefix: &'static str,
    zh: bool,
    chrome: Chrome,
    entity: Entity<SettingsWindow>,
) -> impl IntoElement {
    div()
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .child(div().h(px(SETTINGS_TITLEBAR_H)).flex_none())
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .gap(px(12.))
                .px(px(28.))
                .pt(px(8.))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(rgb(chrome.muted))
                        .child(if zh { "显示语言" } else { "Display Language" }.to_string()),
                )
                .child(language_select(id_prefix, zh, chrome, entity)),
        )
}

fn appearance_page(
    zh: bool,
    chrome: Chrome,
    entity: Entity<SettingsWindow>,
    controls: ThemeControls,
) -> impl IntoElement {
    let interface_theme = fastab_settings::settings::get_string_or("dashboard.theme", "system".into());
    let theme = fastab_settings::settings::get_string_or("autocomplete.theme", "github-dark".into());
    let font = fastab_settings::settings::get_string_or("autocomplete.fontFamily", String::new());
    let font_size = fastab_settings::settings::get_int_or("autocomplete.fontSize", 13);
    let width = fastab_settings::settings::get_int_or("autocomplete.width", 300);
    let height = fastab_settings::settings::get_int_or("autocomplete.height", 140);
    let overflow = fastab_settings::settings::get_string_or("autocomplete.overflow", "scroll".into());

    let mut font_options: Vec<(&str, String)> = FONTS.iter().map(|name| (*name, (*name).to_string())).collect();
    if !font.is_empty() && !FONTS.contains(&font.as_str()) {
        font_options.insert(0, ("Custom", font.clone()));
    }
    font_options.insert(0, (if zh { "系统默认" } else { "System default" }, String::new()));

    let mut font_row = div().min_w(px(0.)).flex().flex_row().flex_wrap().gap(px(6.));
    for (i, (label, value)) in font_options.iter().enumerate() {
        let selected = value == &font || (value.is_empty() && font.is_empty());
        let value = value.clone();
        let entity = entity.clone();
        font_row = font_row.child(
            div()
                .id(("ec-font", i as u32))
                .flex_none()
                .whitespace_nowrap()
                .px(px(10.))
                .py(px(5.))
                .rounded(px(7.))
                .border_1()
                .border_color(rgb(if selected { chrome.accent } else { chrome.separator }))
                .cursor_pointer()
                .bg(rgb(if selected { chrome.selection } else { chrome.card }))
                .text_color(rgb(if selected { chrome.accent } else { chrome.text }))
                .child((*label).to_string())
                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                    let payload = if value.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(value.clone())
                    };
                    entity.update(cx, |this, cx| this.set_string("autocomplete.fontFamily", payload, cx));
                }),
        );
    }

    let size_entity = entity.clone();
    let width_entity = entity.clone();
    let height_entity = entity.clone();
    let overflow_entity = entity.clone();

    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .child(card(
            if zh { "界面" } else { "Interface" },
            chrome,
            div()
                .child(row(
                    if zh { "显示语言" } else { "Display Language" },
                    None,
                    chrome,
                    false,
                    language_select("ec-lang", zh, chrome, entity.clone()),
                ))
                .child(row(
                    if zh { "界面主题" } else { "Interface Theme" },
                    None,
                    chrome,
                    true,
                    theme_selector(
                        ThemeTarget::Interface,
                        &interface_theme,
                        zh,
                        chrome,
                        &controls,
                        entity.clone(),
                    ),
                )),
        ))
        .child(card(
            if zh { "补全提示" } else { "Completions" },
            chrome,
            div()
                .child(row(
                    if zh { "提示主题" } else { "Completion Theme" },
                    Some(if zh {
                        "仅改变终端中的补全提示外观"
                    } else {
                        "Appearance of terminal suggestions"
                    }),
                    chrome,
                    false,
                    theme_selector(ThemeTarget::Completion, &theme, zh, chrome, &controls, entity.clone()),
                ))
                .child(
                    div()
                        .p(px(16.))
                        .child(
                            div()
                                .mb(px(12.))
                                .text_size(px(11.))
                                .text_color(rgb(chrome.muted))
                                .child(if zh { "预览" } else { "Preview" }),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_center()
                                .child(completion_theme_preview(&theme, zh)),
                        ),
                ),
        ))
        .child(card(
            if zh { "字体" } else { "Typography" },
            chrome,
            div()
                .child(stacked_row(
                    if zh { "字体名称" } else { "Font Family" },
                    Some(if zh {
                        "补全弹窗使用的字体"
                    } else {
                        "Font used in the autocomplete popup"
                    }),
                    chrome,
                    false,
                    font_row,
                ))
                .child(row(
                    if zh { "字体大小" } else { "Font Size" },
                    None,
                    chrome,
                    true,
                    stepper(
                        "font-size",
                        font_size.clamp(10, 24),
                        10,
                        24,
                        1,
                        chrome,
                        move |value, cx| {
                            size_entity.update(cx, |this, cx| this.set_int("autocomplete.fontSize", value, cx));
                        },
                    ),
                )),
        ))
        .child(card(
            if zh { "尺寸" } else { "Dimensions" },
            chrome,
            div()
                .child(row(
                    if zh { "最大宽度" } else { "Max Width" },
                    None,
                    chrome,
                    false,
                    stepper(
                        "max-width",
                        width.clamp(150, 800),
                        150,
                        800,
                        10,
                        chrome,
                        move |value, cx| {
                            width_entity.update(cx, |this, cx| this.set_int("autocomplete.width", value, cx));
                        },
                    ),
                ))
                .child(row(
                    if zh { "最大高度" } else { "Max Height" },
                    None,
                    chrome,
                    false,
                    stepper(
                        "max-height",
                        height.clamp(80, 600),
                        80,
                        600,
                        10,
                        chrome,
                        move |value, cx| {
                            height_entity.update(cx, |this, cx| this.set_int("autocomplete.height", value, cx));
                        },
                    ),
                ))
                .child(stacked_row(
                    if zh { "超长文本" } else { "Long Text" },
                    Some(if zh {
                        "超长候选名称使用省略号，或只滚动选中的名称；参数提示始终静态省略，展开详情可查看完整内容"
                    } else {
                        "Ellipsize long names or scroll the selected name. Argument hints stay still; expand details to see them in full"
                    }),
                    chrome,
                    true,
                    select_chips(
                        "ec-overflow",
                        if zh {
                            &[("滚动", "scroll"), ("省略", "ellipsis")]
                        } else {
                            &[("Scroll", "scroll"), ("Ellipsis", "ellipsis")]
                        },
                        overflow.as_str(),
                        chrome,
                        move |value, cx| {
                            overflow_entity.update(cx, |this, cx| {
                                this.set_string("autocomplete.overflow", value, cx);
                            });
                        },
                    ),
                )),
        ))
}

fn behavior_page(
    zh: bool,
    chrome: Chrome,
    entity: Entity<SettingsWindow>,
    ime: PermReady,
    repairing: Option<PermId>,
) -> impl IntoElement {
    let launch = fastab_settings::settings::get_bool_or("app.launchOnStartup", false);
    let silent = fastab_settings::settings::get_bool_or("app.silentLaunch", false);
    let show_menubar_icon = !fastab_settings::settings::get_bool_or("app.hideMenubarIcon", false);
    let only_tab = fastab_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false);
    let fuzzy = fastab_settings::settings::get_bool_or("autocomplete.fuzzySearch", true);
    let first_token = fastab_settings::settings::get_bool_or("autocomplete.firstTokenCompletion", false);
    let sort = fastab_settings::settings::get_string_or("autocomplete.sortMethod", "default".into());
    let history_nav = fastab_settings::settings::get_bool_or("autocomplete.navigateToHistory", false);
    let trailing = fastab_settings::settings::get_bool_or("autocomplete.insertSpaceAutomatically", true);
    let hide_auto = fastab_settings::settings::get_bool_or("autocomplete.hideAutoExecuteSuggestion", false);
    let show_auto = !hide_auto;
    let exec_space = fastab_settings::settings::get_bool_or("autocomplete.immediatelyExecuteAfterSpace", false);
    let dangerous = fastab_settings::settings::get_bool_or("autocomplete.immediatelyRunDangerousCommands", false);
    let history_mode = fastab_settings::settings::get_string_or("beta.history.mode", "show".into());
    let merge = fastab_settings::settings::get_bool_or("beta.history.allShells", false);

    let e = |entity: &Entity<SettingsWindow>| entity.clone();

    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .child(card(
            if zh { "启动与触发" } else { "Startup & Trigger" },
            chrome,
            div()
                .child(bool_row(
                    if zh { "登录时启动" } else { "Launch at Login" },
                    None,
                    launch,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| {
                        if let Err(err) = fastab_integrations::login_item::set_enabled(value) {
                            error!(%err, "Failed to update login item");
                        }
                        this.set_bool("app.launchOnStartup", value, cx);
                    },
                ))
                .child(bool_row(
                    if zh { "静默启动" } else { "Silent Launch" },
                    Some(if zh {
                        "启动时不打开本设置窗口，直接在后台运行"
                    } else {
                        "Start in the background without opening this settings window"
                    }),
                    silent,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("app.silentLaunch", value, cx),
                ))
                .child(bool_row(
                    if zh {
                        "显示菜单栏图标"
                    } else {
                        "Show Menu Bar Icon"
                    },
                    Some(if zh {
                        "隐藏后，再次启动 FasTab 可打开设置"
                    } else {
                        "When hidden, launch FasTab again to open settings"
                    }),
                    show_menubar_icon,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("app.hideMenubarIcon", !value, cx),
                ))
                .child(bool_row(
                    if zh {
                        "按 Tab 后显示建议"
                    } else {
                        "Show Suggestions After Tab"
                    },
                    None,
                    only_tab,
                    chrome,
                    true,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.onlyShowOnTab", value, cx),
                )),
        ))
        .child(card(
            if zh { "补全建议" } else { "Suggestions" },
            chrome,
            div()
                .child(bool_row(
                    if zh { "模糊匹配" } else { "Fuzzy Matching" },
                    Some(if zh {
                        "匹配相近字符序列，而非仅匹配前缀"
                    } else {
                        "Match close character sequences instead of exact prefixes"
                    }),
                    fuzzy,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.fuzzySearch", value, cx),
                ))
                .child(bool_row(
                    if zh {
                        "输入时提示命令名"
                    } else {
                        "Suggest Commands While Typing"
                    },
                    None,
                    first_token,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.firstTokenCompletion", value, cx),
                ))
                .child(stacked_row(
                    if zh { "排序方式" } else { "Sort Order" },
                    None,
                    chrome,
                    true,
                    {
                        let entity = e(&entity);
                        select_chips(
                            "ec-sort",
                            if zh {
                                &[("按相关性", "default"), ("按字母顺序", "alphabetical")]
                            } else {
                                &[("By Relevance", "default"), ("Alphabetical", "alphabetical")]
                            },
                            sort.as_str(),
                            chrome,
                            move |value, cx| {
                                entity.update(cx, |this, cx| this.set_string("autocomplete.sortMethod", value, cx));
                            },
                        )
                    },
                )),
        ))
        .child(card(
            if zh { "键盘与插入" } else { "Keyboard & Insertion" },
            chrome,
            div()
                .child(bool_row(
                    if zh {
                        "使用上方向键浏览历史"
                    } else {
                        "Use Up Arrow for History"
                    },
                    None,
                    history_nav,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.navigateToHistory", value, cx),
                ))
                .child(bool_row(
                    if zh {
                        "自动插入尾随空格"
                    } else {
                        "Insert Trailing Space"
                    },
                    None,
                    trailing,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.insertSpaceAutomatically", value, cx),
                ))
                .child(bool_row(
                    if zh {
                        "显示立即执行"
                    } else {
                        "Show Immediately Execute"
                    },
                    None,
                    show_auto,
                    chrome,
                    !show_auto,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.hideAutoExecuteSuggestion", !value, cx),
                ))
                .when(show_auto, {
                    let entity = e(&entity);
                    let entity2 = e(&entity);
                    move |this| {
                        this.child(bool_row(
                            if zh {
                                "空格结尾时置顶"
                            } else {
                                "Pin After a Trailing Space"
                            },
                            None,
                            exec_space,
                            chrome,
                            false,
                            entity,
                            |this, value, cx| this.set_bool("autocomplete.immediatelyExecuteAfterSpace", value, cx),
                        ))
                        .child(bool_row(
                            if zh {
                                "包含危险命令"
                            } else {
                                "Include Dangerous Commands"
                            },
                            None,
                            dangerous,
                            chrome,
                            true,
                            entity2,
                            |this, value, cx| this.set_bool("autocomplete.immediatelyRunDangerousCommands", value, cx),
                        ))
                    }
                }),
        ))
        .child(optional_input_method_card(zh, chrome, e(&entity), ime, repairing))
        .child(card(
            if zh { "历史记录" } else { "History" },
            chrome,
            div()
                .child(stacked_row(
                    if zh { "历史记录模式" } else { "History Mode" },
                    None,
                    chrome,
                    false,
                    {
                        let entity = e(&entity);
                        select_chips(
                            "ec-history",
                            if zh {
                                &[
                                    ("与补全建议一起显示", "show"),
                                    ("仅显示历史记录", "history_only"),
                                    ("关闭", "off"),
                                ]
                            } else {
                                &[
                                    ("Show with completions", "show"),
                                    ("History only", "history_only"),
                                    ("Off", "off"),
                                ]
                            },
                            history_mode.as_str(),
                            chrome,
                            move |value, cx| {
                                entity.update(cx, |this, cx| this.set_string("beta.history.mode", value, cx));
                            },
                        )
                    },
                ))
                .child(bool_row(
                    if zh { "合并所有 Shell" } else { "Merge All Shells" },
                    Some(if zh {
                        "包含所有 Shell（bash、zsh、fish）的历史记录"
                    } else {
                        "Include history from all shells (bash, zsh, fish)"
                    }),
                    merge,
                    chrome,
                    true,
                    e(&entity),
                    |this, value, cx| this.set_bool("beta.history.allShells", value, cx),
                )),
        ))
}

fn optional_input_method_card(
    zh: bool,
    chrome: Chrome,
    entity: Entity<SettingsWindow>,
    ime: PermReady,
    repairing: Option<PermId>,
) -> impl IntoElement {
    let (title, description, repair_label) = perm_label(PermId::InputMethod, zh);
    let busy = repairing == Some(PermId::InputMethod);
    let can_repair = matches!(ime, PermReady::Missing | PermReady::Error);
    let enabled = can_repair && !busy;
    card(
        title,
        chrome,
        div()
            .px(px(16.))
            .py(px(14.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(chrome.muted))
                            .child(description.to_string()),
                    )
                    .child(
                        div()
                            .mt(px(6.))
                            .text_size(px(12.))
                            .text_color(rgb(if ime == PermReady::Ready {
                                0x30d158
                            } else {
                                chrome.muted
                            }))
                            .child(perm_status_label(PermId::InputMethod, ime, zh).to_string()),
                    ),
            )
            .child(
                div()
                    .id("ec-ime-install")
                    .min_w(px(130.))
                    .px(px(12.))
                    .py(px(6.))
                    .rounded(px(9.))
                    .bg(rgb(if enabled { chrome.accent } else { chrome.separator }))
                    .text_color(rgb(if enabled { chrome.accent_text } else { chrome.muted }))
                    .when(enabled, |this| this.cursor_pointer())
                    .child(if busy {
                        if zh { "处理中…" } else { "Working..." }.to_string()
                    } else if ime == PermReady::Ready {
                        if zh { "已启用" } else { "Enabled" }.to_string()
                    } else {
                        repair_label.to_string()
                    })
                    .when(enabled, |this| {
                        this.on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                            entity.update(cx, |this, cx| {
                                this.repairing = Some(PermId::InputMethod);
                                this.permission_merge.active_repair_generation =
                                    Some(permissions::spawn_repair(&this.proxy, PermId::InputMethod));
                                cx.notify();
                            });
                        })
                    }),
            ),
    )
}

fn bool_row(
    label: &str,
    description: Option<&str>,
    checked: bool,
    chrome: Chrome,
    last: bool,
    entity: Entity<SettingsWindow>,
    on_change: impl Fn(&mut SettingsWindow, bool, &mut Context<'_, SettingsWindow>) + 'static,
) -> impl IntoElement {
    let next = !checked;
    row(
        label,
        description,
        chrome,
        last,
        toggle(
            SharedString::from(format!("toggle-{label}")),
            checked,
            chrome,
            move |cx| {
                entity.update(cx, |this, cx| on_change(this, next, cx));
            },
        ),
    )
}

fn about_page(zh: bool, chrome: Chrome, entity: Entity<SettingsWindow>, copied_doctor: bool) -> impl IntoElement {
    let version = env!("CARGO_PKG_VERSION");
    let auto_updates = !fastab_settings::settings::get_bool_or("app.disableAutoupdates", false);
    let entity_copy = entity.clone();
    let entity_updates = entity.clone();
    let entity_auto = entity.clone();

    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .child(card(
            "FasTab",
            chrome,
            div()
                .px(px(16.))
                .py(px(16.))
                .child(
                    div()
                        .text_size(px(20.))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("FasTab"),
                )
                .child(div().mt(px(4.)).text_color(rgb(chrome.muted)).child(if zh {
                    "适用于 macOS 的终端自动补全".to_string()
                } else {
                    "Terminal autocomplete for macOS".to_string()
                }))
                .child(
                    div()
                        .mt(px(10.))
                        .flex()
                        .flex_row()
                        .gap(px(8.))
                        .child(pill(
                            format!("{} {version}", if zh { "版本" } else { "Version" }),
                            chrome,
                            move |cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(format!("FasTab {version}")));
                                entity_copy.update(cx, |_, cx| cx.notify());
                            },
                        ))
                        .child(pill(
                            if zh {
                                "检查更新".to_string()
                            } else {
                                "Check for Updates".to_string()
                            },
                            chrome,
                            move |cx| {
                                entity_updates.read(cx).proxy.send_event(Event::CheckForUpdates).ok();
                            },
                        )),
                ),
        ))
        .child(card(
            if zh { "更新" } else { "Updates" },
            chrome,
            bool_row(
                if zh {
                    "自动检查更新"
                } else {
                    "Check for Updates Automatically"
                },
                None,
                auto_updates,
                chrome,
                true,
                entity_auto,
                |this, value, cx| this.set_bool("app.disableAutoupdates", !value, cx),
            ),
        ))
        .child(card(
            if zh { "故障排查" } else { "Troubleshooting" },
            chrome,
            div()
                .px(px(16.))
                .py(px(14.))
                .child(div().child(if zh {
                    "在终端中运行内置诊断：".to_string()
                } else {
                    "Run the built-in diagnostic in your terminal:".to_string()
                }))
                .child({
                    let entity_cmd = entity.clone();
                    let entity_btn = entity.clone();
                    div()
                        .mt(px(8.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("ec-doctor-cmd")
                                .flex_1()
                                .px(px(12.))
                                .py(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(chrome.sidebar))
                                .font_family("Menlo")
                                .cursor_pointer()
                                .child(format!("{} doctor", fastab_util::CLI_BINARY_NAME))
                                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                                    copy_doctor(&entity_cmd, cx);
                                }),
                        )
                        .child(
                            div()
                                .id("ec-doctor-copy")
                                .px(px(12.))
                                .py(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(if copied_doctor { chrome.accent } else { chrome.separator }))
                                .text_color(rgb(if copied_doctor { chrome.accent_text } else { chrome.text }))
                                .cursor_pointer()
                                .child(if copied_doctor {
                                    if zh { "已复制" } else { "Copied" }.to_string()
                                } else if zh {
                                    "复制".to_string()
                                } else {
                                    "Copy".to_string()
                                })
                                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                                    copy_doctor(&entity_btn, cx);
                                }),
                        )
                }),
        ))
        .child(card(
            if zh { "链接" } else { "Links" },
            chrome,
            div()
                .child(link_row(
                    if zh { "发行说明" } else { "Release Notes" },
                    fastab_util::consts::url::RELEASE_NOTES,
                    chrome,
                    false,
                ))
                .child(link_row(
                    if zh { "报告问题" } else { "Report an Issue" },
                    fastab_util::consts::url::ISSUE_TRACKER,
                    chrome,
                    false,
                ))
                .child(link_row(
                    if zh { "隐私政策" } else { "Privacy Policy" },
                    "https://fastab.app/privacy-policy",
                    chrome,
                    false,
                ))
                .child(link_row(
                    if zh { "开源许可证" } else { "Open Source Licenses" },
                    "file",
                    chrome,
                    true,
                )),
        ))
        .child(card(
            if zh { "许可与致谢" } else { "License & acknowledgments" },
            chrome,
            div()
                .px(px(16.))
                .py(px(14.))
                .text_size(px(13.))
                .text_color(rgb(chrome.muted))
                .child(if zh {
                    "MIT 许可。Fork 自 Easy Complete，基于 Amazon Q Developer CLI 与 Fig。感谢 Easy Complete、Amazon 与 Fig 的贡献者。"
                        .to_string()
                } else {
                    "MIT License. Forked from Easy Complete, based on Amazon Q Developer CLI and Fig. Thanks to the Easy Complete, Amazon, and Fig contributors."
                        .to_string()
                })
                .child(link_row(
                    "Easy Complete",
                    "https://github.com/chen86860/easy-complete",
                    chrome,
                    false,
                ))
                .child(link_row(
                    "Amazon Q Developer CLI",
                    "https://github.com/aws/amazon-q-developer-cli",
                    chrome,
                    false,
                ))
                .child(link_row(
                    "Fig",
                    "https://github.com/withfig/autocomplete",
                    chrome,
                    true,
                )),
        ))
}

fn copy_doctor(entity: &Entity<SettingsWindow>, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(format!(
        "{} doctor",
        fastab_util::CLI_BINARY_NAME
    )));
    entity.update(cx, |this, cx| {
        this.copied_doctor = true;
        cx.notify();
    });
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(2)).await;
        let _ = cx.update(|cx| {
            entity.update(cx, |this, cx| {
                this.copied_doctor = false;
                cx.notify();
            });
        });
    })
    .detach();
}

fn accessibility_hint(zh: bool) -> &'static str {
    #[cfg(target_os = "macos")]
    {
        macos_utils::accessibility::accessibility_permission_hint(
            macos_utils::os::OperatingSystemVersion::get().major(),
            zh,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        if zh {
            "用于读取当前聚焦的终端窗口并定位补全弹窗。点击后打开系统设置；列表里失效的旧条目会先被移除，再把 FasTab 拖进旁边的列表。FasTab 有自己的 TCC 身份，可以和 Easy Complete 并排安装。"
        } else {
            "Required to read the focused terminal window and position completions. Click to open System Settings. A stale list row is removed first, then drag FasTab into the list beside the card. FasTab has its own TCC identity and can sit beside Easy Complete."
        }
    }
}

fn perm_label(id: PermId, zh: bool) -> (&'static str, &'static str, &'static str) {
    match (id, zh) {
        (PermId::Accessibility, true) => ("辅助功能权限", accessibility_hint(true), "授予辅助功能权限"),
        (PermId::Accessibility, false) => (
            "Accessibility Permission",
            accessibility_hint(false),
            "Grant Accessibility",
        ),
        (PermId::Shell, true) => (
            "Shell 集成",
            "向 .zshrc / .bashrc 注入钩子，使 FasTab 能够跟踪 Shell 状态。",
            "安装 Shell 钩子",
        ),
        (PermId::Shell, false) => (
            "Shell Integration",
            "Injects hooks into .zshrc / .bashrc so FasTab can track your shell state.",
            "Install Shell Hooks",
        ),
        (PermId::InputMethod, true) => (
            "输入法集成",
            "可选：用于 Kitty、Alacritty、Zed、Ghostty、WezTerm 和 Otty 的光标跟踪。启用后随 FasTab 启动自动恢复。",
            "启用输入法",
        ),
        (PermId::InputMethod, false) => (
            "Input Method",
            "Optional cursor tracking in Kitty, Alacritty, Zed, Ghostty, WezTerm, and Otty. Restored automatically when FasTab starts.",
            "Enable Input Method",
        ),
    }
}

fn perm_status_label(id: PermId, state: PermReady, zh: bool) -> &'static str {
    if id == PermId::InputMethod {
        return match (state, zh) {
            (PermReady::Checking, true) => "检查中",
            (PermReady::Checking, false) => "Checking",
            (PermReady::Ready, true) => "已就绪",
            (PermReady::Ready, false) => "Ready",
            (PermReady::Missing, true) => "未就绪",
            (PermReady::Missing, false) => "Not ready",
            (PermReady::Error, true) => "检查失败",
            (PermReady::Error, false) => "Check failed",
        };
    }
    match (state, zh) {
        (PermReady::Checking, true) => "检查中",
        (PermReady::Checking, false) => "Checking",
        (PermReady::Ready, true) => "已就绪",
        (PermReady::Ready, false) => "Ready",
        (PermReady::Missing, true) => "需要设置",
        (PermReady::Missing, false) => "Needs setup",
        (PermReady::Error, true) => "需要处理",
        (PermReady::Error, false) => "Needs attention",
    }
}

fn permission_checking_page(zh: bool, chrome: Chrome, entity: Entity<SettingsWindow>) -> impl IntoElement {
    div()
        .id("ec-permission-checking")
        .flex()
        .flex_1()
        .flex_col()
        .child(permission_language_bar("ec-perm-check-lang", zh, chrome, entity))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .px(px(40.))
                .child(div().text_size(px(15.)).text_color(rgb(chrome.muted)).child(if zh {
                    "正在检查权限…"
                } else {
                    "Checking permissions…"
                })),
        )
}

fn permission_gate_page(
    zh: bool,
    chrome: Chrome,
    gate: PermissionSnapshot,
    repairing: Option<PermId>,
    entity: Entity<SettingsWindow>,
) -> impl IntoElement {
    let rows = [
        (PermId::Accessibility, gate.accessibility),
        (PermId::Shell, gate.shell),
        (PermId::InputMethod, gate.input_method),
    ];
    let busy = repairing.is_some();
    let ax_ready = gate.accessibility == PermReady::Ready;

    let mut list = div()
        .rounded(px(14.))
        .bg(rgb(chrome.card))
        .border_1()
        .border_color(rgb(chrome.separator));
    for (i, (id, state)) in rows.iter().enumerate() {
        let id = *id;
        let state = *state;
        let (title, description, repair_label) = perm_label(id, zh);
        let can_repair = matches!(state, PermReady::Missing | PermReady::Error);
        let blocked = id == PermId::Shell && !ax_ready;
        let this_busy = repairing == Some(id);
        let enabled = can_repair && !busy && !blocked;
        let last = i + 1 == rows.len();
        let entity_row = entity.clone();
        let mut row = div()
            .id(("ec-perm-row", i as u32))
            .px(px(18.))
            .py(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(16.));
        if !last {
            row = row.border_b_1().border_color(rgb(chrome.separator));
        }
        list = list.child(
            row.child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_size(px(14.))
                                    .child(title.to_string()),
                            )
                            .when(id == PermId::InputMethod, |this| {
                                this.child(
                                    div()
                                        .px(px(8.))
                                        .py(px(3.))
                                        .rounded(px(999.))
                                        .bg(rgb(chrome.separator))
                                        .text_color(rgb(chrome.muted))
                                        .text_size(px(12.))
                                        .child(if zh { "非必选" } else { "Optional" }.to_string()),
                                )
                            })
                            .child(
                                div()
                                    .px(px(8.))
                                    .py(px(3.))
                                    .rounded(px(999.))
                                    .bg(rgb(if state == PermReady::Ready {
                                        0x1c3d2a
                                    } else if id == PermId::InputMethod {
                                        chrome.separator
                                    } else {
                                        0x3d2e16
                                    }))
                                    .text_color(rgb(if state == PermReady::Ready {
                                        0x30d158
                                    } else if id == PermId::InputMethod {
                                        chrome.muted
                                    } else {
                                        0xff9f0a
                                    }))
                                    .text_size(px(12.))
                                    .child(perm_status_label(id, state, zh).to_string()),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(4.))
                            .text_size(px(12.))
                            .text_color(rgb(chrome.muted))
                            .child(description.to_string()),
                    )
                    .when(blocked, |this| {
                        this.child(
                            div()
                                .mt(px(4.))
                                .text_size(px(12.))
                                .text_color(rgb(chrome.muted))
                                .child(if zh {
                                    "请先授予辅助功能权限，再执行此步骤。"
                                } else {
                                    "Grant Accessibility first to enable this step."
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .id(("ec-perm-repair", i as u32))
                    .min_w(px(130.))
                    .px(px(12.))
                    .py(px(6.))
                    .rounded(px(9.))
                    .bg(rgb(if enabled { chrome.accent } else { chrome.separator }))
                    .text_color(rgb(if enabled { chrome.accent_text } else { chrome.muted }))
                    .cursor_pointer()
                    .child(if this_busy {
                        if zh { "处理中…" } else { "Working..." }.to_string()
                    } else {
                        repair_label.to_string()
                    })
                    .when(enabled, |this| {
                        this.on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                            entity_row.update(cx, |this, cx| {
                                this.repairing = Some(id);
                                // repair() owns starting the Accessibility guide. A second
                                // queued start here could reopen a card the user just closed.
                                this.permission_merge.active_repair_generation =
                                    Some(permissions::spawn_repair(&this.proxy, id));
                                this.gate.error = None;
                                cx.notify();
                            });
                        })
                    }),
            ),
        );
    }

    let entity_refresh = entity.clone();
    let entity_all = entity.clone();

    div()
        .id("ec-permission-gate")
        .flex()
        .flex_1()
        .flex_col()
        .child(permission_language_bar(
            "ec-perm-lang",
            zh,
            chrome,
            entity.clone(),
        ))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .px(px(40.))
                .child(
                    div()
                        .w(px(640.))
                        .child(
                            div()
                                .mb(px(16.))
                                .child(
                                    div()
                                        .text_size(px(22.))
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .child(if zh { "完成设置" } else { "Finish Setup" }.to_string()),
                                )
                                .child(
                                    div()
                                        .mt(px(6.))
                                        .text_size(px(13.))
                                        .text_color(rgb(chrome.muted))
                                        .child(if zh {
                                            "使用设置前需要辅助功能和 Shell 集成。输入法为非必选，仅部分终端需要。"
                                        } else {
                                            "Accessibility and Shell integration are required before settings can be used. The input method is optional."
                                        }),
                                ),
                        )
                        .child(list)
                        .when_some(gate.error.clone(), |this, err| {
                            this.child(
                                div()
                                    .mt(px(8.))
                                    .text_size(px(12.))
                                    .text_color(rgb(0xff453a))
                                    .child(err),
                            )
                        })
                        .child(
                            div()
                                .mt(px(16.))
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .id("ec-perm-refresh")
                                        .px(px(12.))
                                        .py(px(6.))
                                        .rounded(px(9.))
                                        .bg(rgb(chrome.separator))
                                        .cursor_pointer()
                                        .child(if zh {
                                            "重新检查".to_string()
                                        } else {
                                            "Check Again".to_string()
                                        })
                                        .when(!busy, |this| {
                                            this.on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                                                entity_refresh.update(cx, |this, cx| {
                                                    if this.repairing.is_some() {
                                                        return;
                                                    }
                                                    this.gate = PermissionSnapshot::checking();
                                                    cx.notify();
                                                    permissions::spawn_check(&this.proxy);
                                                });
                                            })
                                        }),
                                )
                                .child(
                                    div()
                                        .id("ec-perm-fix-all")
                                        .px(px(12.))
                                        .py(px(6.))
                                        .rounded(px(9.))
                                        .bg(rgb(if busy { chrome.separator } else { chrome.accent }))
                                        .text_color(rgb(if busy { chrome.muted } else { chrome.accent_text }))
                                        .cursor_pointer()
                                        .child(if repairing.is_some() {
                                            if zh { "处理中…" } else { "Working..." }.to_string()
                                        } else if zh {
                                            "全部修复".to_string()
                                        } else {
                                            "Fix All".to_string()
                                        })
                                        .when(!busy, |this| {
                                            this.on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                                                entity_all.update(cx, |this, cx| {
                                                    this.repairing = Some(PermId::Accessibility);
                                                    this.permission_merge.active_repair_generation =
                                                        Some(permissions::spawn_repair_all(&this.proxy));
                                                    cx.notify();
                                                });
                                            })
                                        }),
                                ),
                        ),
                ),
        )
}

fn pill(label: String, chrome: Chrome, on_click: impl Fn(&mut App) + 'static) -> impl IntoElement {
    div()
        .id(SharedString::from(label.clone()))
        .px(px(10.))
        .py(px(5.))
        .rounded(px(999.))
        .bg(rgb(chrome.separator))
        .cursor_pointer()
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_e, _w, cx| on_click(cx))
}

fn link_row(label: &str, url: &str, chrome: Chrome, last: bool) -> impl IntoElement {
    let url = url.to_string();
    row(
        label,
        None,
        chrome,
        last,
        div()
            .id(SharedString::from(format!("link-{label}")))
            .text_color(rgb(chrome.accent))
            .cursor_pointer()
            .child("↗")
            .on_mouse_down(MouseButton::Left, move |_e, _w, _cx| {
                if url == "file" {
                    if let Some(path) = notices_path() {
                        let _ = std::process::Command::new("open").arg(path).spawn();
                    }
                } else if let Err(err) = fastab_util::open_url(&url) {
                    error!(%err, "Failed to open url");
                }
            }),
    )
}

fn notices_path() -> Option<PathBuf> {
    fastab_util::directories::resources_path()
        .ok()
        .map(|dir| dir.join("Licenses/THIRD_PARTY_NOTICES.txt"))
}

pub(crate) fn locale_is_zh() -> bool {
    let pref = fastab_settings::settings::get_string_or("dashboard.language", "system".into());
    match pref.as_str() {
        "zh-CN" | "zh" => true,
        "en" => false,
        _ => system_locale_is_zh(),
    }
}

fn system_locale_is_zh() -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        use cocoa::base::{id, nil};
        use cocoa::foundation::NSString;
        use objc::{class, msg_send, sel, sel_impl};
        let langs: id = msg_send![class!(NSLocale), preferredLanguages];
        if langs == nil {
            return false;
        }
        let first: id = msg_send![langs, firstObject];
        if first == nil {
            return false;
        }
        let prefix = NSString::alloc(nil).init_str("zh");
        let matched: bool = msg_send![first, hasPrefix: prefix];
        matched
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn activate_app() {
    #[cfg(target_os = "macos")]
    unsafe {
        use cocoa::base::{YES, id};
        use objc::{class, msg_send, sel, sel_impl};
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, activateIgnoringOtherApps: YES];
    }
}

pub fn open_settings_window(cx: &mut App, proxy: EventLoopProxy) -> anyhow::Result<SettingsHandle> {
    let bounds = Bounds::centered(None, size(px(WIN_W), px(WIN_H)), cx);
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SETTINGS_WINDOW_TITLE.into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(SETTINGS_TRAFFIC_LIGHT_X), px(SETTINGS_TRAFFIC_LIGHT_Y))),
            }),
            focus: true,
            show: true,
            kind: gpui::WindowKind::Normal,
            is_movable: true,
            is_resizable: true,
            window_min_size: Some(size(px(WIN_W), px(520.))),
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(SETTINGS_WINDOW_TITLE);
            let entity = cx.new(|cx| SettingsWindow {
                section: Section::Appearance,
                proxy: proxy.clone(),
                gate: PermissionSnapshot::checking(),
                repairing: None,
                permission_merge: PermissionMergeState::default(),
                copied_doctor: false,
                theme_controls: ThemeControls::new(cx),
                ai: AI_SETTINGS_VISIBLE.then(|| cx.new(|cx| ai::AiSettings::new(proxy.clone(), cx))),
            });
            let weak = entity.downgrade();
            window.on_window_should_close(cx, move |_window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.clear_ai_draft(cx);
                });
                // GPUI removes this exact window after returning true. The
                // host observes that removal; a queued unqualified Close could
                // otherwise close a new window opened before it is dispatched.
                true
            });
            entity
        },
    )?;
    handle
        .update(cx, |_view, window, _cx| {
            window.activate_window();
        })
        .ok();
    activate_app();
    start_permission_poller(handle, cx);
    Ok(handle)
}

fn start_permission_poller(handle: SettingsHandle, cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let keep_going = match cx.update(|cx| {
                handle.update(cx, |this, _window, _cx| {
                    if this.gate.all_ready() {
                        return false;
                    }
                    if this.repairing.is_some() {
                        // A repair owns the next permission snapshot. Starting
                        // another check here could advance its generation and
                        // make the repair-complete result look stale.
                        return true;
                    }
                    let ax_now_ready = !permissions::accessibility_is_missing();
                    let ax_marked_ready = this.gate.accessibility == PermReady::Ready;
                    if ax_now_ready != ax_marked_ready {
                        this.proxy.send_event(Event::ReloadAccessibility).ok();
                        permissions::spawn_check(&this.proxy);
                    }
                    true
                })
            }) {
                Ok(Ok(keep)) => keep,
                _ => false,
            };
            if !keep_going {
                break;
            }
        }
    })
    .detach();
}

#[derive(Default)]
struct PermissionMergeState {
    /// The repair task currently owned by this settings window. Completion
    /// snapshots from another window must not clear that task, even when all
    /// permission values happen to be identical.
    active_repair_generation: Option<u64>,
    /// The newest check generation accepted by this view.
    latest_generation: Option<u64>,
    /// Whether the newest accepted snapshot already includes the optional IME
    /// result. A required snapshot arriving after its enriched companion is a
    /// duplicate/out-of-order event and must not regress the IME state.
    latest_is_enriched: bool,
    /// A repair-complete required snapshot whose enriched companion is still
    /// allowed to fill in the optional IME state.
    pending_repair_generation: Option<u64>,
}

fn is_enriched_snapshot(snapshot: &PermissionSnapshot) -> bool {
    snapshot.input_method != PermReady::Checking
}

fn merge_permission_snapshot(
    gate: &mut PermissionSnapshot,
    repairing: &mut Option<PermId>,
    merge_state: &mut PermissionMergeState,
    snapshot: PermissionSnapshot,
) {
    let generation = snapshot.generation;
    let enriched = is_enriched_snapshot(&snapshot);

    if snapshot.completes_repair {
        // A repair task is detached from the settings window, so its result
        // can arrive after that window has been closed and a new one opened.
        // Only the generation registered by this window may release its
        // repair barrier. This also rejects a completion with no active repair
        // (for example, a result left over from the previous window).
        if merge_state.active_repair_generation != Some(generation) {
            return;
        }
        *gate = snapshot;
        *repairing = None;
        merge_state.active_repair_generation = None;
        merge_state.latest_generation = Some(generation);
        merge_state.latest_is_enriched = enriched;
        merge_state.pending_repair_generation = (!enriched).then_some(generation);
        return;
    }

    if merge_state.latest_generation.is_some_and(|latest| generation < latest) {
        return;
    }
    if merge_state.latest_generation == Some(generation) && merge_state.latest_is_enriched && !enriched {
        return;
    }

    // `publish_check` emits required fields first and an enriched IME result
    // second. Once the repair-complete required snapshot landed, only its own
    // enriched companion may update the gate; a late snapshot from an older
    // check must not put stale AX/Shell/IME values back into the UI. A newer
    // required generation supersedes that pending companion and is accepted.
    if let Some(pending) = merge_state.pending_repair_generation {
        if generation == pending {
            if enriched {
                gate.input_method = snapshot.input_method;
                gate.error = snapshot.error;
                gate.completes_repair = false;
                merge_state.latest_is_enriched = true;
                merge_state.pending_repair_generation = None;
            }
            return;
        }
        merge_state.pending_repair_generation = None;
    }

    if repairing.is_some() {
        // Keep the old repair result authoritative until its required snapshot
        // arrives; an unrelated check may still finish while repair is running.
        if merge_state
            .active_repair_generation
            .is_some_and(|active| generation < active)
        {
            return;
        }
        gate.input_method = snapshot.input_method;
        return;
    }

    if enriched {
        match merge_state.latest_generation {
            Some(latest) if generation == latest => {
                gate.input_method = snapshot.input_method;
                gate.error = snapshot.error;
                merge_state.latest_is_enriched = true;
            },
            Some(latest) if generation > latest => {
                // The required half can be delivered after its enrichment on
                // a different producer. The generation still proves this is
                // newer than the current gate, so apply the complete result.
                *gate = snapshot;
                merge_state.latest_generation = Some(generation);
                merge_state.latest_is_enriched = true;
            },
            None => {
                *gate = snapshot;
                merge_state.latest_generation = Some(generation);
                merge_state.latest_is_enriched = true;
            },
            Some(_) => {},
        }
        return;
    }

    *gate = snapshot;
    merge_state.latest_generation = Some(generation);
    merge_state.latest_is_enriched = false;
}

pub fn apply_permission_snapshot(handle: &SettingsHandle, snapshot: PermissionSnapshot, cx: &mut App) {
    handle
        .update(cx, |view, _window, cx| {
            merge_permission_snapshot(
                &mut view.gate,
                &mut view.repairing,
                &mut view.permission_merge,
                snapshot,
            );
            cx.notify();
        })
        .ok();
}

pub fn refresh_permission_check(handle: &SettingsHandle, proxy: &EventLoopProxy, cx: &mut App) {
    let idle = handle
        .update(cx, |view, _window, _cx| view.repairing.is_none())
        .unwrap_or(false);
    if idle {
        permissions::spawn_check(proxy);
    }
}

pub fn focus_settings(handle: &SettingsHandle, cx: &mut App) -> bool {
    let ok = handle
        .update(cx, |_view, window, _cx| {
            window.activate_window();
        })
        .is_ok();
    if ok {
        activate_app();
    }
    ok
}

pub fn close_settings(handle: &SettingsHandle, cx: &mut App) {
    handle
        .update(cx, |view, window, cx| {
            view.clear_ai_draft(cx);
            window.remove_window();
        })
        .ok();
}

fn settings_section_from_path(path: &str) -> Section {
    let path = path.to_ascii_lowercase();
    // Tray / CLI still send the WebView-era paths. Map them onto the
    // native pages that actually hold that content.
    if path.contains("behavior") || path.contains("autocomplete") {
        Section::Behavior
    } else if path.contains("jev") || path.ends_with("/ai") || path == "ai" {
        if AI_SETTINGS_VISIBLE {
            Section::Ai
        } else {
            Section::Appearance
        }
    } else if path.contains("about") || path.contains("help") || path.contains("troubleshoot") {
        Section::About
    } else {
        Section::Appearance
    }
}

pub fn set_settings_section(handle: &SettingsHandle, path: &str, cx: &mut App) {
    let section = settings_section_from_path(path);
    handle
        .update(cx, |view, _window, cx| {
            if view.section != section {
                view.clear_ai_draft(cx);
            }
            view.section = section;
            view.theme_controls.menu = None;
            cx.notify();
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermReady;

    fn snapshot(accessibility: PermReady, shell: PermReady, input_method: PermReady) -> PermissionSnapshot {
        snapshot_with_generation(1, accessibility, shell, input_method)
    }

    fn snapshot_with_generation(
        generation: u64,
        accessibility: PermReady,
        shell: PermReady,
        input_method: PermReady,
    ) -> PermissionSnapshot {
        PermissionSnapshot {
            accessibility,
            shell,
            input_method,
            error: None,
            generation,
            completes_repair: false,
        }
    }

    #[test]
    fn webview_deep_links_open_the_matching_native_page() {
        assert_eq!(settings_section_from_path("/"), Section::Appearance);
        assert_eq!(settings_section_from_path("/appearance"), Section::Appearance);
        assert_eq!(settings_section_from_path("/preferences"), Section::Appearance);
        assert_eq!(settings_section_from_path("/behavior"), Section::Behavior);
        assert_eq!(settings_section_from_path("/autocomplete"), Section::Behavior);
        assert_eq!(settings_section_from_path("/ai"), Section::Appearance);
        assert_eq!(settings_section_from_path("/jev"), Section::Appearance);
        assert_eq!(settings_section_from_path("/about"), Section::About);
        assert_eq!(settings_section_from_path("/help"), Section::About);
        assert_eq!(settings_section_from_path("/troubleshooting"), Section::About);
    }

    #[test]
    fn settings_do_not_show_the_gate_while_permissions_are_checking() {
        assert!(PermissionSnapshot::checking().still_checking());
        assert!(!shows_permission_gate(&PermissionSnapshot::checking()));
        assert!(snapshot(PermReady::Ready, PermReady::Checking, PermReady::Ready).still_checking());
        assert!(!snapshot(PermReady::Ready, PermReady::Ready, PermReady::Checking).still_checking());
    }

    #[test]
    fn late_ime_snapshot_does_not_clear_in_flight_repair() {
        let mut gate = snapshot(PermReady::Missing, PermReady::Ready, PermReady::Checking);
        let mut repairing = Some(PermId::Accessibility);
        let mut merge_state = PermissionMergeState {
            active_repair_generation: Some(2),
            ..Default::default()
        };
        let mut ime_fill = snapshot_with_generation(1, PermReady::Missing, PermReady::Ready, PermReady::Missing);
        merge_permission_snapshot(&mut gate, &mut repairing, &mut merge_state, ime_fill.clone());
        assert_eq!(repairing, Some(PermId::Accessibility));
        assert_eq!(gate.input_method, PermReady::Checking);

        ime_fill.generation = 2;
        ime_fill.completes_repair = true;
        ime_fill.accessibility = PermReady::Ready;
        merge_permission_snapshot(&mut gate, &mut repairing, &mut merge_state, ime_fill);
        assert_eq!(repairing, None);
        assert_eq!(gate.accessibility, PermReady::Ready);
    }

    #[test]
    fn late_enriched_snapshot_cannot_overwrite_repair_complete_state() {
        let mut gate = snapshot_with_generation(1, PermReady::Missing, PermReady::Ready, PermReady::Checking);
        let mut repairing = Some(PermId::Accessibility);
        let mut merge_state = PermissionMergeState {
            active_repair_generation: Some(2),
            ..Default::default()
        };

        // An older check may finish its enriched IME query while repair is in flight.
        merge_permission_snapshot(
            &mut gate,
            &mut repairing,
            &mut merge_state,
            snapshot_with_generation(1, PermReady::Missing, PermReady::Ready, PermReady::Missing),
        );

        let mut repaired = snapshot_with_generation(2, PermReady::Ready, PermReady::Ready, PermReady::Checking);
        repaired.completes_repair = true;
        merge_permission_snapshot(&mut gate, &mut repairing, &mut merge_state, repaired);
        assert_eq!(repairing, None);
        assert_eq!(gate.accessibility, PermReady::Ready);
        assert_eq!(gate.shell, PermReady::Ready);
        assert_eq!(gate.input_method, PermReady::Checking);

        // This is the old enriched result. Its AX/Shell base is stale, so it
        // must not replace the repair-complete state (including IME).
        merge_permission_snapshot(
            &mut gate,
            &mut repairing,
            &mut merge_state,
            snapshot_with_generation(1, PermReady::Missing, PermReady::Ready, PermReady::Missing),
        );
        assert_eq!(gate.accessibility, PermReady::Ready);
        assert_eq!(gate.shell, PermReady::Ready);
        assert_eq!(gate.input_method, PermReady::Checking);

        // The companion enriched result from the repair has the same required
        // base and is still allowed to fill in the optional IME status.
        merge_permission_snapshot(
            &mut gate,
            &mut repairing,
            &mut merge_state,
            snapshot_with_generation(2, PermReady::Ready, PermReady::Ready, PermReady::Ready),
        );
        assert_eq!(gate.accessibility, PermReady::Ready);
        assert_eq!(gate.shell, PermReady::Ready);
        assert_eq!(gate.input_method, PermReady::Ready);
        assert!(!gate.completes_repair);
    }

    #[test]
    fn stale_enriched_snapshot_is_ignored_after_a_new_required_snapshot() {
        let mut gate = snapshot_with_generation(1, PermReady::Ready, PermReady::Ready, PermReady::Checking);
        let mut repairing = None;
        let mut merge_state = PermissionMergeState::default();

        merge_permission_snapshot(
            &mut gate,
            &mut repairing,
            &mut merge_state,
            snapshot_with_generation(2, PermReady::Ready, PermReady::Ready, PermReady::Checking),
        );
        merge_permission_snapshot(
            &mut gate,
            &mut repairing,
            &mut merge_state,
            snapshot_with_generation(1, PermReady::Missing, PermReady::Ready, PermReady::Missing),
        );
        assert_eq!(gate.accessibility, PermReady::Ready);
        assert_eq!(gate.shell, PermReady::Ready);
        assert_eq!(gate.input_method, PermReady::Checking);
    }

    #[test]
    fn newer_required_generation_releases_repair_barrier_before_same_state_old_enrichment() {
        let mut gate = snapshot_with_generation(1, PermReady::Missing, PermReady::Ready, PermReady::Checking);
        let mut repairing = Some(PermId::Accessibility);
        let mut merge_state = PermissionMergeState {
            active_repair_generation: Some(2),
            ..Default::default()
        };

        let mut repaired = snapshot_with_generation(2, PermReady::Ready, PermReady::Ready, PermReady::Checking);
        repaired.completes_repair = true;
        merge_permission_snapshot(&mut gate, &mut repairing, &mut merge_state, repaired);
        assert_eq!(merge_state.pending_repair_generation, Some(2));

        // A new check may have the same AX/Shell values. Its generation, not
        // the values, is what releases the old repair barrier.
        merge_permission_snapshot(
            &mut gate,
            &mut repairing,
            &mut merge_state,
            snapshot_with_generation(3, PermReady::Ready, PermReady::Ready, PermReady::Checking),
        );
        assert_eq!(merge_state.pending_repair_generation, None);
        assert_eq!(merge_state.latest_generation, Some(3));

        // The old enriched result has the same readiness values but is still
        // rejected by generation ordering.
        merge_permission_snapshot(
            &mut gate,
            &mut repairing,
            &mut merge_state,
            snapshot_with_generation(2, PermReady::Ready, PermReady::Ready, PermReady::Missing),
        );
        assert_eq!(gate.input_method, PermReady::Checking);

        merge_permission_snapshot(
            &mut gate,
            &mut repairing,
            &mut merge_state,
            snapshot_with_generation(3, PermReady::Ready, PermReady::Ready, PermReady::Ready),
        );
        assert_eq!(gate.input_method, PermReady::Ready);
    }

    #[test]
    fn active_repair_completion_wins_over_a_newer_check_started_while_repairing() {
        let mut gate = snapshot_with_generation(1, PermReady::Missing, PermReady::Ready, PermReady::Checking);
        let mut repairing = Some(PermId::Accessibility);
        let mut merge_state = PermissionMergeState {
            active_repair_generation: Some(2),
            ..Default::default()
        };

        // This ordinary check is newer, but it must not poison the active
        // repair's completion marker or its required AX/Shell values.
        merge_permission_snapshot(
            &mut gate,
            &mut repairing,
            &mut merge_state,
            snapshot_with_generation(3, PermReady::Missing, PermReady::Missing, PermReady::Checking),
        );
        assert_eq!(repairing, Some(PermId::Accessibility));
        assert_eq!(gate.accessibility, PermReady::Missing);
        assert_eq!(gate.shell, PermReady::Ready);
        assert_eq!(merge_state.latest_generation, None);

        let mut repaired = snapshot_with_generation(2, PermReady::Ready, PermReady::Ready, PermReady::Checking);
        repaired.completes_repair = true;
        merge_permission_snapshot(&mut gate, &mut repairing, &mut merge_state, repaired);
        assert_eq!(repairing, None);
        assert_eq!(gate.accessibility, PermReady::Ready);
        assert_eq!(gate.shell, PermReady::Ready);
        assert_eq!(merge_state.latest_generation, Some(2));
        assert_eq!(merge_state.pending_repair_generation, Some(2));
    }

    #[test]
    fn previous_window_repair_completion_cannot_clear_current_window_repair() {
        // Window A owned generation 1 and was closed while its detached repair
        // task was still running. Window B starts generation 2 after opening.
        let mut gate_b = snapshot_with_generation(2, PermReady::Ready, PermReady::Ready, PermReady::Checking);
        let mut repairing_b = Some(PermId::Accessibility);
        let mut merge_state_b = PermissionMergeState {
            active_repair_generation: Some(2),
            ..Default::default()
        };

        // Keep the readiness values identical to B's state: generation is the
        // only reliable identity once the old window and its merge state are gone.
        let mut old_completion = snapshot_with_generation(1, PermReady::Ready, PermReady::Ready, PermReady::Checking);
        old_completion.completes_repair = true;
        merge_permission_snapshot(&mut gate_b, &mut repairing_b, &mut merge_state_b, old_completion);
        assert_eq!(repairing_b, Some(PermId::Accessibility));
        assert_eq!(merge_state_b.active_repair_generation, Some(2));
        assert_eq!(gate_b.accessibility, PermReady::Ready);
        assert_eq!(gate_b.shell, PermReady::Ready);

        // B's own completion still releases the barrier.
        let mut current_completion =
            snapshot_with_generation(2, PermReady::Ready, PermReady::Ready, PermReady::Checking);
        current_completion.completes_repair = true;
        merge_permission_snapshot(&mut gate_b, &mut repairing_b, &mut merge_state_b, current_completion);
        assert_eq!(repairing_b, None);
        assert_eq!(merge_state_b.active_repair_generation, None);
    }

    #[test]
    fn finish_setup_marks_input_method_optional() {
        let production = include_str!("settings_ui.rs")
            .rsplit_once("mod tests {")
            .map(|(src, _)| src)
            .expect("production source");
        let start = production.find("fn permission_gate_page").expect("gate");
        let body = production[start..]
            .split("fn optional_input_method_card")
            .next()
            .expect("gate body");
        assert!(body.contains("PermId::Accessibility"));
        assert!(body.contains("PermId::Shell"));
        assert!(
            body.contains("PermId::InputMethod"),
            "Finish Setup still lists the input method"
        );
        assert!(body.contains("非必选"));
        assert!(body.contains("Optional"));
        let rows = body
            .split("let rows = [")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("rows");
        assert!(rows.contains("PermId::InputMethod"));
    }

    #[test]
    fn permission_pages_can_change_display_language() {
        let production = include_str!("settings_ui.rs")
            .rsplit_once("mod tests {")
            .map(|(src, _)| src)
            .expect("production source");

        let options = production
            .split("fn language_options")
            .nth(1)
            .and_then(|rest| rest.split("fn language_select").next())
            .expect("language options");
        assert!(options.contains("\"system\""));
        assert!(options.contains("\"en\""));
        assert!(options.contains("\"zh-CN\""));
        assert!(options.contains("跟随系统"));
        assert!(options.contains("Follow System"));

        let select = production
            .split("fn language_select")
            .nth(1)
            .and_then(|rest| rest.split("fn permission_language_bar").next())
            .expect("language select");
        assert!(select.contains("dashboard.language"));
        assert!(select.contains("set_string"));
        assert!(select.contains("\"zh-CN\" | \"zh\""));
        assert!(
            select.contains("language_options(zh), current,"),
            "the chip row must take the static option id, not the local String"
        );

        let bar = production
            .split("fn permission_language_bar")
            .nth(1)
            .and_then(|rest| rest.split("fn appearance_page").next())
            .expect("language bar");
        let reserved = bar.find("SETTINGS_TITLEBAR_H").expect("titlebar spacer");
        let chips = bar.find("language_select").expect("language chips");
        assert!(
            reserved < chips,
            "language chips must sit below the titlebar drag strip"
        );

        let checking = production
            .split("fn permission_checking_page")
            .nth(1)
            .and_then(|rest| rest.split("fn permission_gate_page").next())
            .expect("checking page");
        assert!(checking.contains("permission_language_bar"));
        assert!(checking.contains("ec-perm-check-lang"));

        let gate = production
            .split("fn permission_gate_page")
            .nth(1)
            .and_then(|rest| rest.split("fn pill").next())
            .expect("gate page");
        assert!(gate.contains("permission_language_bar"));
        assert!(gate.contains("ec-perm-lang"));

        let appearance = production
            .split("fn appearance_page")
            .nth(1)
            .and_then(|rest| rest.split("fn behavior_page").next())
            .expect("appearance page");
        assert!(
            appearance.contains("language_select(\"ec-lang\""),
            "appearance must keep the same language control"
        );
    }

    #[test]
    fn settings_show_the_gate_only_after_a_required_check_fails() {
        assert!(shows_permission_gate(&snapshot(
            PermReady::Missing,
            PermReady::Ready,
            PermReady::Ready
        )));
        assert!(shows_permission_gate(&snapshot(
            PermReady::Ready,
            PermReady::Missing,
            PermReady::Ready
        )));
        assert!(!shows_permission_gate(&snapshot(
            PermReady::Ready,
            PermReady::Ready,
            PermReady::Ready
        )));
        assert!(
            !shows_permission_gate(&snapshot(PermReady::Ready, PermReady::Ready, PermReady::Missing)),
            "optional input method must not send the user to the grant page"
        );
    }

    #[test]
    fn fix_all_does_not_open_system_settings_before_the_accessibility_check() {
        let production = include_str!("settings_ui.rs")
            .rsplit_once("mod tests {")
            .map(|(src, _)| src)
            .expect("production source");
        let start = production.find("ec-perm-fix-all").expect("Fix All");
        let end = (start + 1600).min(production.len());
        let body = &production[start..end];
        assert!(
            !body.contains("begin_accessibility_guide"),
            "Fix All must let repair() check Accessibility before opening System Settings"
        );
        assert!(body.contains("spawn_repair_all"));
    }

    #[test]
    fn theme_catalog_keeps_the_old_groups() {
        let ids = |appearance: ThemeAppearance| -> Vec<&'static str> {
            THEMES
                .iter()
                .filter(|theme| theme.appearance == appearance)
                .map(|theme| theme.id)
                .collect()
        };
        assert_eq!(ids(ThemeAppearance::System), ["system"]);
        assert_eq!(
            ids(ThemeAppearance::Light),
            ["light", "github-light", "claude-light", "catppuccin-latte"]
        );
        assert_eq!(
            ids(ThemeAppearance::Dark),
            [
                "dark",
                "github-dark",
                "claude-dark",
                "nord",
                "gruvbox-dark",
                "one-dark",
                "tokyo-night"
            ]
        );
    }

    /// A swatch whose id has no theme file falls back to [`OverlayTheme::dark`]
    /// without a word (`load_named_theme` swallows the miss), so the picker would
    /// silently offer the wrong colors.
    #[test]
    fn every_offered_theme_has_a_file_or_is_built_in() {
        for theme in THEMES {
            if matches!(theme.id, "system" | "light" | "dark") {
                continue;
            }
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../themes")
                .join(format!("{}.json", theme.id));
            assert!(
                path.exists(),
                "{} is offered but themes/{}.json is missing",
                theme.id,
                theme.id
            );
        }
    }
}
