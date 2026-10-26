//! App state and the actions that change it (and load what they need).
//! A cube can have a hundred dimensions and more, so elements load
//! lazily: every element only for axis dimensions and a filter being
//! changed, just the top-level ones for all other filters.

use crate::api::Api;
use crate::model::hierarchy::Hierarchy;
use crate::model::pivot::{Pivot, MAX_READ_CELLS, MAX_VIEW_CELLS};
use fluxolap_api_types::{CubeDto, DatabaseDto, DimensionDto, ExampleDto, ServerDto, Value};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Show {
    #[default]
    Table,
    Line,
    Bar,
    Stacked,
}

impl Show {
    pub fn key(self) -> &'static str {
        match self {
            Show::Table => "table",
            Show::Line => "line",
            Show::Bar => "bar",
            Show::Stacked => "stacked",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "line" => Show::Line,
            "bar" => Show::Bar,
            "stacked" => Show::Stacked,
            _ => Show::Table,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Loaded {
    No,
    Roots,
    All,
}

/// The cube being explored.
#[derive(Debug, Clone)]
pub struct Explorer {
    pub db: u32,
    pub cube: CubeDto,
    /// The cube's dimensions, in coordinate order.
    pub dims: Vec<DimensionDto>,
    /// Per dimension, as far as loaded (see `loaded`).
    pub hier: Vec<Hierarchy>,
    pub loaded: Vec<Loaded>,
    pub pivot: Option<Pivot>,
    pub cells: HashMap<Vec<u32>, Option<Value>>,
    /// `cells` is the whole answer for the current view: a cell not in it
    /// is empty (a sparse answer leaves those out), not still loading.
    pub complete: bool,
    /// Increments per cell request; older answers are ignored.
    pub token: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub show: Show,
    pub heatmap: bool,
    /// The dimension panel's search text.
    pub dim_query: String,
    /// A view from the URL, applied once the dimensions are known.
    pending_view: Option<ViewState>,
}

impl Explorer {
    fn axis_ready(&self) -> bool {
        self.pivot.as_ref().is_some_and(|p| p.rows.iter().chain(&p.cols).all(|&d| self.loaded[d] == Loaded::All))
    }
}

/// A database's dimensions and cubes.
pub type DbContents = (Vec<DimensionDto>, Vec<CubeDto>);

/// Everything the app shares, handed to components through context.
#[derive(Clone, Copy)]
pub struct Ctx {
    pub api: RwSignal<Api>,
    pub server: RwSignal<Option<Result<ServerDto, String>>>,
    pub databases: RwSignal<Vec<DatabaseDto>>,
    /// Per database: its dimensions and cubes, once loaded.
    pub contents: RwSignal<HashMap<u32, DbContents>>,
    pub open_db: RwSignal<Option<u32>>,
    pub explorer: RwSignal<Option<Explorer>>,
    /// The "load a database" page is showing (instead of the cube).
    pub show_load: RwSignal<bool>,
    /// The server's example databases, once asked for.
    pub examples: RwSignal<Option<Result<Vec<ExampleDto>, String>>>,
}

impl Ctx {
    pub fn new(api: Api) -> Self {
        Self {
            api: RwSignal::new(api),
            server: RwSignal::new(None),
            databases: RwSignal::new(Vec::new()),
            contents: RwSignal::new(HashMap::new()),
            open_db: RwSignal::new(None),
            explorer: RwSignal::new(None),
            show_load: RwSignal::new(false),
            examples: RwSignal::new(None),
        }
    }

    /// (Re)connects: server info and the database list. An empty server
    /// opens the load page, so its examples are one click away.
    pub fn connect(self) {
        let api = self.api.get_untracked();
        spawn_local(async move {
            let server = api.server().await;
            let ok = server.is_ok();
            self.server.set(Some(server));
            if ok {
                if let Ok(dbs) = api.databases().await {
                    if dbs.is_empty() && self.explorer.with_untracked(Option::is_none) {
                        self.open_load();
                    }
                    self.databases.set(dbs);
                }
            }
        });
    }

    /// Shows the load page and (re)reads the server's examples.
    pub fn open_load(self) {
        self.show_load.set(true);
        let api = self.api.get_untracked();
        spawn_local(async move {
            let examples = api.examples().await;
            self.examples.set(Some(examples));
        });
    }

    /// After an import: the new database shows up in the sidebar, opened,
    /// and the examples' "loaded" marks are refreshed.
    pub fn imported(self, db: &DatabaseDto) {
        let (api, id) = (self.api.get_untracked(), db.id);
        spawn_local(async move {
            if let Ok(dbs) = api.databases().await {
                self.databases.set(dbs);
            }
            if let Ok(examples) = api.examples().await {
                self.examples.set(Some(Ok(examples)));
            }
            self.open_database(id, None);
        });
    }

    /// Opens a database in the sidebar, loading its dimensions and cubes.
    pub fn open_database(self, db: u32, then: Option<(u32, Option<ViewState>)>) {
        self.open_db.set(Some(db));
        if let Some((_, cubes)) = self.contents.with_untracked(|c| c.get(&db).cloned()) {
            if let Some((cube, view)) = then {
                if let Some(cube) = cubes.iter().find(|c| c.id == cube).cloned() {
                    self.open_cube(db, cube, view);
                }
            }
            return;
        }
        let api = self.api.get_untracked();
        spawn_local(async move {
            let result = async { Ok::<_, String>((api.dimensions(db).await?, api.cubes(db).await?)) }.await;
            match result {
                Ok((dims, cubes)) => {
                    let target = then.and_then(|(cube, view)| cubes.iter().find(|c| c.id == cube).cloned().map(|c| (c, view)));
                    self.contents.update(|c| {
                        c.insert(db, (dims, cubes));
                    });
                    if let Some((cube, view)) = target {
                        self.open_cube(db, cube, view);
                    }
                }
                Err(e) => self.server.set(Some(Err(e))),
            }
        });
    }

    /// Opens a cube: its dimensions' elements load (all of them for the
    /// axis dimensions, top-level ones for the rest), then the cells.
    pub fn open_cube(self, db: u32, cube: CubeDto, view: Option<ViewState>) {
        self.show_load.set(false);
        let all_dims = self.contents.with_untracked(|c| c.get(&db).map(|(d, _)| d.clone())).unwrap_or_default();
        let dims: Vec<DimensionDto> = cube
            .dimensions
            .iter()
            .map(|id| all_dims.iter().find(|d| d.id == *id).cloned().unwrap_or(DimensionDto { id: *id, name: format!("#{id}"), dim_type: Default::default(), element_count: 0, modified: None }))
            .collect();
        let n = dims.len();
        // Every element for the axis dimensions and for filters a link moved
        // off their top element (to show that element's name); top-level
        // ones for the rest.
        let axis: HashSet<usize> = match &view {
            // No rows in the link: the default axes (see `ViewState::apply`).
            Some(v) if v.rows.is_empty() => (0..n.min(2)).chain(v.filters.iter().map(|&(d, _)| d)).filter(|&d| d < n).collect(),
            Some(v) => v.rows.iter().chain(&v.cols).copied().chain(v.filters.iter().map(|&(d, _)| d)).filter(|&d| d < n).collect(),
            None => (0..n.min(2)).collect(),
        };
        self.explorer.set(Some(Explorer {
            db,
            cube: cube.clone(),
            dims: dims.clone(),
            hier: vec![Hierarchy::default(); n],
            loaded: vec![Loaded::No; n],
            pivot: None,
            cells: HashMap::new(),
            complete: false,
            token: 0,
            loading: true,
            error: None,
            show: view.as_ref().map_or(Show::Table, |v| v.show),
            heatmap: view.as_ref().is_some_and(|v| v.heatmap),
            dim_query: String::new(),
            pending_view: view,
        }));
        for (i, dim) in dims.iter().enumerate() {
            self.load_elements(cube.id, i, dim.id, !axis.contains(&i));
        }
    }

    /// Loads dimension `index`'s elements (all, or only the top-level ones).
    pub fn load_elements(self, cube: u32, index: usize, dim: u32, roots_only: bool) {
        let (api, db) = (self.api.get_untracked(), self.explorer.with_untracked(|e| e.as_ref().map(|e| e.db)));
        let Some(db) = db else { return };
        spawn_local(async move {
            let result = api.elements(db, dim, roots_only).await;
            let mut reload = false;
            let mut axis_to_load = Vec::new();
            self.explorer.update(|ex| {
                let Some(ex) = ex.as_mut().filter(|e| e.cube.id == cube && e.db == db) else { return };
                match result {
                    Err(e) => ex.error = Some(e),
                    Ok(elements) => {
                        if roots_only && ex.loaded[index] == Loaded::All {
                            return;
                        }
                        ex.hier[index] = Hierarchy::new(elements);
                        ex.loaded[index] = if roots_only { Loaded::Roots } else { Loaded::All };
                        if ex.pivot.is_none() && ex.loaded.iter().all(|l| *l != Loaded::No) {
                            let mut pivot = Pivot::new(&ex.hier);
                            if let Some(view) = ex.pending_view.take() {
                                view.apply(&mut pivot, &ex.hier);
                            }
                            // The axes can differ from what was loaded in full up front (a
                            // link leaves default axes out): load the rest now.
                            axis_to_load = pivot.rows.iter().chain(&pivot.cols).filter(|&&d| ex.loaded[d] != Loaded::All).map(|&d| (d, ex.dims[d].id)).collect();
                            ex.pivot = Some(pivot);
                            reload = true;
                        } else if let Some(pivot) = ex.pivot.as_mut() {
                            // An axis dimension finished loading: open it like at the start.
                            if !roots_only && pivot.on_axis(index) && pivot.expanded[index].is_empty() {
                                pivot.open_single_root(index, &ex.hier[index]);
                            }
                            reload = !roots_only;
                        }
                    }
                }
            });
            for (index, dim) in axis_to_load {
                self.load_elements(cube, index, dim, false);
            }
            if reload {
                self.reload();
            }
        });
    }

    /// Changes the pivot, loading any dimension that just moved onto an
    /// axis, then re-reads the cells.
    pub fn change(self, f: impl FnOnce(&mut Pivot, &[Hierarchy])) {
        let mut to_load = Vec::new();
        self.explorer.update(|ex| {
            let Some(ex) = ex.as_mut() else { return };
            let Some(pivot) = ex.pivot.as_mut() else { return };
            f(pivot, &ex.hier);
            for &d in pivot.rows.iter().chain(&pivot.cols) {
                if ex.loaded[d] != Loaded::All {
                    to_load.push((ex.cube.id, d, ex.dims[d].id));
                }
            }
        });
        for (cube, d, dim) in to_load {
            self.load_elements(cube, d, dim, false);
        }
        self.reload();
    }

    /// Makes sure every element of dimension `index` is known (to choose a filter value).
    pub fn need_all_elements(self, index: usize) {
        let target = self.explorer.with_untracked(|e| e.as_ref().filter(|e| e.loaded[index] != Loaded::All).map(|e| (e.cube.id, e.dims[index].id)));
        if let Some((cube, dim)) = target {
            self.load_elements(cube, index, dim, false);
        }
    }

    /// Re-reads the cells for the current view (one area query) and
    /// updates the URL.
    pub fn reload(self) {
        self.read_cells(false);
    }

    /// Every few seconds (while the page is visible): picks up what changed
    /// on the server meanwhile -- a cell edited in fluxoterm, a database
    /// loaded or deleted -- without the user doing anything. Unchanged
    /// answers change nothing on screen.
    pub fn poll(self) {
        let hidden = web_sys::window().and_then(|w| w.document()).is_some_and(|d| d.hidden());
        if hidden || self.server.with_untracked(|s| s.as_ref().is_none_or(|s| s.is_err())) {
            if !hidden && self.server.with_untracked(|s| matches!(s, Some(Err(_)))) {
                self.connect(); // offline: try again
            }
            return;
        }
        let (api, open_db) = (self.api.get_untracked(), self.open_db.get_untracked());
        spawn_local(async move {
            if let Ok(dbs) = api.databases().await {
                if self.databases.with_untracked(|d| *d != dbs) {
                    self.databases.set(dbs);
                }
            }
            if let Some(db) = open_db {
                if let (Ok(dims), Ok(cubes)) = (api.dimensions(db).await, api.cubes(db).await) {
                    let fresh = (dims, cubes);
                    if self.contents.with_untracked(|c| c.get(&db) != Some(&fresh)) {
                        self.contents.update(|c| {
                            c.insert(db, fresh);
                        });
                    }
                }
            }
        });
        self.read_cells(true);
    }

    /// The cell request behind `reload` (`quiet`: a background refresh --
    /// no spinner, no URL update, skipped while another request is out).
    fn read_cells(self, quiet: bool) {
        let api = self.api.get_untracked();
        let mut request = None;
        let mut update = |ex: &mut Explorer| {
            if !quiet {
                ex.complete = false; // the view changed: what's there may not be all of it
            }
            if !ex.axis_ready() || (quiet && (ex.loading || ex.error.is_some())) {
                return;
            }
            let pivot = ex.pivot.as_ref().expect("axis_ready checks it");
            let size = pivot.size(&ex.hier);
            // With empty rows hidden only the cells with a value come back,
            // so a sparse view can read far more than a table can show.
            let limit = if pivot.hide_empty { MAX_READ_CELLS } else { MAX_VIEW_CELLS };
            let group = |n: usize| crate::model::format::group(n as f64);
            if size > limit {
                ex.loading = false;
                ex.error = Some(if pivot.hide_empty {
                    format!("This view would read {} cells -- close some opened elements or move a dimension to the filters (at most {}).", group(size), group(limit))
                } else {
                    format!(
                        "This view would show {} cells -- close some opened elements, move a dimension to the filters, or turn on “hide empty” (at most {}, or {} with empty rows hidden).",
                        group(size),
                        group(limit),
                        group(MAX_READ_CELLS)
                    )
                });
                return;
            }
            // A big view isn't re-read in the background: reading it takes the server seconds.
            if quiet && size > MAX_VIEW_CELLS {
                return;
            }
            ex.error = None;
            ex.token += 1;
            request = Some((ex.db, ex.cube.id, pivot.area(&ex.hier), ex.token, pivot.hide_empty));
            if !quiet {
                ex.loading = true;
                write_hash(ex);
            }
        };
        if quiet {
            // Only look first: an unchanged view mustn't wake up every reader of `explorer`.
            let ready = self.explorer.with_untracked(|ex| ex.as_ref().is_some_and(|ex| ex.axis_ready() && !ex.loading && ex.error.is_none()));
            if !ready {
                return;
            }
            self.explorer.update_untracked(|ex| {
                if let Some(ex) = ex.as_mut() {
                    update(ex);
                }
            });
        } else {
            self.explorer.update(|ex| {
                if let Some(ex) = ex.as_mut() {
                    update(ex);
                }
            });
        }
        let Some((db, cube, area, token, skip_empty)) = request else { return };
        spawn_local(async move {
            let result = api.area(db, cube, area, skip_empty).await;
            let cells: Option<HashMap<Vec<u32>, Option<Value>>> = result.as_ref().ok().map(|cells| cells.iter().map(|c| (c.path.clone(), c.value.clone())).collect());
            // A background refresh that found nothing new changes nothing.
            if quiet && self.explorer.with_untracked(|ex| ex.as_ref().is_some_and(|ex| ex.token == token && Some(&ex.cells) == cells.as_ref())) {
                return;
            }
            self.explorer.update(|ex| {
                let Some(ex) = ex.as_mut().filter(|e| e.token == token) else { return };
                ex.loading = false;
                match (cells, result) {
                    (Some(cells), _) => {
                        ex.cells = cells;
                        ex.complete = true;
                    }
                    (None, Err(e)) => ex.error = Some(e),
                    (None, Ok(_)) => {}
                }
            });
        });
    }

    /// Changes something that doesn't need new data (chart type, heatmap).
    pub fn set_display(self, f: impl FnOnce(&mut Explorer)) {
        self.explorer.update(|ex| {
            if let Some(ex) = ex.as_mut() {
                f(ex);
                write_hash(ex);
            }
        });
    }
}

/// The view as kept in the URL: only what differs from the defaults, so a
/// hundred-dimension cube still has a short link.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ViewState {
    pub db: u32,
    pub cube: u32,
    pub rows: Vec<usize>,
    pub cols: Vec<usize>,
    /// Filters moved off their top element: `(dimension, element)`.
    pub filters: Vec<(usize, u32)>,
    pub expanded: Vec<(usize, u32)>,
    pub show: Show,
    pub heatmap: bool,
    pub show_empty: bool,
}

impl ViewState {
    fn of(ex: &Explorer) -> Option<Self> {
        let p = ex.pivot.as_ref()?;
        Some(Self {
            db: ex.db,
            cube: ex.cube.id,
            rows: p.rows.clone(),
            cols: p.cols.clone(),
            filters: p.filters().into_iter().filter(|&d| p.fixed[d] != p.defaults[d]).map(|d| (d, p.fixed[d])).collect(),
            expanded: p.expanded.iter().enumerate().flat_map(|(d, set)| set.iter().map(move |&e| (d, e))).collect(),
            show: ex.show,
            heatmap: ex.heatmap,
            show_empty: !p.hide_empty,
        })
    }

