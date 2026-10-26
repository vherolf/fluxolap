//! The cube explorer: axes, the dimension panel, and the pivot table or a
//! chart of the current view.

use crate::charts::ChartView;
use crate::model::format::{decimals_of, format_aligned, group};
use crate::model::hierarchy::Hierarchy;
use crate::model::pivot::{visible_tuples, Tuple, MAX_VIEW_CELLS};
use crate::state::{Ctx, Explorer, Loaded, Show};
use fluxolap_api_types::Value;
use leptos::prelude::*;

#[component]
pub fn ExplorerView() -> impl IntoView {
    view! {
        <section class="explorer">
            <TitleBar/>
            <Toolbar/>
            <DimensionPanel/>
            <Content/>
        </section>
    }
}

fn with_explorer<T>(f: impl FnOnce(&Explorer) -> T) -> Option<T> {
    expect_context::<Ctx>().explorer.with(|e| e.as_ref().map(f))
}

#[component]
fn TitleBar() -> impl IntoView {
    move || {
        with_explorer(|ex| {
            let dims = if ex.dims.len() <= 6 { ex.dims.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(" × ") } else { format!("{} dimensions", ex.dims.len()) };
            view! {
                <div class="title">
                    <h1>{ex.cube.name.clone()}</h1>
                    <span class="muted">{dims}" · "{group(ex.cube.cell_count as f64)}" stored cells"</span>
                    {ex.loading.then(|| view! { <span class="spinner">"loading …"</span> })}
                </div>
                {ex.error.clone().map(|e| view! { <p class="error">{e}</p> })}
            }
        })
    }
}

#[component]
fn Toolbar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || {
        with_explorer(|ex| {
            let Some(p) = ex.pivot.clone() else { return ().into_any() };
            let chip = |d: usize, on_rows: bool| {
                let name = ex.dims[d].name.clone();
                view! {
                    <span class="chip">
                        {name}
                        <button title={if on_rows { "move to columns" } else { "move to rows" }} on:click=move |_| ctx.change(move |p, _| p.move_to_axis(d, !on_rows))>"⇄"</button>
                        <button title="make it a filter" on:click=move |_| ctx.change(move |p, _| p.move_to_filters(d))>"×"</button>
                    </span>
                }
            };
            let rows = p.rows.iter().map(|&d| chip(d, true)).collect_view();
            let cols = p.cols.iter().map(|&d| chip(d, false)).collect_view();
            // Filters: the ones moved off their top element, the rest counted.
            let changed: Vec<usize> = p.filters().into_iter().filter(|&d| p.fixed[d] != p.defaults[d]).collect();
            let unchanged = p.filters().len() - changed.len();
            let filters = changed
                .iter()
                .map(|&d| {
                    let label = format!("{}: {}", ex.dims[d].name, ex.hier[d].name(p.fixed[d]));
                    let default = p.defaults[d];
                    view! {
                        <span class="chip filter">
                            {label}
                            <button title="back to the top element" on:click=move |_| ctx.change(move |p, _| p.fixed[d] = default)>"×"</button>
                        </span>
                    }
                })
                .collect_view();
            let show = ex.show;
            let seg = move |s: Show, label: &'static str| {
                view! { <button class:on=move || show == s on:click=move |_| ctx.set_display(move |ex| ex.show = s)>{label}</button> }
            };
            let hide_empty = p.hide_empty;
            let heatmap = ex.heatmap;
            view! {
                <div class="toolbar">
                    <div class="axes">
                        <span class="axis-label">"Rows"</span>{rows}
                        <button class="plain" title="swap rows and columns" on:click=move |_| ctx.change(|p, _| p.swap_axes())>"⇅"</button>
                        <span class="axis-label">"Columns"</span>{cols}
                    </div>
                    <div class="axes">
                        <span class="axis-label">"Filters"</span>
                        {filters}
                        <span class="muted small">{if unchanged > 0 { format!("{unchanged} at their top element") } else { String::new() }}</span>
                    </div>
                    <div class="controls">
                        <label><input type="checkbox" prop:checked=hide_empty on:change=move |_| ctx.change(|p, _| p.hide_empty = !p.hide_empty)/>"hide empty"</label>
                        <label><input type="checkbox" prop:checked=heatmap on:change=move |_| ctx.set_display(|ex| ex.heatmap = !ex.heatmap)/>"heatmap"</label>
                        <span class="segmented">
                            {seg(Show::Table, "Table")}{seg(Show::Line, "Line")}{seg(Show::Bar, "Bar")}{seg(Show::Stacked, "Stacked")}
                        </span>
                    </div>
                </div>
            }
            .into_any()
        })
    }
}

