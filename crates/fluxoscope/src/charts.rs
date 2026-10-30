//! Charts of the current view as SVG, drawn from Rust: the rows are the
//! series, the columns the x axis.

use crate::model::chart::{group_rows, nice_ticks, scale, stack, value_range};
use crate::model::format::format_number;
use crate::model::pivot::{visible_tuples, Tuple, MAX_VIEW_CELLS};
use crate::state::{Ctx, Loaded, Show};
use fluxolap_api_types::Value;
use leptos::prelude::*;

const PALETTE: [&str; 12] = ["#3884ff", "#f5a524", "#17c964", "#f31260", "#9353d3", "#06b7db", "#ff7a45", "#7fb800", "#e04dbd", "#5c7cfa", "#b08900", "#8c8c8c"];
const MAX_SERIES: usize = 12;
/// Most points along the x axis (the columns).
const MAX_POINTS: usize = 2_000;
/// Most small charts in one view.
const MAX_PANELS: usize = 36;
/// Most points across all small charts: each one is an SVG element.
const MAX_PANEL_MARKS: usize = 30_000;
const LEFT: f64 = 84.0;
const RIGHT: f64 = 20.0;
const TOP: f64 = 16.0;
const BOTTOM: f64 = 72.0;

/// An SVG's coordinate size. Panels draw on a smaller one, so that their
/// text stays readable instead of being scaled down with the chart.
#[derive(Clone, Copy)]
struct Size {
    w: f64,
    h: f64,
}

const FULL: Size = Size { w: 960.0, h: 420.0 };
const SMALL: Size = Size { w: 480.0, h: 260.0 };

/// A dimension and one of its consolidated elements: what a click opens.
type Drill = Option<(usize, u32)>;

struct Series {
    name: String,
    /// The series' place in the palette and the legend, the same in every panel.
    slot: usize,
    /// The row's innermost consolidated element (clicking the legend drills into it).
    drill: Drill,
    values: Vec<Option<f64>>,
}

/// One chart: the whole view, or one element's rows when split into panels.
struct Panel {
    title: String,
    drill: Drill,
    series: Vec<Series>,
}

/// What a chart draws, on a y axis it may share with other charts.
struct Plot<'a> {
    series: &'a [Series],
    x_labels: &'a [String],
    x_drills: &'a [Drill],
    size: Size,
    ticks: &'a [f64],
    range: (f64, f64),
    ctx: Ctx,
}

fn open(ctx: Ctx, drill: Drill) {
    if let Some((d, e)) = drill {
        ctx.change(move |p, h| p.toggle(d, e, &h[d]));
    }
}