    fn apply(&self, p: &mut Pivot, hier: &[Hierarchy]) {
        let n = hier.len();
        let valid = |d: &usize| *d < n;
        if !self.rows.is_empty() && self.rows.iter().all(valid) && self.cols.iter().all(valid) {
            p.rows = self.rows.clone();
            p.cols = self.cols.clone();
            for set in p.expanded.iter_mut() {
                set.clear();
            }
        }
        for &(d, e) in &self.filters {
            if d < n {
                p.fixed[d] = e;
            }
        }
        for &(d, e) in &self.expanded {
            if d < n {
                p.expanded[d].insert(e);
            }
        }
        p.hide_empty = !self.show_empty;
    }

    /// `db=2&cube=5&r=0&c=1&f=3:17,4:2&x=0:12&v=line&heat=1&empty=1`
    pub fn to_hash(&self) -> String {
        let list = |v: &[usize]| v.iter().map(usize::to_string).collect::<Vec<_>>().join(".");
        let pairs = |v: &[(usize, u32)]| v.iter().map(|(d, e)| format!("{d}:{e}")).collect::<Vec<_>>().join(",");
        let mut s = format!("db={}&cube={}&r={}&c={}", self.db, self.cube, list(&self.rows), list(&self.cols));
        if !self.filters.is_empty() {
            s += &format!("&f={}", pairs(&self.filters));
        }
        if !self.expanded.is_empty() {
            s += &format!("&x={}", pairs(&self.expanded));
        }
        if self.show != Show::Table {
            s += &format!("&v={}", self.show.key());
        }
        if self.heatmap {
            s += "&heat=1";
        }
        if self.show_empty {
            s += "&empty=1";
        }
        s
    }