/// Every dimension, searchable: put it on rows or columns, or pick its
/// filter element. Built for cubes with a hundred dimensions and more.
#[component]
fn DimensionPanel() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let query = RwSignal::new(String::new());
    view! {
        <details class="dims" open=move || with_explorer(|ex| ex.dims.len() <= 8).unwrap_or(true)>
            <summary>{move || with_explorer(|ex| format!("Dimensions ({})", ex.dims.len()))}</summary>
            <input class="search" placeholder="search dimensions" prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev))/>
            <table class="dim-list">
                {move || {
                    let needle = query.get().to_lowercase();
                    with_explorer(|ex| {
                        let Some(p) = ex.pivot.clone() else { return ().into_any() };
                        (0..ex.dims.len())
                            .filter(|&d| needle.is_empty() || ex.dims[d].name.to_lowercase().contains(&needle))
                            .map(|d| {
                                let role = if p.rows.contains(&d) { "rows" } else if p.cols.contains(&d) { "columns" } else { "filter" };
                                let filter = role == "filter";
                                let element = filter_element(ex, d, p.fixed[d]);
                                view! {
                                    <tr>
                                        <td class="dim-name">{ex.dims[d].name.clone()}</td>
                                        <td class="muted small">{group(ex.dims[d].element_count as f64)}" elements"</td>
                                        <td>
                                            <button class:on=role == "rows" on:click=move |_| ctx.change(move |p, _| p.move_to_axis(d, true))>"rows"</button>
                                            <button class:on=role == "columns" on:click=move |_| ctx.change(move |p, _| p.move_to_axis(d, false))>"columns"</button>
                                            <button class:on=filter on:click=move |_| ctx.change(move |p, _| p.move_to_filters(d))>"filter"</button>
                                        </td>
                                        <td>{filter.then_some(element)}</td>
                                    </tr>
                                }
                            })
                            .collect_view()
                            .into_any()
                    })
                }}
            </table>
        </details>
    }
}

/// A filter's element: a dropdown once every element is known, otherwise
/// the current one and a button to load the rest.
fn filter_element(ex: &Explorer, d: usize, current: u32) -> AnyView {
    let ctx = expect_context::<Ctx>();
    if ex.loaded[d] != Loaded::All {
        let name = ex.hier[d].name(current).to_string();
        return view! {
            <span>{name}" "<button class="plain" on:click=move |_| ctx.need_all_elements(d)>"change …"</button></span>
        }
        .into_any();
    }
    let h = &ex.hier[d];
    let options = h
        .all()
        .into_iter()
        .map(|e| {
            let label = if h.is_consolidated(e) { format!("▸ {}", h.name(e)) } else { h.name(e).to_string() };
            view! { <option value=e.to_string() selected=e == current>{label}</option> }
        })
        .collect_view();
    view! {
        <select on:change=move |ev| {
            if let Ok(e) = event_target_value(&ev).parse::<u32>() {
                ctx.change(move |p, _| p.fixed[d] = e);
            }
        }>{options}</select>
    }
    .into_any()
}

#[component]
fn Content() -> impl IntoView {
    let show = Memo::new(move |_| with_explorer(|ex| ex.show).unwrap_or_default());
    move || match show.get() {
        Show::Table => view! { <PivotTable/> }.into_any(),
        other => view! { <ChartView show=other/> }.into_any(),
    }
}