#[component]
pub fn ChartView(show: Show) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || {
        ctx.explorer.with(|ex| {
            let ex = ex.as_ref()?;
            let p = ex.pivot.as_ref()?;
            if !p.rows.iter().chain(&p.cols).all(|&d| ex.loaded[d] == Loaded::All) {
                return Some(view! { <p class="muted">"loading elements …"</p> }.into_any());
            }
            let (mut rows, mut cols) = visible_tuples(p, &ex.hier, &ex.cells, ex.complete);
            if !ex.complete && rows.len().saturating_mul(cols.len()) > MAX_VIEW_CELLS {
                return Some(view! { <p class="muted">"reading the cells …"</p> }.into_any());
            }
            if cols.len() > MAX_POINTS {
                return Some(view! { <p class="muted">{format!("Too many columns to chart ({}) -- close some opened elements (at most {MAX_POINTS}).", cols.len())}</p> }.into_any());
            }
            let split = ex.panels;
            if show != Show::Line || split {
                // Bars add up: a total drawn next to (or stacked on) its own
                // opened children would count them twice, so opened totals go.
                // Panels share one scale: a total there would flatten its children.
                let opened = |t: &Tuple, dims: &[usize]| t.iter().zip(dims).any(|(&(e, _), &d)| p.expanded[d].contains(&e) && ex.hier[d].is_consolidated(e));
                rows.retain(|r| !opened(r, &p.rows));
                cols.retain(|c| !opened(c, &p.cols));
            }
            let tuple_name = |t: &[(u32, usize)], dims: &[usize]| t.iter().zip(dims).map(|(&(e, _), &d)| ex.hier[d].name(e)).collect::<Vec<_>>().join(" · ");
            let drill_of = |t: &[(u32, usize)], dims: &[usize]| -> Drill { t.iter().zip(dims).rev().find(|(&(e, _), &d)| ex.hier[d].is_consolidated(e)).map(|(&(e, _), &d)| (d, e)) };
            let x_labels: Vec<String> = cols.iter().map(|c| if p.cols.is_empty() { "value".into() } else { tuple_name(c, &p.cols) }).collect();
            let x_drills: Vec<Drill> = cols.iter().map(|c| drill_of(c, &p.cols)).collect();

            // One row dimension splits into one series per panel; with more,
            // the outermost is the panels and the inner ones their series.
            let single = split && p.rows.len() == 1;
            let skip = usize::from(split && !single);
            let groups: Vec<(Option<u32>, Vec<Tuple>)> = if split { group_rows(&rows).into_iter().map(|(e, r)| (Some(e), r)).collect() } else { vec![(None, rows.clone())] };
            let more = if split { groups.len().saturating_sub(MAX_PANELS) } else { rows.len().saturating_sub(MAX_SERIES) };
            let cut = split && groups.iter().any(|(_, r)| r.len() > MAX_SERIES);
            let mut slots: Vec<Vec<u32>> = Vec::new();
            let panels: Vec<Panel> = groups
                .iter()
                .take(if split { MAX_PANELS } else { 1 })
                .map(|(element, group)| {
                    let series = group
                        .iter()
                        .take(MAX_SERIES)
                        .map(|r| {
                            let key: Vec<u32> = if single { Vec::new() } else { r[skip..].iter().map(|&(e, _)| e).collect() };
                            let slot = slots.iter().position(|s| *s == key).unwrap_or_else(|| {
                                slots.push(key);
                                slots.len() - 1
                            });
                            Series {
                                name: tuple_name(&r[skip..], &p.rows[skip..]),
                                slot,
                                drill: drill_of(&r[skip..], &p.rows[skip..]),
                                values: cols.iter().map(|c| match ex.cells.get(&p.path(r, c)) {
                                    Some(Some(Value::Number(n))) => Some(*n),
                                    _ => None,
                                }).collect(),
                            }
                        })
                        .collect();
                    let (title, drill) = match (*element, p.rows.first()) {
                        (Some(e), Some(&d)) => (ex.hier[d].name(e).to_string(), ex.hier[d].is_consolidated(e).then_some((d, e))),
                        _ => (String::new(), None),
                    };
                    Panel { title, drill, series }
                })
                .collect();
            if panels.iter().all(|panel| panel.series.is_empty()) || x_labels.is_empty() {
                return Some(view! { <p class="muted">"Nothing to chart in this view."</p> }.into_any());
            }
            let marks: usize = panels.iter().map(|panel| panel.series.len() * x_labels.len()).sum();
            if split && marks > MAX_PANEL_MARKS {
                return Some(view! { <p class="muted">{format!("Too many points for panels ({marks}) -- close some opened elements, or turn panels off.")}</p> }.into_any());
            }
            let extent: Vec<f64> = panels.iter().flat_map(|panel| extent(&panel.series, show == Show::Stacked)).collect();
            let (ticks, range) = y_axis(extent.iter(), show != Show::Line);
            let draw = |panel: &Panel, size: Size| {
                let plot = Plot { series: &panel.series, x_labels: &x_labels, x_drills: &x_drills, size, ticks: &ticks, range, ctx };
                match show {
                    Show::Line => line_chart(&plot),
                    _ => bar_chart(&plot, show == Show::Stacked),
                }
            };
            let charts = if split {
                let cells = panels
                    .iter()
                    .map(|panel| {
                        let drill = panel.drill;
                        view! {
                            <div class="chart">
                                <button class="panel-title" class:drillable=drill.is_some() on:click=move |_| open(ctx, drill)>{panel.title.clone()}</button>
                                {draw(panel, SMALL)}
                            </div>
                        }
                    })
                    .collect_view();
                view! { <div class="panels">{cells}</div> }.into_any()
            } else {
                view! { <div class="chart">{draw(&panels[0], FULL)}</div> }.into_any()
            };
            // Each series once: panels repeat them, in the same colours.
            let mut seen = Vec::new();
            let legend = panels
                .iter()
                .flat_map(|panel| &panel.series)
                .filter(|s| !single && !seen.contains(&s.slot) && {
                    seen.push(s.slot);
                    true
                })
                .map(|s| {
                    let drill = s.drill;
                    let name = s.name.clone();
                    view! {
                        <button class="legend-item" class:drillable=drill.is_some() title={if drill.is_some() { "open / close this element" } else { "" }}
                            on:click=move |_| open(ctx, drill)>
                            <span class="swatch" style=format!("background: {}", PALETTE[s.slot % PALETTE.len()])></span>{name}
                        </button>
                    }
                })
                .collect_view();
            let note = (more > 0).then(|| if split { format!("Showing the first {MAX_PANELS} panels; {more} more are in the table.") } else { format!("Showing the first {MAX_SERIES} rows; {more} more are in the table.") });
            Some(
                view! {
                    {charts}
                    <div class="legend">{legend}</div>
                    {note.map(|note| view! { <p class="muted small">{note}</p> })}
                    {cut.then(|| view! { <p class="muted small">{format!("Each panel shows its first {MAX_SERIES} rows.")}</p> })}
                }
                .into_any(),
            )
        })
    }
}

