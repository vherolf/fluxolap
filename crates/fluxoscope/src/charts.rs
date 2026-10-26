//! Charts of the current view as SVG, drawn from Rust: the rows are the
//! series, the columns the x axis.

use crate::model::chart::{nice_ticks, scale, stack, value_range};
use crate::model::format::format_number;
use crate::model::pivot::{visible_tuples, Tuple, MAX_VIEW_CELLS};
use crate::state::{Ctx, Loaded, Show};
use fluxolap_api_types::Value;
use leptos::prelude::*;

const PALETTE: [&str; 12] = ["#3884ff", "#f5a524", "#17c964", "#f31260", "#9353d3", "#06b7db", "#ff7a45", "#7fb800", "#e04dbd", "#5c7cfa", "#b08900", "#8c8c8c"];
const MAX_SERIES: usize = 12;
/// Most points along the x axis (the columns).
const MAX_POINTS: usize = 2_000;
const W: f64 = 960.0;
const H: f64 = 420.0;
const LEFT: f64 = 84.0;
const RIGHT: f64 = 20.0;
const TOP: f64 = 16.0;
const BOTTOM: f64 = 72.0;

struct Series {
    name: String,
    /// The row's innermost consolidated element (clicking the legend drills into it).
    drill: Option<(usize, u32)>,
    values: Vec<Option<f64>>,
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
            if show != Show::Line {
                // Bars add up: a total drawn next to (or stacked on) its own
                // opened children would count them twice, so opened totals go.
                let opened = |t: &Tuple, dims: &[usize]| t.iter().zip(dims).any(|(&(e, _), &d)| p.expanded[d].contains(&e) && ex.hier[d].is_consolidated(e));
                rows.retain(|r| !opened(r, &p.rows));
                cols.retain(|c| !opened(c, &p.cols));
            }
            let tuple_name = |t: &Tuple, dims: &[usize]| t.iter().zip(dims).map(|(&(e, _), &d)| ex.hier[d].name(e)).collect::<Vec<_>>().join(" · ");
            let x_labels: Vec<String> = cols.iter().map(|c| if p.cols.is_empty() { "value".into() } else { tuple_name(c, &p.cols) }).collect();
            let series: Vec<Series> = rows
                .iter()
                .take(MAX_SERIES)
                .map(|r| Series {
                    name: tuple_name(r, &p.rows),
                    drill: r.iter().zip(&p.rows).rev().find(|(&(e, _), &d)| ex.hier[d].is_consolidated(e)).map(|(&(e, _), &d)| (d, e)),
                    values: cols.iter().map(|c| match ex.cells.get(&p.path(r, c)) {
                        Some(Some(Value::Number(n))) => Some(*n),
                        _ => None,
                    }).collect(),
                })
                .collect();
            if series.is_empty() || x_labels.is_empty() {
                return Some(view! { <p class="muted">"Nothing to chart in this view."</p> }.into_any());
            }
            let more = rows.len().saturating_sub(MAX_SERIES);
            let svg = match show {
                Show::Line => line_chart(&series, &x_labels),
                _ => bar_chart(&series, &x_labels, show == Show::Stacked),
            };
            let legend = series
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let drill = s.drill;
                    let name = s.name.clone();
                    view! {
                        <button class="legend-item" class:drillable=drill.is_some() title={if drill.is_some() { "open / close this element" } else { "" }}
                            on:click=move |_| if let Some((d, e)) = drill { ctx.change(move |p, h| p.toggle(d, e, &h[d])) }>
                            <span class="swatch" style=format!("background: {}", PALETTE[i % PALETTE.len()])></span>{name}
                        </button>
                    }
                })
                .collect_view();
            Some(
                view! {
                    <div class="chart">{svg}</div>
                    <div class="legend">{legend}</div>
                    {(more > 0).then(|| view! { <p class="muted small">{format!("Showing the first {MAX_SERIES} rows; {more} more are in the table.")}</p> })}
                }
                .into_any(),
            )
        })
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

fn y_px(v: f64, range: (f64, f64)) -> f64 {
    scale(v, range, (H - BOTTOM, TOP))
}

