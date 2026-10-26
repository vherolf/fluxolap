//! Drawing. Reads the `App` state; the only thing it writes back is what
//! the layout decides -- the grid's scroll position and how many rows and
//! columns fit -- which the next cell request depends on.

use crate::app::{decimals_of, format_aligned, format_number, group, App, Browser, DimPurpose, DimView, Grid, InputKind, Item, Mode, Pane, PickerEntry, Popup, View};
use crate::stats::{self, bytes, StatsState};
use fluxolap_api_types::{ObjectType, Value};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row, Sparkline, Table, TableState, Wrap};
use ratatui::Frame;

const ACCENT: Color = Color::Cyan;
const CONSOLIDATED: Color = Color::Yellow;
const TEXT_VALUE: Color = Color::Green;
const MUTED: Color = Color::DarkGray;
const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, status, hints] = Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)]).areas(frame.area());
    draw_header(frame, header, app);
    if let Some(error) = app.offline.clone().filter(|_| app.browser.databases.is_empty()) {
        draw_offline(frame, body, &app.server_url, &error);
    } else {
        match &mut app.view {
            View::Browser => draw_browser(frame, body, &app.browser),
            View::Dimension(view) => draw_dimension(frame, body, view),
            View::Grid(grid) => draw_grid(frame, body, grid),
            View::Stats(_) => draw_stats(frame, body, app),
        }
    }
    if let Some((text, is_error)) = app.status_line() {
        let style = if is_error { Style::new().fg(Color::Red) } else { Style::new().fg(Color::Green) };
        frame.render_widget(Paragraph::new(Span::styled(format!(" {text}"), style)), status);
    }
    frame.render_widget(Paragraph::new(hint_line(app)), hints);
    if let Some(popup) = &app.popup {
        draw_popup(frame, popup, app);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(" fluxoterm ", Style::new().fg(Color::Black).bg(ACCENT).bold()), Span::raw(" ")];
    spans.push(Span::styled(app.server_url.clone(), Style::new().bold()));
    match (&app.server, &app.offline) {
        (_, Some(_)) => spans.push(Span::styled("  offline", Style::new().fg(Color::Red))),
        (Some(s), None) => spans.push(Span::styled(format!("  {} {}", s.name, s.version), Style::new().fg(MUTED))),
        (None, None) => {}
    }
    let [left, memory, right] = Layout::horizontal([Constraint::Min(10), Constraint::Length(26), Constraint::Length(12)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), left);
    if let Some(line) = header_memory(&app.stats).filter(|_| app.offline.is_none()) {
        frame.render_widget(Paragraph::new(line).alignment(Alignment::Right), memory);
    }
    if app.busy > 0 {
        let tick = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()) / 120) as usize;
        frame.render_widget(Paragraph::new(format!("{} loading ", SPINNER[tick % 4])).fg(ACCENT).alignment(Alignment::Right), right);
    }
}

/// `mem 812.0 MB / 2.0 GB` in the header: the server's resident memory,
/// yellow from 75 % and red from 90 % of its limit (or the machine's memory).
fn header_memory(stats: &StatsState) -> Option<Line<'static>> {
    let (rss, of) = stats.header_memory()?;
    let mut spans = vec![Span::styled("mem ", Style::new().fg(MUTED)), Span::styled(bytes(rss), Style::new().fg(usage_color(rss, of)).bold())];
    if let Some(of) = of {
        spans.push(Span::styled(format!(" / {}", bytes(of)), Style::new().fg(MUTED)));
    }
    Some(Line::from(spans))
}

fn usage_color(used: u64, of: Option<u64>) -> Color {
    match of.filter(|&of| of > 0).map(|of| used as f64 / of as f64) {
        Some(r) if r >= 0.9 => Color::Red,
        Some(r) if r >= 0.75 => Color::Yellow,
        _ => Color::Green,
    }
}

// ---------------------------------------------------------------------------
// Server statistics
// ---------------------------------------------------------------------------