/// The values a chart's y axis must cover.
fn extent(series: &[Series], stacked: bool) -> Vec<f64> {
    let raw: Vec<Vec<Option<f64>>> = series.iter().map(|s| s.values.clone()).collect();
    if stacked {
        stack(&raw).into_iter().flatten().flat_map(|(a, b)| [a, b]).collect()
    } else {
        raw.into_iter().flatten().flatten().collect()
    }
}

/// The y range and ticks for `values`: from 0 for bars (`zero`), the
/// data's own range for lines (a CO2 curve from 316 to 427 ppm would be
/// flat from 0).
fn y_axis<'a>(values: impl IntoIterator<Item = &'a f64>, zero: bool) -> (Vec<f64>, (f64, f64)) {
    let values: Vec<f64> = values.into_iter().copied().collect();
    let (lo, hi) = if zero {
        value_range(values.iter().copied())
    } else {
        values.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)))
    };
    let (lo, hi) = if lo.is_finite() { (lo, hi) } else { (0.0, 1.0) };
    let ticks = nice_ticks(lo, hi, 6);
    let range = (*ticks.first().unwrap_or(&lo), *ticks.last().unwrap_or(&hi));
    (ticks, range)
}

fn y_px(v: f64, plot: &Plot) -> f64 {
    scale(v, plot.range, (plot.size.h - BOTTOM, TOP))
}

/// Grid lines, y tick labels and x labels (thinned to about one per 60 units).
fn frame(plot: &Plot, x_at: impl Fn(usize) -> f64) -> impl IntoView {
    let Size { w, h } = plot.size;
    let grid = plot
        .ticks
        .iter()
        .map(|&t| {
            let y = y_px(t, plot);
            view! {
                <line x1=LEFT.to_string() x2=(w - RIGHT).to_string() y1=y.to_string() y2=y.to_string() class={if t == 0.0 { "axis" } else { "grid" }}/>
                <text x=(LEFT - 8.0).to_string() y=(y + 4.0).to_string() class="tick" text-anchor="end">{format_number(t)}</text>
            }
        })
        .collect_view();
    let every = plot.x_labels.len().div_ceil((w / 60.0) as usize).max(1);
    let labels = plot
        .x_labels
        .iter()
        .enumerate()
        .filter(|(i, _)| i % every == 0)
        .map(|(i, label)| {
            let (x, y) = (x_at(i), h - BOTTOM + 14.0);
            let short: String = if label.chars().count() > 18 { label.chars().take(17).collect::<String>() + "…" } else { label.clone() };
            view! { <text x=x.to_string() y=y.to_string() class="tick" text-anchor="end" transform=format!("rotate(-35 {x} {y})")>{short}</text> }
        })
        .collect_view();
    view! { {grid}{labels} }
}