    pub fn from_hash(hash: &str) -> Option<Self> {
        let hash = hash.trim_start_matches('#');
        let mut v = ViewState::default();
        let (mut db, mut cube) = (None, None);
        let list = |s: &str| s.split('.').filter_map(|x| x.parse().ok()).collect::<Vec<usize>>();
        let pairs = |s: &str| s.split(',').filter_map(|p| p.split_once(':')).filter_map(|(d, e)| Some((d.parse().ok()?, e.parse().ok()?))).collect::<Vec<(usize, u32)>>();
        for part in hash.split('&') {
            let Some((k, val)) = part.split_once('=') else { continue };
            match k {
                "db" => db = val.parse().ok(),
                "cube" => cube = val.parse().ok(),
                "r" => v.rows = list(val),
                "c" => v.cols = list(val),
                "f" => v.filters = pairs(val),
                "x" => v.expanded = pairs(val),
                "v" => v.show = Show::parse(val),
                "heat" => v.heatmap = val == "1",
                "empty" => v.show_empty = val == "1",
                _ => {}
            }
        }
        v.db = db?;
        v.cube = cube?;
        Some(v)
    }
}

fn write_hash(ex: &Explorer) {
    let Some(view) = ViewState::of(ex) else { return };
    let hash = format!("#{}", view.to_hash());
    if let Some(window) = web_sys::window() {
        if let Ok(history) = window.history() {
            let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&hash));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_survives_the_url() {
        let v = ViewState { db: 2, cube: 5, rows: vec![0, 2], cols: vec![1], filters: vec![(3, 17)], expanded: vec![(0, 0), (2, 4)], show: Show::Line, heatmap: true, show_empty: false };
        let hash = v.to_hash();
        assert_eq!(hash, "db=2&cube=5&r=0.2&c=1&f=3:17&x=0:0,2:4&v=line&heat=1");
        assert_eq!(ViewState::from_hash(&format!("#{hash}")), Some(v));
        assert_eq!(ViewState::from_hash("#nothing"), None);
    }
}