fn draw_stats(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.stats;
    let Some(s) = state.latest.as_ref() else {
        let text = if !state.supported {
            "This server has no statistics (GET /api/v1/stats) -- it needs fluxolap 21 or newer. S or q goes back."
        } else if app.offline.is_some() {
            "The server can't be reached."
        } else {
            "Reading the server's statistics …"
        };
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(panel("Server statistics", true)), area);
        return;
    };
    let View::Stats(view) = &app.view else { return };
    let [top, table_area] = Layout::vertical([Constraint::Length(8), Constraint::Min(5)]).areas(area);
    let [server_area, memory_area, requests_area] = Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(34), Constraint::Min(30)]).areas(top);

    // Server
    let server = vec![
        field("version", format!("fluxolap {}", s.version)),
        field("uptime", stats::duration(s.uptime_secs)),
        field("process", format!("pid {} · {} CPUs", s.pid, s.cpus)),
        field("system", format!("{} {}", s.os, s.arch)),
        field("saved to", s.data_dir.clone().unwrap_or_else(|| "nowhere (in memory only)".into())),
        field("contents", format!("{} dbs · {} cubes · {} dims", s.totals.databases, s.totals.cubes, s.totals.dimensions)),
    ];
    frame.render_widget(Paragraph::new(server).block(panel("Server", false)), server_area);

    // Memory
    let block = panel("Memory", false);
    let inner = block.inner(memory_area);
    frame.render_widget(block, memory_area);
    let [gauge_area, text_area, spark_area] = Layout::vertical([Constraint::Length(1), Constraint::Length(3), Constraint::Min(1)]).areas(inner);
    let m = &s.memory;
    let of = m.limit_bytes.or(m.system_total_bytes);
    match (m.rss_bytes, of) {
        (Some(rss), Some(of)) if of > 0 => {
            let label = format!("{} of {} {}", bytes(rss), bytes(of), if m.limit_bytes.is_some() { "limit" } else { "RAM" });
            let gauge = Gauge::default().ratio((rss as f64 / of as f64).min(1.0)).label(label).gauge_style(Style::new().fg(usage_color(rss, Some(of))).bg(Color::Black));
            frame.render_widget(gauge, gauge_area);
        }
        (Some(rss), _) => frame.render_widget(Paragraph::new(field("resident", bytes(rss))), gauge_area),
        (None, _) => frame.render_widget(Paragraph::new(field("resident", "not reported")), gauge_area),
    }
    let opt = |v: Option<u64>| v.map_or_else(|| "–".to_string(), bytes);
    let share = m.rss_bytes.filter(|&r| r > 0).map_or_else(String::new, |r| format!("  ({:.0} % of resident)", m.data_bytes as f64 / r as f64 * 100.0));
    let memory = vec![
        field("peak", opt(m.peak_rss_bytes)),
        field("cell data", format!("{}{share}", bytes(m.data_bytes))),
        field("available", opt(m.system_available_bytes)),
    ];
    frame.render_widget(Paragraph::new(memory), text_area);
    frame.render_widget(Sparkline::default().data(state.rss_series()).style(Style::new().fg(ACCENT)), spark_area);

    // Requests
    let block = panel("Requests", false);
    let inner = block.inner(requests_area);
    frame.render_widget(block, requests_area);
    let [text_area, spark_area] = Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).areas(inner);
    let r = &s.requests;
    let rates = state.rates();
    let requests = vec![
        field("now", rates.map_or_else(|| "–".into(), |x| format!("{:.1} req/s", x.requests_per_sec))),
        field("latency", rates.and_then(|x| x.avg_latency_ms).map_or_else(|| "–".into(), |ms| format!("{ms:.1} ms average"))),
        field("total", format!("{} · {} in flight", group(r.total as f64), r.in_flight)),
        Line::from(vec![
            Span::styled(format!("{:<14}", "errors"), Style::new().fg(MUTED)),
            Span::styled(format!("{} client", group(r.client_errors as f64)), Style::new().fg(if r.client_errors > 0 { Color::Yellow } else { Color::Reset })),
            Span::raw(" · "),
            Span::styled(format!("{} server", group(r.server_errors as f64)), Style::new().fg(if r.server_errors > 0 { Color::Red } else { Color::Reset })),
        ]),
    ];
    frame.render_widget(Paragraph::new(requests), text_area);
    frame.render_widget(Sparkline::default().data(state.request_series()).style(Style::new().fg(CONSOLIDATED)), spark_area);

    // Databases and cubes, largest first
    let rows = app.stats_rows();
    let total = m.data_bytes.max(1);
    let bar = |b: u64| {
        let n = ((b as f64 / total as f64) * 12.0).round() as usize;
        format!("{:<12} {:>3.0}%", "█".repeat(n.min(12)), b as f64 / total as f64 * 100.0)
    };
    let lines: Vec<Row> = rows
        .iter()
        .map(|row| match *row {
            stats::Row::Database(d) => {
                let db = &s.databases[d];
                if !db.loaded {
                    return Row::new(vec![Cell::from(Span::styled(db.name.clone(), Style::new().bold())), Cell::from(""), Cell::from(Span::styled("not loaded", Style::new().fg(MUTED)))]);
                }
                Row::new(vec![
                    Cell::from(Line::from(vec![
                        Span::styled(db.name.clone(), Style::new().bold().fg(ACCENT)),
                        Span::styled(format!("  {} elements", group(db.elements as f64)), Style::new().fg(MUTED)),
                    ])),
                    Cell::from(Line::from(db.dimensions.to_string()).alignment(Alignment::Right)),
                    Cell::from(Line::from(group(db.cells as f64)).alignment(Alignment::Right)),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Line::from(bytes(db.bytes)).alignment(Alignment::Right).bold()),
                    Cell::from(Span::styled(bar(db.bytes), Style::new().fg(ACCENT))),
                ])
            }
            stats::Row::Cube(d, c) => {
                let cube = &s.databases[d].cubes[c];
                let mut name = vec![Span::styled("  ▦ ", Style::new().fg(ACCENT)), Span::raw(cube.name.clone())];
                if let Some(tag) = type_tag(cube.cube_type) {
                    name.push(Span::styled(format!(" {tag}"), Style::new().fg(MUTED).italic()));
                }
                let b = cube.hot_bytes + cube.cold_bytes;
                Row::new(vec![
                    Cell::from(Line::from(name)),
                    Cell::from(Line::from(cube.dimensions.to_string()).alignment(Alignment::Right)),
                    Cell::from(Line::from(group(cube.cells as f64)).alignment(Alignment::Right)),
                    Cell::from(Span::styled(format!("{} / {}", group(cube.hot_cells as f64), group(cube.cold_cells as f64)), Style::new().fg(MUTED))),
                    Cell::from(Line::from(if cube.locks > 0 { cube.locks.to_string() } else { String::new() }).alignment(Alignment::Right)),
                    Cell::from(Line::from(bytes(b)).alignment(Alignment::Right)),
                    Cell::from(Span::styled(bar(b), Style::new().fg(MUTED))),
                ])
            }
        })
        .collect();
    let header = Row::new(vec![
        Cell::from("name"),
        Cell::from(Line::from("dims").alignment(Alignment::Right)),
        Cell::from(Line::from("cells").alignment(Alignment::Right)),
        Cell::from("hot / cold cells"),
        Cell::from(Line::from("locks").alignment(Alignment::Right)),
        Cell::from(Line::from("memory").alignment(Alignment::Right)),
        Cell::from("share"),
    ])
    .style(Style::new().fg(MUTED).bold());
    let title = format!(
        "Databases & cubes by memory · {} cells · {} elements{}",
        group(s.totals.cells as f64),
        group(s.totals.elements as f64),
        if view.show_system { "" } else { " · . shows automatic cubes" }
    );
    let widths = [Constraint::Min(24), Constraint::Length(5), Constraint::Length(12), Constraint::Length(20), Constraint::Length(5), Constraint::Length(10), Constraint::Length(17)];
    let table = Table::new(lines, widths).header(header).block(panel(&title, true)).row_highlight_style(highlight(true)).highlight_symbol("▶ ");
    let mut table_state = TableState::default().with_selected((!rows.is_empty()).then_some(view.sel.min(rows.len().saturating_sub(1))));
    frame.render_stateful_widget(table, table_area, &mut table_state);
}

