//! The `chart` component: bars, lines or areas over labelled points, painted with GPUI
//! (no web view). The plot is a canvas; the axes' labels, the legend and the tooltip of the
//! point under the mouse are ordinary elements around and over it.

use gpui::{
    AnyElement, Bounds, Context, Hsla, InteractiveElement as _, IntoElement, MouseMoveEvent, ParentElement as _, PathBuilder, Pixels, Point,
    StatefulInteractiveElement as _, Styled as _, canvas, div, fill, point, px, size,
};
use serde_json::{Value, json};
use theme::ActiveTheme as _;
use ui::{Color, Label, LabelCommon as _, LabelSize, h_flex, v_flex};

use crate::surface::{Surface, styled, token_color};
use crate::tree::{Node, NodeId};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bar,
    Line,
    Area,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub name: String,
    /// One per label; `None` leaves a gap.
    pub values: Vec<Option<f64>>,
    /// A theme colour token (`accent`, `success`…); else the next of the theme's accents.
    pub color: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    pub kind: Kind,
    pub labels: Vec<String>,
    pub series: Vec<Series>,
    pub height: f32,
}

impl Chart {
    pub fn from_node(node: &Node) -> Self {
        let kind = match node.str_prop("kind") {
            Some("line") => Kind::Line,
            Some("area") => Kind::Area,
            _ => Kind::Bar,
        };
        let labels = node.prop("labels").and_then(Value::as_array).map(|l| l.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect()).unwrap_or_default();
        let series = node
            .prop("series")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|s| Series {
                        name: s.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                        values: s.get("values").and_then(Value::as_array).map(|v| v.iter().map(Value::as_f64).collect()).unwrap_or_default(),
                        color: s.get("color").and_then(Value::as_str).map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let height = node.prop("height").and_then(Value::as_f64).unwrap_or(180.) as f32;
        Self { kind, labels, series, height }
    }

    /// How many points: the labels, or the longest series when there are none.
    pub fn points(&self) -> usize {
        self.labels.len().max(self.series.iter().map(|s| s.values.len()).max().unwrap_or(0))
    }

    fn label(&self, index: usize) -> String {
        self.labels.get(index).cloned().unwrap_or_else(|| (index + 1).to_string())
    }
}

/// The value axis: its ticks from bottom to top (the first and last are its ends). Zero is
/// always on it, so bars start from it.
pub fn ticks(values: impl Iterator<Item = f64>) -> Vec<f64> {
    let (mut low, mut high) = values.filter(|v| v.is_finite()).fold((0f64, 0f64), |(lo, hi), v| (lo.min(v), hi.max(v)));
    if high - low < f64::EPSILON {
        high = low + 1.;
    }
    let step = nice_step((high - low) / 5.);
    low = (low / step).floor() * step;
    high = (high / step).ceil() * step;
    let count = ((high - low) / step).round() as usize;
    // Rounded so that 3 × 0.2 is 0.6.
    (0..=count).map(|i| ((low + step * i as f64) * 1e9).round() / 1e9).collect()
}

/// 1, 2, 2.5 or 5 times a power of ten, at least `raw`.
fn nice_step(raw: f64) -> f64 {
    let magnitude = 10f64.powf(raw.log10().floor());
    let fraction = raw / magnitude;
    let nice = [1., 2., 2.5, 5., 10.].into_iter().find(|n| fraction <= *n + 1e-9).unwrap_or(10.);
    nice * magnitude
}

/// A value for an axis or a tooltip: `1.5k`, `2M`, `0.25`.
pub fn format_value(value: f64) -> String {
    let abs = value.abs();
    let (scaled, suffix) = match abs {
        a if a >= 1e9 => (value / 1e9, "B"),
        a if a >= 1e6 => (value / 1e6, "M"),
        a if a >= 1e4 => (value / 1e3, "k"),
        _ => (value, ""),
    };
    let text = if scaled.fract().abs() < 1e-9 { format!("{scaled:.0}") } else if scaled.abs() >= 100. { format!("{scaled:.0}") } else { format!("{scaled:.2}") };
    let text = if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.').to_string() } else { text };
    format!("{text}{suffix}")
}

const AXIS_WIDTH: f32 = 44.;

impl Surface {
    pub(crate) fn render_chart(&self, node: &Node, id: NodeId, eid: gpui::ElementId, style: &Value, cx: &Context<Self>) -> AnyElement {
        let chart = Chart::from_node(node);
        let points = chart.points();
        let ticks = ticks(chart.series.iter().flat_map(|s| s.values.iter().flatten().copied()));
        let (low, high) = (ticks[0], ticks[ticks.len() - 1]);
        let accents = &cx.theme().accents().0;
        let colors: Vec<Hsla> = chart
            .series
            .iter()
            .enumerate()
            .map(|(i, s)| s.color.as_deref().and_then(|c| token_color(c, cx)).or_else(|| accents.get(i % accents.len().max(1)).copied()).unwrap_or(cx.theme().colors().text_accent))
            .collect();
        let theme = cx.theme().colors();
        let (grid_color, text_muted, tooltip_bg, border) = (theme.border_variant, theme.text_muted, theme.elevated_surface_background, theme.border);
        let hovered = self.chart_hover.borrow().get(&id).copied().filter(|i| *i < points);
        let bounds_cell = self.chart_bounds.clone();
        let height = px(chart.height);

        // The value axis: tick labels at their heights.
        let axis = div().relative().w(px(AXIS_WIDTH)).h(height).flex_none().children(ticks.iter().map(|tick| {
            let fraction = ((tick - low) / (high - low)) as f32;
            div()
                .absolute()
                .right(px(6.))
                .top(height * (1. - fraction) - px(7.))
                .child(Label::new(format_value(*tick)).size(LabelSize::XSmall).color(Color::Muted))
        }));

        let plot_chart = chart.clone();
        let plot_colors = colors.clone();
        let plot_ticks = ticks.clone();
        let plot = canvas(
            move |bounds, _, _| bounds,
            move |_, bounds: Bounds<Pixels>, window, _| {
                bounds_cell.borrow_mut().insert(id, bounds);
                paint(&plot_chart, &plot_ticks, &plot_colors, hovered, grid_color, text_muted, bounds, window);
            },
        )
        .size_full();

        let hover_bounds = self.chart_bounds.clone();
        let host = self.host.clone();
        let labels = chart.labels.clone();
        let plot_area = div()
            .id(eid)
            .relative()
            .flex_1()
            .min_w_0()
            .h(height)
            .child(plot)
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                let Some(bounds) = hover_bounds.borrow().get(&id).copied() else { return };
                let index = index_at(event.position, bounds, points);
                if this.chart_hover.borrow().get(&id).copied() != index {
                    match index {
                        Some(index) => this.chart_hover.borrow_mut().insert(id, index),
                        None => this.chart_hover.borrow_mut().remove(&id),
                    };
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(move |this, hovering: &bool, _, cx| {
                if !*hovering && this.chart_hover.borrow_mut().remove(&id).is_some() {
                    cx.notify();
                }
            }))
            .when_some(node.has_event("onClick").then_some(()), |el, _| {
                let labels = labels.clone();
                el.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(index) = this.chart_hover.borrow().get(&id).copied() {
                        host.read(cx).dispatch(id, "onClick", json!({ "index": index, "label": labels.get(index) }));
                    }
                }))
            })
            .children(hovered.map(|index| {
                // Beside the point, on the side with more room.
                let bounds = self.chart_bounds.borrow().get(&id).copied();
                let slot = bounds.map(|b| b.size.width / points.max(1) as f32).unwrap_or(px(0.));
                let x = slot * (index as f32 + 0.5);
                let width = bounds.map(|b| b.size.width).unwrap_or(px(0.));
                let tooltip = v_flex()
                    .absolute()
                    .top(px(4.))
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .bg(tooltip_bg)
                    .child(Label::new(chart.label(index)).size(LabelSize::Small))
                    .children(chart.series.iter().zip(&colors).map(|(series, color)| {
                        let value = series.values.get(index).copied().flatten().map(format_value).unwrap_or_else(|| "—".into());
                        h_flex()
                            .gap_1()
                            .child(div().size(px(8.)).rounded_sm().bg(*color))
                            .when(!series.name.is_empty(), |row| row.child(Label::new(series.name.clone()).size(LabelSize::XSmall).color(Color::Muted)))
                            .child(Label::new(value).size(LabelSize::XSmall))
                    }));
                if x < width / 2. { tooltip.left(x + px(8.)) } else { tooltip.right(width - x + px(8.)) }
            }));

        // The labels under the points, thinned out when they would crowd.
        let every = points.div_ceil(12).max(1);
        let x_labels = h_flex().pl(px(AXIS_WIDTH)).children((0..points).map(|i| {
            let label = if i % every == 0 { chart.label(i) } else { String::new() };
            div().flex_1().min_w_0().flex().justify_center().overflow_hidden().child(Label::new(label).size(LabelSize::XSmall).color(Color::Muted).truncate())
        }));

        let legend = (chart.series.len() > 1 || node.prop("legend").and_then(Value::as_bool) == Some(true)).then(|| {
            h_flex().pl(px(AXIS_WIDTH)).gap_3().flex_wrap().children(chart.series.iter().zip(&colors).map(|(series, color)| {
                h_flex().gap_1().child(div().size(px(8.)).rounded_sm().bg(*color)).child(Label::new(series.name.clone()).size(LabelSize::XSmall).color(Color::Muted))
            }))
        });

        let content = v_flex().w_full().gap_1().children(legend).child(h_flex().w_full().child(axis).child(plot_area)).child(x_labels);
        styled(div().w_full(), style, cx).child(content).into_any_element()
    }
}