fn line_chart(plot: &Plot) -> AnyView {
    let Size { w, h } = plot.size;
    let n = plot.x_labels.len();
    let x_at = move |i: usize| if n <= 1 { (LEFT + w - RIGHT) / 2.0 } else { LEFT + i as f64 * (w - RIGHT - LEFT) / (n - 1) as f64 };
    let ctx = plot.ctx;
    let lines = plot
        .series
        .iter()
        .map(|s| {
            let color = PALETTE[s.slot % PALETTE.len()];
            // A gap in the data breaks the line.
            let mut segments: Vec<Vec<(f64, f64)>> = vec![Vec::new()];
            for (i, v) in s.values.iter().enumerate() {
                match v {
                    Some(v) => segments.last_mut().expect("never empty").push((x_at(i), y_px(*v, plot))),
                    None => segments.push(Vec::new()),
                }
            }
            let paths = segments
                .into_iter()
                .filter(|seg| !seg.is_empty())
                .map(|seg| {
                    let points = seg.iter().map(|(x, y)| format!("{x:.1},{y:.1}")).collect::<Vec<_>>().join(" ");
                    view! { <polyline points=points fill="none" stroke=color stroke-width="2.2"/> }
                })
                .collect_view();
            let dots = s
                .values
                .iter()
                .enumerate()
                .filter_map(|(i, v)| v.map(|v| (i, v)))
                .map(|(i, v)| {
                    let tip = format!("{} · {}: {}", s.name, plot.x_labels[i], format_number(v));
                    let drill = plot.x_drills[i].or(s.drill);
                    view! { <circle class:drillable=drill.is_some() on:click=move |_| open(ctx, drill) cx=x_at(i).to_string() cy=y_px(v, plot).to_string() r={if n > 60 { "1.6" } else { "3" }} fill=color><title>{tip}</title></circle> }
                })
                .collect_view();
            view! { <g>{paths}{dots}</g> }
        })
        .collect_view();
    view! {
        <svg viewBox=format!("0 0 {w} {h}") class="chart-svg">
            {frame(plot, x_at)}
            {lines}
        </svg>
    }
    .into_any()
}

fn bar_chart(plot: &Plot, stacked: bool) -> AnyView {
    let Size { w, h } = plot.size;
    let raw: Vec<Vec<Option<f64>>> = plot.series.iter().map(|s| s.values.clone()).collect();
    let stacks = stack(&raw);
    let n = plot.x_labels.len().max(1);
    let group_w = (w - RIGHT - LEFT) / n as f64;
    let x_at = move |i: usize| LEFT + (i as f64 + 0.5) * group_w;
    let bar_w = if stacked { group_w * 0.7 } else { group_w * 0.8 / plot.series.len() as f64 };
    let ctx = plot.ctx;
    let bars = plot
        .series
        .iter()
        .enumerate()
        .map(|(si, s)| {
            let color = PALETTE[s.slot % PALETTE.len()];
            (0..plot.x_labels.len())
                .filter_map(|i| {
                    let v = s.values.get(i).copied().flatten()?;
                    let (bottom, top) = if stacked { stacks[si][i] } else { (v.min(0.0), v.max(0.0)) };
                    let x = if stacked { x_at(i) - bar_w / 2.0 } else { LEFT + i as f64 * group_w + group_w * 0.1 + si as f64 * bar_w };
                    let (y1, y2) = (y_px(top, plot), y_px(bottom, plot));
                    let tip = format!("{} · {}: {}", s.name, plot.x_labels[i], format_number(v));
                    let drill = plot.x_drills[i].or(s.drill);
                    Some(view! {
                        <rect class:drillable=drill.is_some() on:click=move |_| open(ctx, drill) x=format!("{x:.1}") y=format!("{y1:.1}") width=format!("{:.1}", bar_w.max(0.5)) height=format!("{:.1}", (y2 - y1).max(0.0)) fill=color><title>{tip}</title></rect>
                    })
                })
                .collect_view()
        })
        .collect_view();
    view! {
        <svg viewBox=format!("0 0 {w} {h}") class="chart-svg">
            {frame(plot, x_at)}
            {bars}
        </svg>
    }
    .into_any()
}