fn draw_offline(frame: &mut Frame, area: Rect, url: &str, error: &str) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(format!("Can't reach {url}"), Style::new().fg(Color::Red).bold())),
        Line::from(error.to_string()),
        Line::from(""),
        Line::from(Span::styled("Retrying every few seconds. Start a server with `fluxolap`, or pass another address: fluxoterm http://host:7777", Style::new().fg(MUTED))),
    ]);
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center).wrap(Wrap { trim: true }).block(panel("fluxolap", false)), area);
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    let block = Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(Span::styled(format!(" {title} "), Style::new().bold()));
    if focused {
        block.border_style(Style::new().fg(ACCENT))
    } else {
        block.border_style(Style::new().fg(MUTED))
    }
}

fn type_tag(t: ObjectType) -> Option<&'static str> {
    match t {
        ObjectType::Normal => None,
        ObjectType::System => Some("system"),
        ObjectType::Attribute => Some("attribute"),
        ObjectType::UserInfo => Some("user info"),
    }
}

// ---------------------------------------------------------------------------
// Browser
// ---------------------------------------------------------------------------

fn draw_browser(frame: &mut Frame, area: Rect, b: &Browser) {
    let [dbs, contents, details] = Layout::horizontal([Constraint::Percentage(26), Constraint::Percentage(34), Constraint::Min(30)]).areas(area);
    let focus = b.focus.unwrap_or(Pane::Databases);

    let items: Vec<ListItem> = b.databases.iter().map(|d| ListItem::new(Line::from(vec![Span::raw(d.name.clone()), Span::styled(format!("  #{}", d.id), Style::new().fg(MUTED))]))).collect();
    let mut state = ListState::default().with_selected((!b.databases.is_empty()).then_some(b.db_sel));
    let list = List::new(items).block(panel(&format!("Databases ({})", b.databases.len()), focus == Pane::Databases)).highlight_style(highlight(focus == Pane::Databases)).highlight_symbol("▶ ");
    frame.render_stateful_widget(list, dbs, &mut state);

    let rows = b.items();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|item| match item {
            Item::Cube(c) => {
                let mut spans = vec![Span::styled("▦ ", Style::new().fg(ACCENT)), Span::raw(c.name.clone())];
                spans.push(Span::styled(format!("  {} cells", group(c.cell_count as f64)), Style::new().fg(MUTED)));
                if let Some(tag) = type_tag(c.cube_type) {
                    spans.push(Span::styled(format!("  {tag}"), Style::new().fg(MUTED).italic()));
                }
                ListItem::new(Line::from(spans))
            }
            Item::Dimension(d) => {
                let mut spans = vec![Span::styled("≡ ", Style::new().fg(CONSOLIDATED)), Span::raw(d.name.clone())];
                spans.push(Span::styled(format!("  {} elements", group(d.element_count as f64)), Style::new().fg(MUTED)));
                if let Some(tag) = type_tag(d.dim_type) {
                    spans.push(Span::styled(format!("  {tag}"), Style::new().fg(MUTED).italic()));
                }
                ListItem::new(Line::from(spans))
            }
        })
        .collect();
    let title = match b.selected_db() {
        Some(db) => format!("{} · cubes & dimensions", db.name),
        None => "cubes & dimensions".into(),
    };
    let mut state = ListState::default().with_selected((focus == Pane::Contents && !rows.is_empty()).then_some(b.item_sel));
    let list = List::new(items).block(panel(&title, focus == Pane::Contents)).highlight_style(highlight(true)).highlight_symbol("▶ ");
    frame.render_stateful_widget(list, contents, &mut state);

    let text = browser_details(b, &rows, focus);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(panel("Details", false)), details);
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    }
}

fn field(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{label:<14}"), Style::new().fg(MUTED)), Span::raw(value.into())])
}