/// A header label (with a drill toggle on consolidated elements).
fn label_cell(h: &Hierarchy, d: usize, (element, depth): (u32, usize), opened: bool, header: bool) -> AnyView {
    let ctx = expect_context::<Ctx>();
    let name = h.name(element).to_string();
    let indent = format!("padding-left: {}em", depth as f32 * 1.1 + 0.4);
    if !h.is_consolidated(element) {
        let marker = "";
        return if header { view! { <th class="col-head">{marker}{name}</th> }.into_any() } else { view! { <th class="row-head" style=indent>{name}</th> }.into_any() };
    }
    let marker = if opened { "▾ " } else { "▸ " };
    let toggle = move |_| ctx.change(move |p, h| p.toggle(d, element, &h[d]));
    if header {
        view! { <th class="col-head consolidated" on:click=toggle>{marker}{name}</th> }.into_any()
    } else {
        view! { <th class="row-head consolidated" style=indent on:click=toggle>{marker}{name}</th> }.into_any()
    }
}

/// Whether tuple `i` starts a new group at `level` (so its label is shown).
fn starts_group(tuples: &[Tuple], i: usize, level: usize) -> bool {
    i == 0 || tuples[i][..=level] != tuples[i - 1][..=level]
}

/// Height of one table row in px -- fixed (see `.pivot tbody` in style.css)
/// so that only the rows in view need to exist.
const ROW_HEIGHT: f64 = 32.0;
/// Rows drawn above and below the visible ones, for smooth scrolling.
const OVERSCAN: usize = 20;
/// Most columns a table draws (every column of a drawn row exists).
const MAX_COLUMNS: usize = 500;

/// What the table body needs to draw any window of its rows.
struct TableData {
    rows: Vec<Tuple>,
    cols: Vec<Tuple>,
    decimals: Vec<usize>,
    /// The range of the numbers shown, for the heatmap.
    range: (f64, f64),
}

#[component]
fn PivotTable() -> impl IntoView {
    // The table's scroll position; only the rows around it are drawn.
    let scroll = RwSignal::new(0.0_f64);
    let viewport = web_sys::window().and_then(|w| w.inner_height().ok()).and_then(|h| h.as_f64()).unwrap_or(900.0);
    move || {
        with_explorer(|ex| {
            let Some(p) = ex.pivot.as_ref() else { return view! { <p class="muted">"loading dimensions …"</p> }.into_any() };
            if !p.rows.iter().chain(&p.cols).all(|&d| ex.loaded[d] == Loaded::All) {
                return view! { <p class="muted">"loading elements …"</p> }.into_any();
            }
            let (rows, cols) = visible_tuples(p, &ex.hier, &ex.cells, ex.complete);
            if !ex.complete && rows.len().saturating_mul(cols.len()) > MAX_VIEW_CELLS {
                return view! { <p class="muted">"reading the cells …"</p> }.into_any();
            }
            if cols.len() > MAX_COLUMNS {
                let text = format!("This view has {} columns -- close some opened elements or swap rows and columns (at most {MAX_COLUMNS}).", group(cols.len() as f64));
                return view! { <p class="error">{text}</p> }.into_any();
            }
            if rows.is_empty() || cols.is_empty() {
                return view! { <p class="muted">"No values in this view. Turn off “hide empty”, or pick other filters."</p> }.into_any();
            }
            let value = |r: &Tuple, c: &Tuple| ex.cells.get(&p.path(r, c)).cloned();
            let decimals: Vec<usize> = cols
                .iter()
                .map(|c| rows.iter().filter_map(|r| match value(r, c) { Some(Some(Value::Number(n))) => Some(decimals_of(n)), _ => None }).max().unwrap_or(0))
                .collect();
            let range = ex.cells.values().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| match v {
                Some(Value::Number(n)) => (lo.min(*n), hi.max(*n)),
                _ => (lo, hi),
            });

            let col_levels = p.cols.len().max(1);
            let header_rows = (0..col_levels)
                .map(|l| {
                    let corner = (0..p.rows.len())
                        .map(|i| {
                            let text = if l + 1 == col_levels { ex.dims[p.rows[i]].name.clone() } else { String::new() };
                            view! { <th class="corner">{text}</th> }
                        })
                        .collect_view();
                    let heads = (0..cols.len())
                        .map(|j| {
                            if p.cols.is_empty() {
                                return view! { <th class="col-head">"value"</th> }.into_any();
                            }
                            if !starts_group(&cols, j, l) {
                                return view! { <th class="col-head cont"></th> }.into_any();
                            }
                            let d = p.cols[l];
                            label_cell(&ex.hier[d], d, cols[j][l], p.expanded[d].contains(&cols[j][l].0), true)
                        })
                        .collect_view();
                    view! { <tr>{corner}{heads}</tr> }
                })
                .collect_view();

            let summary = format!("{} rows × {} columns", group(rows.len() as f64), group(cols.len() as f64));
            let data = std::sync::Arc::new(TableData { rows, cols, decimals, range });
            let width = data.cols.len() + p.rows.len();
            let body = move || table_rows(&data, scroll.get(), viewport, width);
            let wrap = NodeRef::<leptos::html::Div>::new();
            // Redrawn (new data): back to where the reader was.
            wrap.on_load(move |el| el.set_scroll_top(scroll.get_untracked() as i32));
            view! {
                <div class="table-wrap" node_ref=wrap on:scroll=move |ev| scroll.set(event_target::<web_sys::Element>(&ev).scroll_top() as f64)>
                    <table class="pivot">
                        <thead>{header_rows}</thead>
                        <tbody>{body}</tbody>
                    </table>
                </div>
                <p class="muted small">{summary}</p>
            }
            .into_any()
        })
    }
}

