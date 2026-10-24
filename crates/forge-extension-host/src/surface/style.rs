//! The `style` props of nodes: sizes, colors (theme tokens), spacing and layout, and the
//! icons, label sizes and colors components take.

use super::*;

pub(super) fn icon_name(name: &str) -> Option<IconName> {
    IconName::from_str(name).ok()
}

pub(super) fn icon_size(style: &Value) -> IconSize {
    match style.get("size").and_then(Value::as_str) {
        Some("xs") => IconSize::XSmall,
        Some("sm") => IconSize::Small,
        Some("lg") => IconSize::Medium,
        _ => IconSize::Small,
    }
}

pub(super) fn label_size(style: &Value) -> LabelSize {
    match style.get("size").and_then(Value::as_str) {
        Some("xs") => LabelSize::XSmall,
        Some("sm") => LabelSize::Small,
        Some("lg") => LabelSize::Large,
        _ => LabelSize::Default,
    }
}

pub(super) fn label_color(token: &str) -> Color {
    match token {
        "muted" => Color::Muted,
        "accent" => Color::Accent,
        "error" => Color::Error,
        "warning" => Color::Warning,
        "success" => Color::Success,
        _ => Color::Default,
    }
}

pub(crate) fn token_color(token: &str, cx: &App) -> Option<Hsla> {
    let c = cx.theme().colors();
    let s = cx.theme().status();
    Some(match token {
        "default" => c.text,
        "muted" => c.text_muted,
        "accent" => c.text_accent,
        "error" => s.error,
        "warning" => s.warning,
        "success" => s.success,
        "surface" => c.surface_background,
        "elevated" => c.elevated_surface_background,
        "panel" => c.panel_background,
        "editor" => c.editor_background,
        "transparent" => gpui::transparent_black(),
        _ => return None,
    })
}

pub(crate) fn styled<E: gpui::Styled>(el: E, style: &Value, cx: &App) -> E {
    let num = |k: &str| style.get(k).and_then(Value::as_f64).map(|v| px(v as f32));
    let s = |k: &str| style.get(k).and_then(Value::as_str);
    let mut el = el.flex();
    el = if s("direction") == Some("row") { el.flex_row().items_center() } else { el.flex_col() };
    if let Some(v) = num("gap") {
        el = el.gap(v);
    }
    if let Some(v) = num("padding") {
        el = el.p(v);
    }
    if let Some(v) = num("paddingX") {
        el = el.px(v);
    }
    if let Some(v) = num("paddingY") {
        el = el.py(v);
    }
    el = match s("align") {
        Some("start") => el.items_start(),
        Some("center") => el.items_center(),
        Some("end") => el.items_end(),
        Some("stretch") => el.items_stretch(),
        _ => el,
    };
    el = match s("justify") {
        Some("start") => el.justify_start(),
        Some("center") => el.justify_center(),
        Some("end") => el.justify_end(),
        Some("between") => el.justify_between(),
        _ => el,
    };
    if style.get("grow").and_then(Value::as_bool) == Some(true) {
        el = el.flex_grow_1().flex_basis(px(0.)).min_h_0().min_w_0();
    }
    if style.get("shrink").and_then(Value::as_bool) == Some(false) {
        el = el.flex_shrink_0();
    }
    if style.get("wrap").and_then(Value::as_bool) == Some(true) {
        el = el.flex_wrap();
    }
    el = match style.get("width") {
        Some(Value::String(f)) if f == "full" => el.w_full(),
        Some(v) if v.is_number() => el.w(px(v.as_f64().unwrap_or_default() as f32)),
        _ => el,
    };
    el = match style.get("height") {
        Some(Value::String(f)) if f == "full" => el.h_full(),
        Some(v) if v.is_number() => el.h(px(v.as_f64().unwrap_or_default() as f32)),
        _ => el,
    };
    if let Some(v) = num("minWidth") {
        el = el.min_w(v);
    }
    if let Some(v) = num("maxWidth") {
        el = el.max_w(v);
    }
    if let Some(c) = s("background").and_then(|t| token_color(t, cx)) {
        el = el.bg(c);
    }
    if let Some(c) = s("color").and_then(|t| token_color(t, cx)) {
        el = el.text_color(c);
    }
    if style.get("border").and_then(Value::as_bool) == Some(true) {
        el = el.border_1().border_color(cx.theme().colors().border);
    }
    match s("borderSide") {
        Some("top") => el = el.border_t_1().border_color(cx.theme().colors().border),
        Some("bottom") => el = el.border_b_1().border_color(cx.theme().colors().border),
        Some("left") => el = el.border_l_1().border_color(cx.theme().colors().border),
        Some("right") => el = el.border_r_1().border_color(cx.theme().colors().border),
        _ => {}
    }
    if style.get("rounded").and_then(Value::as_bool) == Some(true) {
        el = el.rounded_md();
    }
    el
}