fn browser_details(b: &Browser, rows: &[Item], focus: Pane) -> Text<'static> {
    let Some(db) = b.selected_db() else {
        return Text::from("No databases yet. Press i (EDIT mode), then o to load the examples or another database file.");
    };
    if focus == Pane::Contents {
        match rows.get(b.item_sel) {
            Some(Item::Cube(c)) => {
                let dims: Vec<String> = c.dimensions.iter().map(|&d| b.dimension_name(d).to_string()).collect();
                return Text::from(vec![
                    Line::from(Span::styled(c.name.clone(), Style::new().bold().fg(ACCENT))),
                    Line::from(""),
                    field("cube id", c.id.to_string()),
                    field("type", type_tag(c.cube_type).unwrap_or("normal")),
                    field("dimensions", dims.join(" × ")),
                    field("stored cells", group(c.cell_count as f64)),
                    Line::from(""),
                    Line::from(Span::styled("Enter opens the cube grid · in EDIT mode: c clears it, dd deletes it", Style::new().fg(MUTED))),
                ]);
            }
            Some(Item::Dimension(d)) => {
                let used_by: Vec<String> = b.contents.as_ref().map_or_else(Vec::new, |(_, _, cubes)| cubes.iter().filter(|c| c.dimensions.contains(&d.id)).map(|c| c.name.clone()).collect());
                return Text::from(vec![
                    Line::from(Span::styled(d.name.clone(), Style::new().bold().fg(CONSOLIDATED))),
                    Line::from(""),
                    field("dimension id", d.id.to_string()),
                    field("type", type_tag(d.dim_type).unwrap_or("normal")),
                    field("elements", group(d.element_count as f64)),
                    field("used by", if used_by.is_empty() { "no cube".to_string() } else { used_by.join(", ") }),
                    Line::from(""),
                    Line::from(Span::styled("Enter shows its element hierarchy", Style::new().fg(MUTED))),
                ]);
            }
            None => {}
        }
    }
    let mut lines = vec![Line::from(Span::styled(db.name.clone(), Style::new().bold().fg(ACCENT))), Line::from(""), field("database id", db.id.to_string())];
    if let Some((id, dims, cubes)) = &b.contents {
        if *id == db.id {
            let normal = |t: ObjectType| t == ObjectType::Normal;
            lines.push(field("cubes", format!("{}  (+{} automatic)", cubes.iter().filter(|c| normal(c.cube_type)).count(), cubes.iter().filter(|c| !normal(c.cube_type)).count())));
            lines.push(field("dimensions", format!("{}  (+{} automatic)", dims.iter().filter(|d| normal(d.dim_type)).count(), dims.iter().filter(|d| !normal(d.dim_type)).count())));
            let cells: usize = cubes.iter().filter(|c| normal(c.cube_type)).map(|c| c.cell_count).sum();
            lines.push(field("stored cells", group(cells as f64)));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Enter/l browses its cubes and dimensions · e exports it · in EDIT mode: dd deletes it", Style::new().fg(MUTED))));
    Text::from(lines)
}

// ---------------------------------------------------------------------------
// Dimension
// ---------------------------------------------------------------------------

fn draw_dimension(frame: &mut Frame, area: Rect, view: &mut DimView) {
    let [tree, details] = Layout::horizontal([Constraint::Min(30), Constraint::Length(46)]).areas(area);
    let block = panel(&format!("{} · {} elements", view.dim.name, group(view.dim.element_count as f64)), true);
    let Some(h) = &view.hierarchy else {
        frame.render_widget(Paragraph::new("loading elements…").block(block), area);
        return;
    };
    let rows = h.visible(&view.expanded);
    let height = block.inner(tree).height as usize;
    view.sel = view.sel.min(rows.len().saturating_sub(1));
    if view.sel < view.scroll {
        view.scroll = view.sel;
    } else if height > 0 && view.sel >= view.scroll + height {
        view.scroll = view.sel + 1 - height;
    }
    let items: Vec<ListItem> = rows
        .iter()
        .skip(view.scroll)
        .take(height)
        .map(|&(id, depth)| {
            let e = h.get(id).expect("visible rows exist");
            let marker = if h.is_consolidated(id) {
                if view.expanded.contains(&id) {
                    "▾ "
                } else {
                    "▸ "
                }
            } else {
                "  "
            };
            let name_style = if h.is_consolidated(id) { Style::new().fg(CONSOLIDATED).bold() } else { Style::new() };
            let mut spans = vec![Span::raw("  ".repeat(depth)), Span::styled(marker, Style::new().fg(CONSOLIDATED)), Span::styled(e.name.clone(), name_style)];
            if !e.children.is_empty() {
                spans.push(Span::styled(format!("  ({})", e.children.len()), Style::new().fg(MUTED)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected((!rows.is_empty()).then_some(view.sel - view.scroll));
    frame.render_stateful_widget(List::new(items).block(block).highlight_style(highlight(true)), tree, &mut state);

    let text = match rows.get(view.sel).and_then(|&(id, _)| h.get(id)) {
        Some(e) => {
            let mut lines = vec![
                Line::from(Span::styled(e.name.clone(), Style::new().bold().fg(if h.is_consolidated(e.id) { CONSOLIDATED } else { ACCENT }))),
                Line::from(""),
                field("element id", e.id.to_string()),
                field("type", format!("{:?}", e.elem_type).to_lowercase()),
                field("level", e.level.to_string()),
                field("position", e.position.to_string()),
                field("parents", if e.parents.is_empty() { "—".to_string() } else { e.parents.iter().map(|&p| h.name(p).to_string()).collect::<Vec<_>>().join(", ") }),
            ];
            if !e.children.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(format!("children ({})", e.children.len()), Style::new().fg(MUTED))));
                for c in e.children.iter().take(40) {
                    let weight = if c.weight == 1.0 { String::new() } else { format!("  × {}", format_number(c.weight)) };
                    lines.push(Line::from(format!("  {}{weight}", h.name(c.id))));
                }
                if e.children.len() > 40 {
                    lines.push(Line::from(Span::styled(format!("  … and {} more", e.children.len() - 40), Style::new().fg(MUTED))));
                }
            }
            Text::from(lines)
        }
        None => Text::from("empty dimension"),
    };
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(panel("Element", false)), details);
}

// ---------------------------------------------------------------------------
// Grid
// ---------------------------------------------------------------------------

fn draw_grid(frame: &mut Frame, area: Rect, grid: &mut Grid) {
    let [info, table] = Layout::vertical([Constraint::Length(4), Constraint::Min(3)]).areas(area);
    let dim_names: Vec<&str> = grid.dims.iter().map(|d| d.name.as_str()).collect();
    if !grid.ready() {
        let block = panel(&grid.cube.name, true);
        frame.render_widget(Paragraph::new(format!("loading {} …", dim_names.join(", "))).block(block), area);
        return;
    }

    // What's shown: the cube, the axes, and one element per other dimension.
    let mut filter = vec![Span::styled("filter  ", Style::new().fg(MUTED))];
    let fixed = grid.fixed_dims();
    if fixed.is_empty() {
        filter.push(Span::styled("—", Style::new().fg(MUTED)));
    }
    // With many dimensions only the focused filter and the ones moved off
    // their top element are shown; the rest are summed up.
    let many = fixed.len() > 6;
    let mut hidden = 0;
    for d in &fixed {
        let focused = grid.fixed_focus == Some(*d);
        if many && !focused && grid.defaults.get(*d) == Some(&grid.fixed[*d]) {
            hidden += 1;
            continue;
        }
        let name = grid.hier[*d].as_ref().map_or("?", |h| h.name(grid.fixed[*d])).to_string();
        filter.push(Span::styled(format!("{}: ", grid.dims[*d].name), Style::new().fg(MUTED)));
        let style = if focused { Style::new().fg(Color::Black).bg(ACCENT).bold() } else { Style::new().bold() };
        filter.push(Span::styled(if focused { format!("◂ {name} ▸") } else { name }, style));
        filter.push(Span::raw("   "));
    }
    if hidden > 0 {
        filter.push(Span::styled(format!("+{hidden} more at their top element (f: choose)"), Style::new().fg(MUTED)));
    }
    let col_name = grid.col_dim.map_or("—", |c| dim_names[c]);
    let info_text = Text::from(vec![
        Line::from(vec![
            Span::styled(grid.cube.name.clone(), Style::new().bold().fg(ACCENT)),
            Span::styled(
                format!(
                    "   {}   {} stored cells",
                    if dim_names.len() <= 6 { dim_names.join(" × ") } else { format!("{} dimensions", dim_names.len()) },
                    group(grid.cube.cell_count as f64)
                ),
                Style::new().fg(MUTED),
            ),
        ]),
        Line::from(vec![
            Span::styled("rows    ", Style::new().fg(MUTED)),
            Span::styled(dim_names[grid.row_dim].to_string(), Style::new().bold()),
            Span::styled("   columns  ", Style::new().fg(MUTED)),
            Span::styled(col_name.to_string(), Style::new().bold()),
            Span::styled(
                if grid.suppressing() { "   empty rows hidden (z)" } else if grid.hide_empty { "   too many rows to hide empty ones" } else { "   empty rows shown (z)" },
                Style::new().fg(MUTED),
            ),
        ]),
        Line::from(filter),
    ]);
    frame.render_widget(Paragraph::new(info_text), info);

    // Whether the values refresh on their own (a toggles), at the right of the frame.
    let refresh = if grid.auto_refresh { " ↻ every 5 s " } else { " ↻ off (r: once) " };
    let block = panel("cells", true).title(Line::from(Span::styled(refresh, Style::new().fg(MUTED))).alignment(Alignment::Right));
    let inner = block.inner(table);
    frame.render_widget(block, table);

    let rows = grid.rows();
    let cols = grid.cols();
    let row_h = grid.row_hier().clone();
    let col_h = grid.col_hier().cloned();

    // Rows: keep the cursor on screen.
    let height = (inner.height as usize).saturating_sub(1).max(1);
    grid.cur_row = grid.cur_row.min(rows.len().saturating_sub(1));
    grid.cur_col = grid.cur_col.min(cols.len().saturating_sub(1));
    if grid.cur_row < grid.top_row {
        grid.top_row = grid.cur_row;
    } else if grid.cur_row >= grid.top_row + height {
        grid.top_row = grid.cur_row + 1 - height;
    }
    let visible_rows: Vec<(u32, usize)> = rows.iter().skip(grid.top_row).take(height).copied().collect();

    // Row labels: indented, with an open/closed marker on consolidated elements.
    let label = |id: u32, depth: usize, expanded: bool| -> String {
        let marker = if row_h.is_consolidated(id) { if expanded { "▾ " } else { "▸ " } } else { "  " };
        format!("{}{marker}{}", "  ".repeat(depth), row_h.name(id))
    };
    let label_width = visible_rows.iter().map(|&(id, d)| label(id, d, false).chars().count()).max().unwrap_or(8).max(dim_names[grid.row_dim].chars().count()).clamp(10, 42) as u16;

    // Columns: as many as fit, each as wide as its header or widest known value.
    let col_label = |id: u32| -> String {
        match &col_h {
            Some(h) => {
                let marker = if h.is_consolidated(id) { if grid.expanded_cols.contains(&id) { "▾" } else { "▸" } } else { "" };
                format!("{marker}{}", h.name(id))
            }
            None => "value".to_string(),
        }
    };
    // Each column shows as many decimals as its most precise visible value,
    // so its numbers line up on the decimal point.
    let col_decimals = |col: u32| -> usize {
        visible_rows
            .iter()
            .filter_map(|&(r, _)| match grid.cells.get(&grid.path(r, col)) {
                Some(Some(Value::Number(n))) => Some(decimals_of(*n)),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    };
    let col_width = |col: u32| -> u16 {
        let header = col_label(col).chars().count();
        let d = col_decimals(col);
        let values = visible_rows.iter().filter_map(|&(r, _)| grid.cells.get(&grid.path(r, col)).map(|v| cell_text(v, d).chars().count())).max().unwrap_or(0);
        header.max(values).clamp(6, 28) as u16
    };
    let widths: Vec<u16> = cols.iter().map(|&(c, _)| col_width(c)).collect();
    let decimals: Vec<usize> = cols.iter().map(|&(c, _)| col_decimals(c)).collect();
    let available = inner.width.saturating_sub(label_width + 1);
    let fit = |left: usize| -> usize {
        let mut used = 0u16;
        let mut n = 0;
        for w in widths.iter().skip(left) {
            if used + w + 1 > available && n > 0 {
                break;
            }
            used += w + 1;
            n += 1;
        }
        n.max(1)
    };
    let mut left_col = grid.left_col.min(grid.cur_col);
    while grid.cur_col >= left_col + fit(left_col) {
        left_col += 1;
    }
    let n_cols = fit(left_col);
    grid.left_col = left_col;
    grid.viewport = (height, n_cols);
    let visible_cols: Vec<(u32, usize)> = cols.iter().skip(grid.left_col).take(n_cols).copied().collect();

    let header_style = Style::new().fg(MUTED).bold();
    let mut header_cells = vec![Cell::from(Span::styled(dim_names[grid.row_dim].to_string(), Style::new().fg(MUTED)))];
    for (i, &(c, _)) in visible_cols.iter().enumerate() {
        let consolidated = col_h.as_ref().is_some_and(|h| h.is_consolidated(c));
        let mut style = if consolidated { Style::new().fg(CONSOLIDATED).bold() } else { header_style };
        if grid.left_col + i == grid.cur_col {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        // A text column reads left-aligned, header included.
        let text_column = visible_rows.iter().any(|&(r, _)| matches!(grid.cells.get(&grid.path(r, c)), Some(Some(Value::Text(_)))));
        let align = if text_column { Alignment::Left } else { Alignment::Right };
        header_cells.push(Cell::from(Line::from(Span::styled(col_label(c), style)).alignment(align)));
    }

    let table_rows: Vec<Row> = visible_rows
        .iter()
        .enumerate()
        .map(|(ri, &(r, depth))| {
            let row_index = grid.top_row + ri;
            let consolidated_row = row_h.is_consolidated(r);
            let mut label_style = if consolidated_row { Style::new().fg(CONSOLIDATED).bold() } else { Style::new() };
            if row_index == grid.cur_row {
                label_style = label_style.add_modifier(Modifier::UNDERLINED);
            }
            let mut cells = vec![Cell::from(Span::styled(label(r, depth, grid.expanded_rows.contains(&r)), label_style))];
            for (ci, &(c, _)) in visible_cols.iter().enumerate() {
                let path = grid.path(r, c);
                let decimals = decimals[grid.left_col + ci];
                let consolidated = consolidated_row || col_h.as_ref().is_some_and(|h| h.is_consolidated(c)) || fixed.iter().any(|&d| grid.hier[d].as_ref().is_some_and(|h| h.is_consolidated(grid.fixed[d])));
                let (text, mut style, align) = match grid.cells.get(&path) {
                    None => ("…".to_string(), Style::new().fg(MUTED), Alignment::Right),
                    Some(None) => ("·".to_string(), Style::new().fg(MUTED), Alignment::Right),
                    Some(Some(Value::Number(n))) => (format_aligned(*n, decimals), if consolidated { Style::new().bold() } else { Style::new() }, Alignment::Right),
                    Some(Some(Value::Text(t))) => (t.clone(), Style::new().fg(TEXT_VALUE), Alignment::Left),
                };
                if row_index == grid.cur_row && grid.left_col + ci == grid.cur_col {
                    style = Style::new().fg(Color::Black).bg(ACCENT).bold();
                }
                cells.push(Cell::from(Line::from(Span::styled(text, style)).alignment(align)));
            }
            Row::new(cells)
        })
        .collect();

    let mut constraints = vec![Constraint::Length(label_width)];
    constraints.extend(widths.iter().skip(grid.left_col).take(n_cols).map(|&w| Constraint::Length(w)));
    let table_widget = Table::new(table_rows, constraints).header(Row::new(header_cells).style(header_style)).column_spacing(1);
    frame.render_widget(table_widget, inner);
}

/// A cell as displayed (for width calculation).
fn cell_text(v: &Option<Value>, decimals: usize) -> String {
    match v {
        None => "·".into(),
        Some(Value::Number(n)) => format_aligned(*n, decimals),
        Some(Value::Text(t)) => t.clone(),
    }
}

// ---------------------------------------------------------------------------
// Popups and hints
// ---------------------------------------------------------------------------

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(4));
    let h = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_popup(frame: &mut Frame, popup: &Popup, app: &App) {
    match popup {
        Popup::Picker { files, sel, input, welcome } => draw_picker(frame, app, files.as_deref(), *sel, input, *welcome),
        Popup::DimPicker { purpose, query, sel } => draw_dim_picker(frame, app, *purpose, query, *sel),
        Popup::Confirm { text, .. } => {
            let area = centered(frame.area(), 64, 7);
            frame.render_widget(Clear, area);
            let body = Text::from(vec![Line::from(""), Line::from(text.clone()).bold(), Line::from(""), Line::from(Span::styled("y / Enter: yes    n / Esc: no", Style::new().fg(MUTED)))]);
            frame.render_widget(Paragraph::new(body).alignment(Alignment::Center).wrap(Wrap { trim: true }).block(panel("Confirm", true).border_style(Style::new().fg(Color::Red))), area);
        }
        Popup::Input { title, value, kind } => {
            let area = centered(frame.area(), 72, 6);
            frame.render_widget(Clear, area);
            let hint = match kind {
                InputKind::EditCell { .. } => "a number, text, or empty · Enter: save · Esc: cancel",
                InputKind::Export { .. } => "file to write · Enter: export · Esc: cancel",
                InputKind::Rename(_) => "Enter: rename · Esc: cancel",
            };
            let body = Text::from(vec![Line::from(""), Line::from(vec![Span::raw(format!(" {value}")), Span::styled("▏", Style::new().fg(ACCENT))]), Line::from(""), Line::from(Span::styled(format!(" {hint}"), Style::new().fg(MUTED)))]);
            frame.render_widget(Paragraph::new(body).block(panel(title, true)), area);
        }
        Popup::Help => {
            let area = centered(frame.area(), 80, 40);
            frame.render_widget(Clear, area);
            let key = |k: &str, d: &str| Line::from(vec![Span::styled(format!("  {k:<14}"), Style::new().fg(ACCENT).bold()), Span::raw(d.to_string())]);
            let head = |t: &str| Line::from(Span::styled(t.to_string(), Style::new().bold()));
            let lines = vec![
                head("Modes (like vim)"),
                key("VIEW", "the default: look around, nothing can change"),
                key("i  /  Esc", "enter EDIT mode (changes go to the server) / back to VIEW"),
                Line::from(""),
                head("Moving"),
                key("h j k l  arrows", "move · h/l switch panes in the browser"),
                key("gg  G", "top · bottom"),
                key("Ctrl-d  Ctrl-u", "half a page down / up"),
                key("Enter", "open a database, cube or dimension · in a grid: drill down"),
                key("q  Esc", "back · quit from the browser"),
                Line::from(""),
                head("Cube grid"),
                key("Space  x", "open/close the row / column element under the cursor"),
                key("s", "swap rows and columns"),
                key("R  C  f", "choose (type to search) the dimension for rows / columns / to filter"),
                key("[  ]", "previous / next element of the chosen filter"),
                key("z  r", "hide/show rows without any value · reload"),
                key("a  :auto", "refresh the values on their own every 5 s, on/off"),
                key("EDIT: Enter", "edit the cell (a consolidated cell splashes)"),
                key("EDIT: dd  Del", "clear the cell"),
                Line::from(""),
                head("Browser"),
                key(".", "show/hide automatic system and attribute objects"),
                key("e", "export the selected database to a file"),
                key("EDIT: o", "load a database file (the examples are listed)"),
                key("EDIT: F2  n", "rename · dd / Del delete · c clear a cube's cells"),
                Line::from(""),
                head("Commands"),
                key(":q", "quit"),
                key(":w [file]", "export the current database (default <name>.bin.zst)"),
                key(":e [file]", "EDIT: load a database file (no file: pick one)"),
                key(":r  :help", "refresh · this help"),
                key("S  :stats", "the server's memory and statistics (S again: back)"),
                Line::from(""),
                Line::from(Span::styled("  any key closes this help", Style::new().fg(MUTED))),
            ];
            frame.render_widget(Paragraph::new(lines).block(panel("Keys", true)), area);
        }
    }
}

/// The bottom line: the mode (like vim's `-- INSERT --`), then either the
/// `:` command being typed or the keys that work right now.
fn hint_line(app: &App) -> Line<'static> {
    let mut spans = match app.mode {
        Mode::View => vec![Span::styled(" VIEW ", Style::new().fg(Color::Black).bg(Color::Gray).bold())],
        Mode::Edit => vec![Span::styled(" EDIT ", Style::new().fg(Color::White).bg(Color::Red).bold())],
    };
    spans.push(Span::raw(" "));
    if let Some(command) = &app.command {
        spans.push(Span::styled(format!(":{command}"), Style::new().bold()));
        spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
        return Line::from(spans);
    }
    let edit = app.mode == Mode::Edit;
    let hints: Vec<(&str, &str)> = if app.popup.is_some() {
        Vec::new()
    } else {
        match (&app.view, edit) {
            (View::Browser, false) => vec![("hjkl", "move"), ("Enter", "open"), ("i", "edit mode"), ("e / :w", "export"), (".", "system objects"), ("S", "stats"), (":", "command"), ("?", "help"), ("q", "quit")],
            (View::Browser, true) => vec![("o / :e", "load"), ("F2", "rename"), ("dd", "delete"), ("c", "clear cube"), ("e", "export"), ("Esc", "view mode"), ("?", "help")],
            (View::Dimension(_), _) => vec![("jk", "move"), ("l h", "open/close"), ("gg G", "top/bottom"), ("q", "back"), ("?", "help")],
            (View::Grid(_), false) => vec![("hjkl", "move"), ("Enter/Space x", "open row/col"), ("s", "swap"), ("R C", "choose axes"), ("f [ ]", "filter"), ("z", "empty rows"), ("a", "auto refresh"), ("i", "edit mode"), ("q", "back"), ("?", "help")],
            (View::Grid(_), true) => vec![("Enter", "edit cell"), ("dd / Del", "clear cell"), ("hjkl", "move"), ("Esc", "view mode"), ("?", "help")],
            (View::Stats(_), _) => vec![("jk", "move"), ("Enter", "open"), (".", "automatic cubes"), ("r", "refresh"), ("S / q", "back"), ("?", "help")],
        }
    };
    for (k, d) in hints {
        spans.push(Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(MUTED)));
        spans.push(Span::styled(format!(" {d}  "), Style::new().fg(MUTED)));
    }
    Line::from(spans)
}

/// The import picker: database files found on disk, a filter/path line,
/// and "load all" on top.
fn draw_picker(frame: &mut Frame, app: &App, files: Option<&[crate::app::DbFile]>, sel: usize, input: &str, welcome: bool) {
    let area = centered(frame.area(), 96, 24);
    frame.render_widget(Clear, area);
    let title = if welcome { "The server has no databases yet -- load some?" } else { "Load a database file" };
    let block = panel(title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [input_area, list_area, hint_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(3), Constraint::Length(1)]).areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" filter or path  ", Style::new().fg(MUTED)),
            Span::raw(input.to_string()),
            Span::styled("▏", Style::new().fg(ACCENT)),
        ])),
        input_area,
    );

    let lines: Vec<Line> = match files {
        None => vec![Line::from(Span::styled(" searching …", Style::new().fg(MUTED)))],
        Some(files) => {
            let entries = app.picker_entries(files, input);
            if entries.is_empty() {
                let mut lines = vec![Line::from(Span::styled(
                    if input.trim().is_empty() { " No database files (*.bin.zst) found in:" } else { " No file matches -- Enter loads what you typed as a path." },
                    Style::new().fg(MUTED),
                ))];
                if input.trim().is_empty() {
                    lines.extend(app.search_dirs.iter().map(|d| Line::from(Span::styled(format!("   {d}"), Style::new().fg(MUTED)))));
                }
                lines
            } else {
                let height = list_area.height as usize;
                let top = sel.saturating_sub(height.saturating_sub(1));
                entries
                    .iter()
                    .enumerate()
                    .skip(top)
                    .take(height)
                    .map(|(i, entry)| {
                        let selected = i == sel;
                        let mut spans = match entry {
                            PickerEntry::All(paths) => vec![Span::styled(format!(" ★ Load all {} not yet on the server", paths.len()), Style::new().fg(ACCENT).bold())],
                            PickerEntry::File { file, loaded } => {
                                // The path goes last: it's the part that may be cut off.
                                vec![
                                    Span::styled(format!(" {:<24}", file.name), if *loaded { Style::new().fg(MUTED) } else { Style::new().bold() }),
                                    Span::styled(format!("{:>9}  ", human_size(file.size)), Style::new().fg(MUTED)),
                                    Span::styled(if *loaded { "✓ on the server  " } else { "                 " }, Style::new().fg(TEXT_VALUE)),
                                    Span::styled(file.path.clone(), Style::new().fg(MUTED)),
                                ]
                            }
                        };
                        if selected {
                            spans = spans.into_iter().map(|s| s.patch_style(Style::new().fg(Color::Black).bg(ACCENT))).collect();
                        }
                        Line::from(spans)
                    })
                    .collect()
            }
        }
    };
    frame.render_widget(Paragraph::new(lines), list_area);
    frame.render_widget(Paragraph::new(Span::styled(" ↑↓ choose · Enter load · type to filter or give a path · Esc close", Style::new().fg(MUTED))), hint_area);
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / (1 << 10) as f64),
        b => format!("{b} B"),
    }
}