/// The body rows around scroll position `top`, with spacer rows standing
/// in for the ones above and below, so the scrollbar spans all of them.
fn table_rows(data: &TableData, top: f64, viewport: f64, width: usize) -> AnyView {
    let n = data.rows.len();
    let first = ((top / ROW_HEIGHT) as usize).saturating_sub(OVERSCAN).min(n);
    let last = (((top + viewport) / ROW_HEIGHT) as usize + OVERSCAN).min(n);
    let spacer = |rows: usize| (rows > 0).then(|| view! { <tr class="spacer" style=format!("height: {}px", rows as f64 * ROW_HEIGHT)><td colspan=width.to_string()></td></tr> });
    let drawn = with_explorer(|ex| {
        let Some(p) = ex.pivot.as_ref() else { return Vec::new() };
        let (lo, hi) = data.range;
        let heat = |v: f64| {
            if !ex.heatmap || hi <= lo {
                return String::new();
            }
            format!("background: rgba(56, 132, 255, {:.3})", 0.06 + 0.55 * (v - lo) / (hi - lo))
        };
        (first..last)
            .map(|i| {
                let r = &data.rows[i];
                let heads = (0..p.rows.len())
                    .map(|l| {
                        // The first drawn row shows its labels too, so a scrolled table keeps its context.
                        if i != first && !starts_group(&data.rows, i, l) {
                            return view! { <th class="row-head cont"></th> }.into_any();
                        }
                        let d = p.rows[l];
                        label_cell(&ex.hier[d], d, r[l], p.expanded[d].contains(&r[l].0), false)
                    })
                    .collect_view();
                let consolidated_row = r.iter().zip(&p.rows).any(|(&(e, _), &d)| ex.hier[d].is_consolidated(e));
                let cells = data
                    .cols
                    .iter()
                    .enumerate()
                    .map(|(j, c)| match ex.cells.get(&p.path(r, c)).cloned() {
                        None if ex.complete => view! { <td class="num empty">"·"</td> }.into_any(),
                        None => view! { <td class="num pending">"…"</td> }.into_any(),
                        Some(None) => view! { <td class="num empty">"·"</td> }.into_any(),
                        Some(Some(Value::Number(n))) => view! { <td class="num" style=heat(n)>{format_aligned(n, data.decimals[j])}</td> }.into_any(),
                        Some(Some(Value::Text(t))) => view! { <td class="text">{t}</td> }.into_any(),
                    })
                    .collect_view();
                view! { <tr class:total=consolidated_row>{heads}{cells}</tr> }.into_any()
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    view! { {spacer(first)}{drawn}{spacer(n - last)} }.into_any()
}