/// Grid lines, y tick labels and x labels (thinned to about 16).
fn frame(ticks: &[f64], range: (f64, f64), x_labels: &[String], x_at: impl Fn(usize) -> f64) -> impl IntoView {
    let grid = ticks
        .iter()
        .map(|&t| {
            let y = y_px(t, range);
            view! {
                <line x1=LEFT.to_string() x2=(W - RIGHT).to_string() y1=y.to_string() y2=y.to_string() class={if t == 0.0 { "axis" } else { "grid" }}/>
                <text x=(LEFT - 8.0).to_string() y=(y + 4.0).to_string() class="tick" text-anchor="end">{format_number(t)}</text>
            }
        })
        .collect_view();
    let every = x_labels.len().div_ceil(16).max(1);
    let labels = x_labels
        .iter()
        .enumerate()
        .filter(|(i, _)| i % every == 0)
        .map(|(i, label)| {
            let (x, y) = (x_at(i), H - BOTTOM + 14.0);
            let short: String = if label.chars().count() > 18 { label.chars().take(17).collect::<String>() + "…" } else { label.clone() };
            view! { <text x=x.to_string() y=y.to_string() class="tick" text-anchor="end" transform=format!("rotate(-35 {x} {y})")>{short}</text> }
        })
        .collect_view();
    view! { {grid}{labels} }
}

fn line_chart(series: &[Series], x_labels: &[String]) -> AnyView {
    let (ticks, range) = y_axis(series.iter().flat_map(|s| s.values.iter().flatten()), false);
    let n = x_labels.len();
    let x_at = move |i: usize| if n <= 1 { (LEFT + W - RIGHT) / 2.0 } else { LEFT + i as f64 * (W - RIGHT - LEFT) / (n - 1) as f64 };
    let lines = series
        .iter()
        .enumerate()
        .map(|(si, s)| {
            let color = PALETTE[si % PALETTE.len()];
            // A gap in the data breaks the line.
            let mut segments: Vec<Vec<(f64, f64)>> = vec![Vec::new()];
            for (i, v) in s.values.iter().enumerate() {
                match v {
                    Some(v) => segments.last_mut().expect("never empty").push((x_at(i), y_px(*v, range))),
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
                    let tip = format!("{} · {}: {}", s.name, x_labels[i], format_number(v));
                    view! { <circle cx=x_at(i).to_string() cy=y_px(v, range).to_string() r={if n > 60 { "1.6" } else { "3" }} fill=color><title>{tip}</title></circle> }
                })
                .collect_view();
            view! { <g>{paths}{dots}</g> }
        })
        .collect_view();
    view! {
        <svg viewBox=format!("0 0 {W} {H}") class="chart-svg">
            {frame(&ticks, range, x_labels, x_at)}
            {lines}
        </svg>
    }
    .into_any()
}

fn bar_chart(series: &[Series], x_labels: &[String], stacked: bool) -> AnyView {
    let raw: Vec<Vec<Option<f64>>> = series.iter().map(|s| s.values.clone()).collect();
    let stacks = stack(&raw);
    let extent: Vec<f64> = if stacked { stacks.iter().flatten().flat_map(|&(a, b)| [a, b]).collect() } else { raw.iter().flatten().flatten().copied().collect() };
    let (ticks, range) = y_axis(extent.iter(), true);
    let n = x_labels.len().max(1);
    let group_w = (W - RIGHT - LEFT) / n as f64;
    let x_at = move |i: usize| LEFT + (i as f64 + 0.5) * group_w;
    let bar_w = if stacked { group_w * 0.7 } else { group_w * 0.8 / series.len() as f64 };
    let bars = series
        .iter()
        .enumerate()
        .map(|(si, s)| {
            let color = PALETTE[si % PALETTE.len()];
            (0..x_labels.len())
                .filter_map(|i| {
                    let v = s.values.get(i).copied().flatten()?;
                    let (bottom, top) = if stacked { stacks[si][i] } else { (v.min(0.0), v.max(0.0)) };
                    let x = if stacked { x_at(i) - bar_w / 2.0 } else { LEFT + i as f64 * group_w + group_w * 0.1 + si as f64 * bar_w };
                    let (y1, y2) = (y_px(top, range), y_px(bottom, range));
                    let tip = format!("{} · {}: {}", s.name, x_labels[i], format_number(v));
                    Some(view! {
                        <rect x=format!("{x:.1}") y=format!("{y1:.1}") width=format!("{:.1}", bar_w.max(0.5)) height=format!("{:.1}", (y2 - y1).max(0.0)) fill=color><title>{tip}</title></rect>
                    })
                })
                .collect_view()
        })
        .collect_view();
    view! {
        <svg viewBox=format!("0 0 {W} {H}") class="chart-svg">
            {frame(&ticks, range, x_labels, x_at)}
            {bars}
        </svg>
    }
    .into_any()
}