/// Choosing a dimension for rows, columns or the filter: searchable, since
/// a cube can have a hundred dimensions.
fn draw_dim_picker(frame: &mut Frame, app: &App, purpose: DimPurpose, query: &str, sel: usize) {
    let area = centered(frame.area(), 70, 22);
    frame.render_widget(Clear, area);
    let title = match purpose {
        DimPurpose::Rows => "Dimension on rows",
        DimPurpose::Columns => "Dimension on columns",
        DimPurpose::Filter => "Filter to change with [ ]",
    };
    let block = panel(title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [input_area, list_area, hint_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(3), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(" search  ", Style::new().fg(MUTED)), Span::raw(query.to_string()), Span::styled("▏", Style::new().fg(ACCENT))])),
        input_area,
    );
    let entries = app.dim_picker_entries(purpose, query);
    let View::Grid(grid) = &app.view else { return };
    let height = list_area.height as usize;
    let top = sel.saturating_sub(height.saturating_sub(1));
    let lines: Vec<Line> = if entries.is_empty() {
        vec![Line::from(Span::styled(" no dimension matches", Style::new().fg(MUTED)))]
    } else {
        entries
            .iter()
            .enumerate()
            .skip(top)
            .take(height)
            .map(|(i, (d, place))| {
                let mut spans = vec![
                    Span::styled(format!(" {:<30}", grid.dims[*d].name), Style::new().bold()),
                    Span::styled(format!("{:>9} elements  ", group(grid.dims[*d].element_count as f64)), Style::new().fg(MUTED)),
                    Span::styled(place.clone(), Style::new().fg(MUTED)),
                ];
                if i == sel {
                    spans = spans.into_iter().map(|s| s.patch_style(Style::new().fg(Color::Black).bg(ACCENT))).collect();
                }
                Line::from(spans)
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), list_area);
    frame.render_widget(Paragraph::new(Span::styled(" type to search · ↑↓ choose · Enter pick · Esc close", Style::new().fg(MUTED))), hint_area);
}