use gpui::prelude::FluentBuilder as _;

/// The point under `position`, if it is over the plot.
fn index_at(position: Point<Pixels>, bounds: Bounds<Pixels>, points: usize) -> Option<usize> {
    if points == 0 || !bounds.contains(&position) {
        return None;
    }
    let fraction = (position.x - bounds.origin.x) / bounds.size.width;
    Some(((fraction * points as f32) as usize).min(points - 1))
}

#[allow(clippy::too_many_arguments)]
fn paint(chart: &Chart, ticks: &[f64], colors: &[Hsla], hovered: Option<usize>, grid: Hsla, guide: Hsla, bounds: Bounds<Pixels>, window: &mut gpui::Window) {
    let points = chart.points();
    let (low, high) = (ticks[0], ticks[ticks.len() - 1]);
    let y = |value: f64| bounds.origin.y + bounds.size.height * (1. - ((value - low) / (high - low)) as f32);
    let slot = bounds.size.width / points.max(1) as f32;
    let x_center = |index: usize| bounds.origin.x + slot * (index as f32 + 0.5);

    for tick in ticks {
        let line_y = y(*tick);
        let color = if *tick == 0. { guide.opacity(0.5) } else { grid };
        window.paint_quad(fill(Bounds::new(point(bounds.origin.x, line_y), size(bounds.size.width, px(1.))), color));
    }
    if let Some(index) = hovered {
        let highlight = match chart.kind {
            Kind::Bar => Bounds::new(point(bounds.origin.x + slot * index as f32, bounds.origin.y), size(slot, bounds.size.height)),
            _ => Bounds::new(point(x_center(index), bounds.origin.y), size(px(1.), bounds.size.height)),
        };
        window.paint_quad(fill(highlight, guide.opacity(if chart.kind == Kind::Bar { 0.08 } else { 0.4 })));
    }

    match chart.kind {
        Kind::Bar => {
            let groups = chart.series.len().max(1);
            let bar = (slot * 0.7 / groups as f32).max(px(1.));
            let zero = y(0f64.clamp(low, high));
            for (s, series) in chart.series.iter().enumerate() {
                for (i, value) in series.values.iter().enumerate().take(points) {
                    let Some(value) = value else { continue };
                    let left = bounds.origin.x + slot * i as f32 + slot * 0.15 + bar * s as f32;
                    let top = y(*value).min(zero);
                    let height = (y(*value) - zero).abs().max(px(1.));
                    window.paint_quad(fill(Bounds::new(point(left, top), size(bar - px(1.), height)), colors[s]));
                }
            }
        }
        Kind::Line | Kind::Area => {
            for (s, series) in chart.series.iter().enumerate() {
                // Runs of consecutive values; gaps break the line.
                let mut runs: Vec<Vec<Point<Pixels>>> = vec![vec![]];
                for (i, value) in series.values.iter().enumerate().take(points) {
                    match value {
                        Some(v) => runs.last_mut().unwrap().push(point(x_center(i), y(*v))),
                        None => runs.push(vec![]),
                    }
                }
                for run in runs.iter().filter(|r| !r.is_empty()) {
                    if chart.kind == Kind::Area && run.len() > 1 {
                        let base = y(0f64.clamp(low, high));
                        let mut area = PathBuilder::fill();
                        area.move_to(point(run[0].x, base));
                        run.iter().for_each(|p| area.line_to(*p));
                        area.line_to(point(run[run.len() - 1].x, base));
                        area.close();
                        if let Ok(path) = area.build() {
                            window.paint_path(path, colors[s].opacity(0.2));
                        }
                    }
                    if run.len() > 1 {
                        let mut line = PathBuilder::stroke(px(2.));
                        line.move_to(run[0]);
                        run[1..].iter().for_each(|p| line.line_to(*p));
                        if let Ok(path) = line.build() {
                            window.paint_path(path, colors[s]);
                        }
                    }
                    for p in run {
                        let r = if hovered.is_some_and(|h| (x_center(h) - p.x).abs() < px(0.5)) { px(4.) } else if points <= 40 { px(2.5) } else { px(0.) };
                        if r > px(0.) {
                            window.paint_quad(fill(Bounds::new(point(p.x - r, p.y - r), size(r * 2., r * 2.)), colors[s]).corner_radii(r));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_round_ticks_that_include_zero() {
        assert_eq!(ticks([3., 17., 42.].into_iter()), [0., 10., 20., 30., 40., 50.]);
        assert_eq!(ticks([-12., 30.].into_iter()), [-20., -10., 0., 10., 20., 30.]);
        assert_eq!(ticks([0.2, 0.9].into_iter()), [0., 0.2, 0.4, 0.6, 0.8, 1.]);
        assert_eq!(ticks(std::iter::empty()), [0., 0.2, 0.4, 0.6, 0.8, 1.]);
        assert_eq!(ticks([1200., 5300.].into_iter()), [0., 2000., 4000., 6000.]);
    }

    #[test]
    fn formats_values_compactly() {
        assert_eq!(format_value(0.), "0");
        assert_eq!(format_value(0.25), "0.25");
        assert_eq!(format_value(1234.), "1234");
        assert_eq!(format_value(15000.), "15k");
        assert_eq!(format_value(2_500_000.), "2.5M");
        assert_eq!(format_value(-42.5), "-42.5");
        assert_eq!(format_value(3e9), "3B");
    }

    #[test]
    fn finds_the_point_under_the_mouse() {
        let bounds = Bounds::new(point(px(100.), px(0.)), size(px(400.), px(100.)));
        assert_eq!(index_at(point(px(110.), px(50.)), bounds, 4), Some(0));
        assert_eq!(index_at(point(px(499.), px(50.)), bounds, 4), Some(3));
        assert_eq!(index_at(point(px(50.), px(50.)), bounds, 4), None);
        assert_eq!(index_at(point(px(110.), px(50.)), bounds, 0), None);
    }
}
