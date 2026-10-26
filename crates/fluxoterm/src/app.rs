//! fluxoterm's state and behavior, free of I/O: `on_key` and `on_msg`
//! change the state and return the requests (`Cmd`) to run; `main`
//! executes them and feeds the results back as `Msg`s. That keeps all of
//! it testable without a terminal or a server.

use crate::client::Result;
use crate::stats::{self, StatsState};
use crate::tree::Hierarchy;
use fluxolap_api_types::{CellDto, CubeDto, DatabaseDto, DimensionDto, ElementDto, ObjectType, ServerDto, ServerStats, Value};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// How often the browser re-reads the server (and a grid its cells, if it
/// refreshes on its own).
pub const REFRESH_EVERY: Duration = Duration::from_secs(5);
/// A grid refreshes on its own by default only for cubes up to this many
/// stored cells: a re-read makes the server compute the visible window again,
/// which costs more the bigger the cube.
pub const AUTO_REFRESH_MAX_CELLS: usize = 1_000_000;
/// How long a status message stays up.
const STATUS_FOR: Duration = Duration::from_secs(6);

/// A request for `main` to run.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    LoadServer,
    /// The server's memory and statistics (not counted as busy: it polls).
    LoadStats,
    LoadDatabases,
    LoadContents { db: u32 },
    /// A dimension's elements; `roots_only` for just its top-level ones.
    LoadElements { db: u32, dim: u32, roots_only: bool },
    LoadCells { db: u32, cube: u32, area: Vec<Vec<u32>>, token: u64 },
    WriteCell { db: u32, cube: u32, path: Vec<u32>, value: Option<Value> },
    ClearCube { db: u32, cube: u32 },
    DeleteDatabase { db: u32 },
    DeleteDimension { db: u32, dim: u32 },
    DeleteCube { db: u32, cube: u32 },
    Rename { target: Target, name: String },
    Import { path: String },
    Export { db: u32, path: String },
    /// Look for database files (`*.bin.zst`) in these directories.
    FindFiles { dirs: Vec<String> },
}

/// A database file found on disk, offered for import.
#[derive(Debug, Clone, PartialEq)]
pub struct DbFile {
    pub path: String,
    /// The database name, from the file name (`db-3-Climate.bin.zst` -> `Climate`).
    pub name: String,
    pub size: u64,
}

/// A line of the import picker.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerEntry {
    /// Every listed file whose database isn't on the server yet.
    All(Vec<String>),
    File { file: DbFile, loaded: bool },
}

/// Something to rename or delete.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Database(u32),
    Dimension(u32, u32),
    Cube(u32, u32),
}

/// A finished request.
#[derive(Debug)]
pub enum Msg {
    Server(Result<ServerDto>),
    /// `None`: the server has no statistics (too old).
    Stats(Result<Option<Box<ServerStats>>>),
    Databases(Result<Vec<DatabaseDto>>),
    Contents { db: u32, result: Result<(Vec<DimensionDto>, Vec<CubeDto>)> },
    Elements { db: u32, dim: u32, roots_only: bool, result: Result<Vec<ElementDto>> },
    Cells { token: u64, result: Result<Vec<CellDto>> },
    /// A change went through (`Ok` carries a message for the status line)
    /// or failed; `then` says what to re-read.
    Done { result: Result<String>, then: After },
    Files(Vec<DbFile>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum After {
    Refresh,
    ReloadCells,
}

/// Like vim: VIEW (the default) only looks; EDIT (`i`, back with `Esc`)
/// is needed for anything that changes the server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    View,
    Edit,
}

/// A jump by `gg`, `G`, `Ctrl-d`, `Ctrl-u`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Nav {
    Top,
    Bottom,
    HalfDown,
    HalfUp,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pane {
    Databases,
    Contents,
}

/// A row of the browser's middle pane.
#[derive(Debug, Clone)]
pub enum Item {
    Cube(CubeDto),
    Dimension(DimensionDto),
}

#[derive(Debug, Default)]
pub struct Browser {
    pub databases: Vec<DatabaseDto>,
    pub db_sel: usize,
    pub focus: Option<Pane>,
    /// The selected database's dimensions and cubes, once loaded.
    pub contents: Option<(u32, Vec<DimensionDto>, Vec<CubeDto>)>,
    pub item_sel: usize,
    /// Show the automatic system/attribute dimensions and cubes.
    pub show_system: bool,
}

impl Browser {
    pub fn selected_db(&self) -> Option<&DatabaseDto> {
        self.databases.get(self.db_sel)
    }

    /// The middle pane: cubes, then dimensions (hiding automatic ones
    /// unless `show_system`).
    pub fn items(&self) -> Vec<Item> {
        let Some((db, dims, cubes)) = &self.contents else { return Vec::new() };
        if Some(*db) != self.selected_db().map(|d| d.id) {
            return Vec::new();
        }
        let shown = |t: ObjectType| self.show_system || matches!(t, ObjectType::Normal | ObjectType::UserInfo);
        let mut items: Vec<Item> = cubes.iter().filter(|c| shown(c.cube_type)).cloned().map(Item::Cube).collect();
        items.extend(dims.iter().filter(|d| shown(d.dim_type)).cloned().map(Item::Dimension));
        items
    }

    pub fn dimension_name(&self, id: u32) -> &str {
        self.contents.as_ref().and_then(|(_, dims, _)| dims.iter().find(|d| d.id == id)).map_or("?", |d| d.name.as_str())
    }
}

#[derive(Debug)]
pub struct DimView {
    pub db: u32,
    pub dim: DimensionDto,
    pub hierarchy: Option<Hierarchy>,
    pub expanded: HashSet<u32>,
    pub sel: usize,
    pub scroll: usize,
}

impl DimView {
    pub fn rows(&self) -> Vec<(u32, usize)> {
        self.hierarchy.as_ref().map_or_else(Vec::new, |h| h.visible(&self.expanded))
    }
}

/// The cube grid: two dimensions as rows and columns, one element each for
/// the others ("fixed").
#[derive(Debug)]
pub struct Grid {
    pub db: u32,
    pub cube: CubeDto,
    /// The cube's dimensions, in coordinate order.
    pub dims: Vec<DimensionDto>,
    /// Elements per dimension, as far as loaded: every element for axis
    /// dimensions (and filters once stepped through), otherwise only the
    /// top-level ones -- a cube can have a hundred dimensions and more.
    pub hier: Vec<Option<Hierarchy>>,
    /// Whether `hier[d]` holds every element of dimension `d`.
    pub complete: Vec<bool>,
    /// The element each dimension started on (its first top-level one).
    pub defaults: Vec<u32>,
    /// A `[`/`]` step waiting for its dimension's elements to load.
    pending_step: Option<(usize, bool)>,
    pub row_dim: usize,
    /// `None` for a one-dimensional cube.
    pub col_dim: Option<usize>,
    /// Selected element per dimension (used for the non-axis ones).
    pub fixed: Vec<u32>,
    /// Which non-axis dimension `[`/`]` changes (index into `dims`).
    pub fixed_focus: Option<usize>,
    pub expanded_rows: HashSet<u32>,
    pub expanded_cols: HashSet<u32>,
    pub cur_row: usize,
    pub cur_col: usize,
    pub top_row: usize,
    pub left_col: usize,
    /// How many rows/columns the last draw had room for.
    pub viewport: (usize, usize),
    /// Known cell values by full path (element ids in cube dimension order).
    pub cells: HashMap<Vec<u32>, Option<Value>>,
    /// Increments per cell request; answers to older ones are ignored.
    pub token: u64,
    /// The window last requested, so it isn't requested again while in flight.
    pub requested: Option<(Vec<u32>, Vec<u32>, Vec<u32>)>,
    /// Hide rows without a value in any column (zero suppression).
    pub hide_empty: bool,
    /// Read the window again even though its values are known: they may have
    /// changed on the server (set every `REFRESH_EVERY`).
    pub reread: bool,
    /// Re-read the visible values every `REFRESH_EVERY` (`a` toggles; on by
    /// default for cubes up to `AUTO_REFRESH_MAX_CELLS`).
    pub auto_refresh: bool,
}

/// Largest area (rows x columns) the grid loads whole to hide empty rows;
/// beyond it, every row is shown and only the visible window is loaded.
pub const SUPPRESSION_LIMIT: usize = 250_000;

impl Grid {
    pub fn ready(&self) -> bool {
        !self.fixed.is_empty() && self.complete[self.row_dim] && self.col_dim.is_none_or(|c| self.complete[c])
    }

    pub fn row_hier(&self) -> &Hierarchy {
        self.hier[self.row_dim].as_ref().expect("grid is ready")
    }

    pub fn col_hier(&self) -> Option<&Hierarchy> {
        self.col_dim.map(|c| self.hier[c].as_ref().expect("grid is ready"))
    }

    /// Every row of the expanded hierarchy, empty or not.
    pub fn all_rows(&self) -> Vec<(u32, usize)> {
        self.row_hier().visible(&self.expanded_rows)
    }

    /// Whether empty rows are being hidden: asked for, and the whole area
    /// is small enough to load.
    pub fn suppressing(&self) -> bool {
        self.hide_empty && self.all_rows().len() * self.cols().len() <= SUPPRESSION_LIMIT
    }

    /// The rows shown: all of them, or -- when hiding empty rows -- those
    /// with a value (or not yet loaded) in some column.
    pub fn rows(&self) -> Vec<(u32, usize)> {
        let rows = self.all_rows();
        if !self.suppressing() {
            return rows;
        }
        let cols = self.cols();
        rows.into_iter()
            .filter(|&(r, _)| cols.iter().any(|&(c, _)| !matches!(self.cells.get(&self.path(r, c)), Some(None))))
            .collect()
    }

    pub fn cols(&self) -> Vec<(u32, usize)> {
        self.col_hier().map_or_else(|| vec![(u32::MAX, 0)], |h| h.visible(&self.expanded_cols))
    }

    /// The non-axis dimensions, in cube order.
    pub fn fixed_dims(&self) -> Vec<usize> {
        (0..self.dims.len()).filter(|&d| d != self.row_dim && Some(d) != self.col_dim).collect()
    }

    /// The full cell path for a row and column element.
    pub fn path(&self, row: u32, col: u32) -> Vec<u32> {
        let mut path = self.fixed.clone();
        path[self.row_dim] = row;
        if let Some(c) = self.col_dim {
            path[c] = col;
        }
        path
    }

    fn cursor_path(&self) -> Option<Vec<u32>> {
        let row = self.rows().get(self.cur_row)?.0;
        let col = self.cols().get(self.cur_col)?.0;
        Some(self.path(row, col))
    }

    fn cursor_is_consolidated(&self) -> bool {
        let Some(path) = self.cursor_path() else { return false };
        path.iter().zip(&self.hier).any(|(&e, h)| h.as_ref().is_some_and(|h| h.is_consolidated(e)))
    }

    /// Sets defaults once every dimension's elements are known: each
    /// non-axis dimension on its top element, and the first consolidated
    /// row and column element opened, so the grid starts on real data.
    fn init(&mut self) {
        self.fixed = self.hier.iter().map(|h| h.as_ref().expect("all loaded").roots().first().copied().unwrap_or(0)).collect();
        self.defaults = self.fixed.clone();
        self.fixed_focus = self.fixed_dims().first().copied();
        self.expanded_rows = opened_root(self.row_hier());
        self.expanded_cols = self.col_hier().map(opened_root).unwrap_or_default();
    }

    /// Puts dimension `d` on rows (or columns); if it's on the other axis,
    /// the axes swap. Returns whether its elements still need loading.
    fn set_axis(&mut self, on_rows: bool, d: usize) -> bool {
        if on_rows {
            if d == self.row_dim {
                return false;
            }
            if Some(d) == self.col_dim {
                self.col_dim = Some(self.row_dim);
            }
            self.row_dim = d;
        } else {
            let Some(col) = self.col_dim else { return false };
            if d == col {
                return false;
            }
            if d == self.row_dim {
                self.row_dim = col;
            }
            self.col_dim = Some(d);
        }
        self.expanded_rows = if self.complete[self.row_dim] { opened_root(self.row_hier()) } else { HashSet::new() };
        self.expanded_cols = match self.col_dim {
            Some(c) if self.complete[c] => opened_root(self.hier[c].as_ref().expect("loaded")),
            _ => HashSet::new(),
        };
        if self.fixed_focus.is_none_or(|f| f == self.row_dim || Some(f) == self.col_dim) {
            self.fixed_focus = self.fixed_dims().first().copied();
        }
        self.reset_view();
        !self.complete[d]
    }

    /// Moves filter dimension `d` to its next (or previous) element.
    fn step_fixed(&mut self, d: usize, forward: bool) {
        let all = self.hier[d].as_ref().expect("loaded").all();
        if let Some(i) = all.iter().position(|&e| e == self.fixed[d]) {
            let delta = if forward { 1 } else { all.len() - 1 };
            self.fixed[d] = all[(i + delta) % all.len()];
        }
    }

    fn reset_view(&mut self) {
        self.cur_row = 0;
        self.cur_col = 0;
        self.top_row = 0;
        self.left_col = 0;
        self.requested = None;
    }

    /// The request for the visible window's cells, if any are unknown and
    /// not already asked for.
    pub fn fetch(&mut self) -> Option<Cmd> {
        if !self.ready() {
            return None;
        }
        // Hiding empty rows needs every row's values; otherwise the visible
        // window is enough.
        let (rows, cols): (Vec<u32>, Vec<u32>) = if self.suppressing() {
            (self.all_rows().iter().map(|r| r.0).collect(), self.cols().iter().map(|c| c.0).collect())
        } else {
            (
                self.rows().iter().skip(self.top_row).take(self.viewport.0.max(1)).map(|r| r.0).collect(),
                self.cols().iter().skip(self.left_col).take(self.viewport.1.max(1)).map(|c| c.0).collect(),
            )
        };
        let missing = rows.iter().any(|&r| cols.iter().any(|&c| !self.cells.contains_key(&self.path(r, c))));
        let window = (rows.clone(), cols.clone(), self.fixed.clone());
        if !self.reread && (!missing || self.requested.as_ref() == Some(&window)) {
            return None;
        }
        // A re-read keeps the old values on screen until the new ones arrive.
        self.reread = false;
        self.requested = Some(window);
        self.token += 1;
        let mut area: Vec<Vec<u32>> = self.fixed.iter().map(|&e| vec![e]).collect();
        area[self.row_dim] = rows;
        if let Some(c) = self.col_dim {
            area[c] = cols;
        }
        Some(Cmd::LoadCells { db: self.db, cube: self.cube.id, area, token: self.token })
    }
}

#[derive(Debug)]
pub enum View {
    Browser,
    Dimension(DimView),
    Grid(Box<Grid>),
    Stats(StatsView),
}

/// The server statistics screen (`S`, `:stats`); `back` is the view it
/// was opened from, shown again when it closes.
#[derive(Debug)]
pub struct StatsView {
    pub sel: usize,
    /// Show the automatic system/attribute cubes too.
    pub show_system: bool,
    pub back: Box<View>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputKind {
    Rename(Target),
    Export { db: u32 },
    EditCell { db: u32, cube: u32, path: Vec<u32> },
}

#[derive(Debug)]
pub enum Popup {
    Confirm { text: String, cmd: Cmd },
    Input { title: String, value: String, kind: InputKind },
    /// Import: files found on disk (`None` while searching), filtered by
    /// `input`, which is also taken as a path when nothing matches.
    /// `welcome` when opened because the server has no databases.
    Picker { files: Option<Vec<DbFile>>, sel: usize, input: String, welcome: bool },
    /// Choose a cube dimension (searchable) for rows, columns or the filter to change.
    DimPicker { purpose: DimPurpose, query: String, sel: usize },
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DimPurpose {
    Rows,
    Columns,
    Filter,
}

#[derive(Debug)]
pub struct App {
    pub server_url: String,
    pub server: Option<ServerDto>,
    /// Why the server can't be reached, while it can't.
    pub offline: Option<String>,
    pub browser: Browser,
    pub view: View,
    pub popup: Option<Popup>,
    pub status: Option<(String, bool, Instant)>,
    /// Requests in flight (drives the spinner).
    pub busy: usize,
    pub quit: bool,
    pub last_refresh: Instant,
    /// Where to look for database files to import (set by `main`).
    pub search_dirs: Vec<String>,
    /// `:` commands to run on start (`fluxoterm +stats`, set by `main`).
    pub startup_commands: Vec<String>,
    /// Whether the first database list has arrived (the empty-server
    /// offer is made only then).
    started: bool,
    /// Open the picker as soon as files are found (empty server on start).
    welcome_pending: bool,
    pub mode: Mode,
    /// The `:` command line while it's being typed.
    pub command: Option<String>,
    /// The first key of a two-key command (`gg`, `dd`).
    pending: Option<char>,
    /// The server's memory and statistics, polled.
    pub stats: StatsState,
    /// A cube (database, cube id) to open once its database's contents are
    /// loaded (Enter on a cube in the stats view).
    open_after_contents: Option<(u32, u32)>,
}

impl App {
    pub fn new(server_url: String) -> Self {
        Self {
            server_url,
            server: None,
            offline: None,
            browser: Browser::default(),
            view: View::Browser,
            popup: None,
            status: None,
            busy: 0,
            quit: false,
            last_refresh: Instant::now(),
            search_dirs: Vec::new(),
            startup_commands: Vec::new(),
            started: false,
            welcome_pending: false,
            mode: Mode::View,
            command: None,
            pending: None,
            stats: StatsState::default(),
            open_after_contents: None,
        }
    }

    /// The picker's lines for `files` filtered by `input`: first "load
    /// all" (when some aren't on the server yet and nothing is typed),
    /// then each file, marked if a database with its name exists.
    pub fn picker_entries(&self, files: &[DbFile], input: &str) -> Vec<PickerEntry> {
        let needle = input.trim().to_lowercase();
        let loaded = |f: &DbFile| self.browser.databases.iter().any(|d| d.name == f.name);
        let matching: Vec<&DbFile> = files.iter().filter(|f| needle.is_empty() || f.name.to_lowercase().contains(&needle) || f.path.to_lowercase().contains(&needle)).collect();
        let mut entries = Vec::new();
        let missing: Vec<String> = matching.iter().filter(|f| !loaded(f)).map(|f| f.path.clone()).collect();
        if needle.is_empty() && missing.len() > 1 {
            entries.push(PickerEntry::All(missing));
        }
        entries.extend(matching.into_iter().map(|f| PickerEntry::File { file: f.clone(), loaded: loaded(f) }));
        entries
    }

    fn open_picker(&mut self) -> Vec<Cmd> {
        self.popup = Some(Popup::Picker { files: None, sel: 0, input: String::new(), welcome: false });
        let dirs = self.search_dirs.clone();
        self.issue(vec![Cmd::FindFiles { dirs }])
    }

    /// What to request on start.
    pub fn start(&mut self) -> Vec<Cmd> {
        self.browser.focus = Some(Pane::Databases);
        let mut cmds = self.issue(vec![Cmd::LoadServer, Cmd::LoadDatabases]);
        cmds.extend(self.poll_stats(Instant::now()));
        for command in std::mem::take(&mut self.startup_commands) {
            cmds.extend(self.run_command(&command));
        }
        cmds
    }

    fn issue(&mut self, cmds: Vec<Cmd>) -> Vec<Cmd> {
        self.busy += cmds.len();
        cmds
    }

    fn info(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), false, Instant::now()));
    }

    fn error(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), true, Instant::now()));
    }

    /// The status line, while it's fresh.
    pub fn status_line(&self) -> Option<(&str, bool)> {
        self.status.as_ref().filter(|(_, _, at)| at.elapsed() < STATUS_FOR).map(|(t, e, _)| (t.as_str(), *e))
    }

    /// Periodic work: the server's statistics (every few seconds, in any
    /// view), and re-reading the server every `REFRESH_EVERY` in the browser.
    pub fn tick(&mut self) -> Vec<Cmd> {
        let mut cmds: Vec<Cmd> = self.poll_stats(Instant::now()).into_iter().collect();
        if self.busy == 0 && self.popup.is_none() && self.last_refresh.elapsed() >= REFRESH_EVERY {
            match &mut self.view {
                View::Browser => cmds.extend(self.refresh()),
                // Values changed elsewhere (fluxoscope, Calc, the API) show up:
                // the next draw reads the visible window again.
                View::Grid(grid) if grid.auto_refresh => {
                    self.last_refresh = Instant::now();
                    grid.reread = true;
                }
                _ => {}
            }
        }
        cmds
    }

    /// A stats request if one is due (faster while the stats view is open).
    /// Not counted in `busy`, so polling doesn't spin the spinner.
    fn poll_stats(&mut self, now: Instant) -> Option<Cmd> {
        if !self.stats.due(matches!(self.view, View::Stats(_)), now) {
            return None;
        }
        self.stats.pending = true;
        self.stats.last_poll = Some(now);
        Some(Cmd::LoadStats)
    }

    fn on_stats(&mut self, result: Result<Option<Box<ServerStats>>>) {
        self.stats.pending = false;
        match result {
            Ok(Some(stats)) => self.stats.record(*stats, Instant::now()),
            Ok(None) => self.stats.supported = false,
            // Offline: the header says so; keep polling.
            Err(_) => self.stats.latest = None,
        }
    }

    /// `S` / `:stats`: opens the statistics, or closes them again.
    fn toggle_stats(&mut self) -> Vec<Cmd> {
        match std::mem::replace(&mut self.view, View::Browser) {
            View::Stats(view) => self.view = *view.back,
            other => {
                self.view = View::Stats(StatsView { sel: 0, show_system: false, back: Box::new(other) });
                // Read them right away rather than at the next slow tick.
                self.stats.last_poll = None;
                return self.poll_stats(Instant::now()).into_iter().collect();
            }
        }
        Vec::new()
    }

    fn refresh(&mut self) -> Vec<Cmd> {
        self.last_refresh = Instant::now();
        // The selected database's contents load once the list is back
        // (`Msg::Databases`), so a just-deleted one is never asked for.
        let mut cmds = vec![Cmd::LoadDatabases];
        if self.server.is_none() {
            cmds.push(Cmd::LoadServer);
        }
        self.issue(cmds)
    }

    /// Cell requests the current screen needs; call after each draw.
    pub fn after_draw(&mut self) -> Vec<Cmd> {
        let cmd = match &mut self.view {
            View::Grid(grid) => grid.fetch(),
            _ => None,
        };
        self.issue(cmd.into_iter().collect())
    }

    // -----------------------------------------------------------------
    // Results
    // -----------------------------------------------------------------

    pub fn on_msg(&mut self, msg: Msg) -> Vec<Cmd> {
        // Stats polling isn't counted in `busy` (see `poll_stats`).
        if !matches!(msg, Msg::Stats(_)) {
            self.busy = self.busy.saturating_sub(1);
        }
        match msg {
            Msg::Stats(result) => self.on_stats(result),
            Msg::Server(Ok(server)) => {
                self.server = Some(server);
                self.offline = None;
            }
            Msg::Server(Err(e)) => self.offline = Some(e.0),
            Msg::Databases(Ok(databases)) => {
                self.offline = None;
                if !self.started {
                    self.started = true;
                    if databases.is_empty() && !self.search_dirs.is_empty() {
                        self.welcome_pending = true;
                        let dirs = self.search_dirs.clone();
                        self.browser.databases = databases;
                        return self.issue(vec![Cmd::FindFiles { dirs }]);
                    }
                }
                let previous = self.browser.selected_db().map(|d| d.id);
                self.browser.databases = databases;
                self.browser.db_sel = previous
                    .and_then(|id| self.browser.databases.iter().position(|d| d.id == id))
                    .unwrap_or(self.browser.db_sel)
                    .min(self.browser.databases.len().saturating_sub(1));
                if let Some(db) = self.browser.selected_db().map(|d| d.id) {
                    return self.issue(vec![Cmd::LoadContents { db }]);
                }
                self.browser.contents = None;
            }
            Msg::Databases(Err(e)) => {
                self.offline = Some(e.0);
                self.server = None;
            }
            Msg::Contents { db, result: Ok((dims, cubes)) } => {
                if self.browser.selected_db().map(|d| d.id) == Some(db) {
                    let open = self.open_after_contents.take_if(|(d, _)| *d == db).and_then(|(_, cube)| cubes.iter().find(|c| c.id == cube).cloned());
                    self.browser.contents = Some((db, dims, cubes));
                    self.browser.item_sel = self.browser.item_sel.min(self.browser.items().len().saturating_sub(1));
                    if let Some(cube) = open {
                        return self.open_grid(db, cube);
                    }
                }
            }
            Msg::Contents { db, result: Err(e) } => {
                // A database deleted meanwhile (here or elsewhere) is no error.
                if self.browser.databases.iter().any(|d| d.id == db) {
                    self.error(e.0);
                }
            }
            Msg::Elements { db, dim, roots_only, result } => return self.on_elements(db, dim, roots_only, result),
            Msg::Cells { token, result } => {
                if let View::Grid(grid) = &mut self.view {
                    if token == grid.token {
                        match result {
                            Ok(cells) => {
                                for cell in cells {
                                    grid.cells.insert(cell.path, cell.value);
                                }
                            }
                            Err(e) => {
                                self.error(e.0);
                                return Vec::new();
                            }
                        }
                    }
                }
            }
            Msg::Files(found) => {
                if let Some(Popup::Picker { files, sel, .. }) = &mut self.popup {
                    *files = Some(found);
                    *sel = 0;
                } else if std::mem::take(&mut self.welcome_pending) && !found.is_empty() && self.popup.is_none() {
                    self.popup = Some(Popup::Picker { files: Some(found), sel: 0, input: String::new(), welcome: true });
                }
            }
            Msg::Done { result, then } => {
                match result {
                    Ok(text) => self.info(text),
                    Err(e) => self.error(e.0),
                }
                match then {
                    After::Refresh => return self.refresh(),
                    After::ReloadCells => {
                        if let View::Grid(grid) = &mut self.view {
                            grid.cells.clear();
                            grid.requested = None;
                        }
                    }
                }
            }
        }
        Vec::new()
    }

    fn on_elements(&mut self, db: u32, dim: u32, roots_only: bool, result: Result<Vec<ElementDto>>) -> Vec<Cmd> {
        let elements = match result {
            Ok(e) => e,
            Err(e) => {
                self.error(e.0);
                return Vec::new();
            }
        };
        match &mut self.view {
            View::Dimension(view) if view.db == db && view.dim.id == dim => {
                let h = Hierarchy::new(elements);
                let roots = h.roots();
                if roots.len() == 1 && h.is_consolidated(roots[0]) {
                    view.expanded.insert(roots[0]);
                }
                view.hierarchy = Some(h);
            }
            View::Grid(grid) if grid.db == db => {
                let h = Hierarchy::new(elements);
                let mut completed = Vec::new();
                for i in 0..grid.dims.len() {
                    if grid.dims[i].id != dim || (roots_only && grid.complete[i]) {
                        continue;
                    }
                    grid.hier[i] = Some(h.clone());
                    if !roots_only {
                        grid.complete[i] = true;
                        completed.push(i);
                    }
                }
                if grid.fixed.is_empty() {
                    if grid.hier.iter().all(Option::is_some) {
                        grid.init();
                    }
                    return Vec::new();
                }
                for i in completed {
                    // An axis dimension that just finished loading opens like at the start.
                    if i == grid.row_dim && grid.expanded_rows.is_empty() {
                        grid.expanded_rows = opened_root(grid.row_hier());
                    }
                    if Some(i) == grid.col_dim && grid.expanded_cols.is_empty() {
                        grid.expanded_cols = opened_root(grid.hier[i].as_ref().expect("just loaded"));
                    }
                    if let Some((d, forward)) = grid.pending_step {
                        if d == i {
                            grid.pending_step = None;
                            grid.step_fixed(d, forward);
                        }
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    // -----------------------------------------------------------------
    // Keys
    // -----------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return Vec::new();
        }
        if self.popup.is_some() {
            return self.on_popup_key(key);
        }
        if self.command.is_some() {
            return self.on_command_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (self.pending.take(), key.code) {
            (Some('g'), KeyCode::Char('g')) => return self.navigate(Nav::Top),
            (Some('d'), KeyCode::Char('d')) => return self.delete_selected(),
            _ => {}
        }
        match key.code {
            KeyCode::Char('d') if ctrl => return self.navigate(Nav::HalfDown),
            KeyCode::Char('u') if ctrl => return self.navigate(Nav::HalfUp),
            KeyCode::Char(':') => {
                self.command = Some(String::new());
                return Vec::new();
            }
            KeyCode::Char('?') => {
                self.popup = Some(Popup::Help);
                return Vec::new();
            }
            KeyCode::Char(c @ ('g' | 'd')) => {
                self.pending = Some(c);
                return Vec::new();
            }
            KeyCode::Char('G') => return self.navigate(Nav::Bottom),
            KeyCode::Char('S') => return self.toggle_stats(),
            KeyCode::Char('i') if self.mode == Mode::View => {
                self.mode = Mode::Edit;
                self.info("EDIT mode: changes go to the server -- Esc returns to VIEW");
                return Vec::new();
            }
            KeyCode::Esc if self.mode == Mode::Edit => {
                self.mode = Mode::View;
                self.info("VIEW mode: read-only");
                return Vec::new();
            }
            _ => {}
        }
        match &self.view {
            View::Browser => self.on_browser_key(key),
            View::Dimension(_) => self.on_dimension_key(key),
            View::Grid(_) => self.on_grid_key(key),
            View::Stats(_) => self.on_stats_key(key),
        }
    }

    fn on_stats_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let rows = self.stats_rows();
        let View::Stats(view) = &mut self.view else { return Vec::new() };
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => return self.toggle_stats(),
            KeyCode::Up | KeyCode::Char('k') => view.sel = step(view.sel, -1, rows.len()),
            KeyCode::Down | KeyCode::Char('j') => view.sel = step(view.sel, 1, rows.len()),
            KeyCode::PageUp => view.sel = step(view.sel, -10, rows.len()),
            KeyCode::PageDown => view.sel = step(view.sel, 10, rows.len()),
            KeyCode::Char('.') => {
                view.show_system = !view.show_system;
                view.sel = 0;
            }
            KeyCode::Char('r') => {
                self.stats.last_poll = None;
                return self.poll_stats(Instant::now()).into_iter().collect();
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                let Some(stats) = &self.stats.latest else { return Vec::new() };
                let (db, cube) = match rows.get(view.sel) {
                    Some(&stats::Row::Database(d)) => (stats.databases[d].id, None),
                    Some(&stats::Row::Cube(d, c)) => (stats.databases[d].id, Some(stats.databases[d].cubes[c].id)),
                    None => return Vec::new(),
                };
                return self.show_in_browser(db, cube);
            }
            _ => {}
        }
        Vec::new()
    }

    /// The stats view's table lines, for the current reading.
    pub fn stats_rows(&self) -> Vec<stats::Row> {
        let show_system = matches!(&self.view, View::Stats(v) if v.show_system);
        self.stats.latest.as_ref().map_or_else(Vec::new, |s| stats::rows(s, show_system))
    }

    /// Selects database `db` in the browser and switches to it; with a
    /// `cube`, opens that cube's grid once the database's contents are in.
    fn show_in_browser(&mut self, db: u32, cube: Option<u32>) -> Vec<Cmd> {
        let Some(pos) = self.browser.databases.iter().position(|d| d.id == db) else {
            self.error("that database isn't in the list yet -- try again in a moment");
            return Vec::new();
        };
        self.view = View::Browser;
        let b = &mut self.browser;
        b.db_sel = pos;
        b.item_sel = 0;
        b.focus = Some(Pane::Databases);
        self.open_after_contents = cube.map(|c| (db, c));
        self.issue(vec![Cmd::LoadContents { db }])
    }

    fn on_popup_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let Some(popup) = self.popup.take() else { return Vec::new() };
        match popup {
            Popup::Help => Vec::new(),
            Popup::DimPicker { purpose, mut query, sel } => {
                let entries = self.dim_picker_entries(purpose, &query);
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => self.popup = Some(Popup::DimPicker { purpose, query, sel: step(sel, -1, entries.len()) }),
                    KeyCode::Down => self.popup = Some(Popup::DimPicker { purpose, query, sel: step(sel, 1, entries.len()) }),
                    KeyCode::Enter => match entries.get(sel) {
                        Some(&(d, _)) => return self.pick_dim(purpose, d),
                        None => self.popup = Some(Popup::DimPicker { purpose, query, sel }),
                    },
                    KeyCode::Backspace => {
                        query.pop();
                        self.popup = Some(Popup::DimPicker { purpose, query, sel: 0 });
                    }
                    KeyCode::Char(c) => {
                        query.push(c);
                        self.popup = Some(Popup::DimPicker { purpose, query, sel: 0 });
                    }
                    _ => self.popup = Some(Popup::DimPicker { purpose, query, sel }),
                }
                Vec::new()
            }
            Popup::Picker { files, sel, mut input, welcome } => {
                let entries = files.as_deref().map(|f| self.picker_entries(f, &input)).unwrap_or_default();
                match key.code {
                    KeyCode::Esc => return Vec::new(),
                    KeyCode::Up => {
                        self.popup = Some(Popup::Picker { files, sel: step(sel, -1, entries.len()), input, welcome });
                    }
                    KeyCode::Down => {
                        self.popup = Some(Popup::Picker { files, sel: step(sel, 1, entries.len()), input, welcome });
                    }
                    KeyCode::Enter => {
                        let cmds: Vec<Cmd> = match entries.get(sel) {
                            Some(PickerEntry::All(paths)) => paths.iter().map(|p| Cmd::Import { path: p.clone() }).collect(),
                            Some(PickerEntry::File { loaded: true, file }) => {
                                self.error(format!("'{}' is already on the server -- delete it first to load a fresh copy", file.name));
                                Vec::new()
                            }
                            Some(PickerEntry::File { file, .. }) => vec![Cmd::Import { path: file.path.clone() }],
                            None if !input.trim().is_empty() => vec![Cmd::Import { path: input.trim().to_string() }],
                            None => Vec::new(),
                        };
                        if cmds.is_empty() {
                            self.popup = Some(Popup::Picker { files, sel, input, welcome });
                        }
                        return self.issue(cmds);
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        self.popup = Some(Popup::Picker { files, sel: 0, input, welcome });
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
                        self.popup = Some(Popup::Picker { files, sel: 0, input, welcome });
                    }
                    _ => self.popup = Some(Popup::Picker { files, sel, input, welcome }),
                }
                Vec::new()
            }
            Popup::Confirm { text, cmd } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.issue(vec![cmd]),
                KeyCode::Char('n') | KeyCode::Esc => Vec::new(),
                _ => {
                    self.popup = Some(Popup::Confirm { text, cmd });
                    Vec::new()
                }
            },
            Popup::Input { title, mut value, kind } => match key.code {
                KeyCode::Esc => Vec::new(),
                KeyCode::Enter => self.submit(value, kind),
                KeyCode::Backspace => {
                    value.pop();
                    self.popup = Some(Popup::Input { title, value, kind });
                    Vec::new()
                }
                KeyCode::Char(c) => {
                    value.push(c);
                    self.popup = Some(Popup::Input { title, value, kind });
                    Vec::new()
                }
                _ => {
                    self.popup = Some(Popup::Input { title, value, kind });
                    Vec::new()
                }
            },
        }
    }

    fn submit(&mut self, value: String, kind: InputKind) -> Vec<Cmd> {
        let text = value.trim().to_string();
        let cmd = match kind {
            InputKind::Rename(target) if !text.is_empty() => Cmd::Rename { target, name: text },
            InputKind::Export { db } if !text.is_empty() => Cmd::Export { db, path: text },
            InputKind::EditCell { db, cube, path } => Cmd::WriteCell { db, cube, path, value: parse_value(&text) },
            _ => return Vec::new(),
        };
        self.issue(vec![cmd])
    }

    fn on_browser_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let b = &mut self.browser;
        let focus = b.focus.unwrap_or(Pane::Databases);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if focus == Pane::Contents && key.code == KeyCode::Esc {
                    b.focus = Some(Pane::Databases);
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => return self.move_browser(-1),
            KeyCode::Down | KeyCode::Char('j') => return self.move_browser(1),
            KeyCode::PageUp => return self.move_browser(-10),
            KeyCode::PageDown => return self.move_browser(10),
            KeyCode::Home => return self.navigate(Nav::Top),
            KeyCode::End => return self.navigate(Nav::Bottom),
            KeyCode::Tab | KeyCode::Right | KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Char('l') => {
                let leftward = matches!(key.code, KeyCode::Left | KeyCode::Char('h'));
                b.focus = Some(if focus == Pane::Databases && !b.items().is_empty() && !leftward { Pane::Contents } else { Pane::Databases });
            }
            KeyCode::Enter => {
                if focus == Pane::Databases {
                    if !b.items().is_empty() {
                        b.focus = Some(Pane::Contents);
                    }
                } else if let Some(db) = b.selected_db().map(|d| d.id) {
                    match b.items().get(b.item_sel).cloned() {
                        Some(Item::Cube(cube)) => return self.open_grid(db, cube),
                        Some(Item::Dimension(dim)) => return self.open_dimension(db, dim),
                        None => {}
                    }
                }
            }
            KeyCode::Char('r') => return self.refresh(),
            KeyCode::Char('.') => {
                b.show_system = !b.show_system;
                b.item_sel = 0;
                let text = if b.show_system { "showing system and attribute objects" } else { "hiding system and attribute objects" };
                self.info(text);
            }
            KeyCode::Delete => {
                if self.require_edit() {
                    self.confirm_delete();
                }
            }
            KeyCode::Char('c') => {
                if let (Some(db), Some(Item::Cube(cube))) = (b.selected_db().map(|d| d.id), b.items().get(b.item_sel).cloned()) {
                    if focus == Pane::Contents && self.require_edit() {
                        self.popup = Some(Popup::Confirm {
                            text: format!("Clear all {} cells of cube '{}'?", group(cube.cell_count as f64), cube.name),
                            cmd: Cmd::ClearCube { db, cube: cube.id },
                        });
                    }
                }
            }
            KeyCode::F(2) | KeyCode::Char('n') => {
                if self.require_edit() {
                    self.ask_rename();
                }
            }
            KeyCode::Char('o') => {
                if self.require_edit() {
                    return self.open_picker();
                }
            }
            KeyCode::Char('e') => self.ask_export(),
            _ => {}
        }
        Vec::new()
    }

    fn move_browser(&mut self, delta: isize) -> Vec<Cmd> {
        let b = &mut self.browser;
        match b.focus.unwrap_or(Pane::Databases) {
            Pane::Databases => {
                let before = b.db_sel;
                b.db_sel = step(b.db_sel, delta, b.databases.len());
                if b.db_sel != before {
                    b.item_sel = 0;
                    if let Some(db) = b.selected_db().map(|d| d.id) {
                        return self.issue(vec![Cmd::LoadContents { db }]);
                    }
                }
            }
            Pane::Contents => b.item_sel = step(b.item_sel, delta, b.items().len()),
        }
        Vec::new()
    }

    fn selected_target(&self) -> Option<(Target, String)> {
        let b = &self.browser;
        let db = b.selected_db()?;
        match b.focus.unwrap_or(Pane::Databases) {
            Pane::Databases => Some((Target::Database(db.id), db.name.clone())),
            Pane::Contents => match b.items().get(b.item_sel)? {
                Item::Cube(c) => Some((Target::Cube(db.id, c.id), c.name.clone())),
                Item::Dimension(d) => Some((Target::Dimension(db.id, d.id), d.name.clone())),
            },
        }
    }

    fn confirm_delete(&mut self) {
        let Some((target, name)) = self.selected_target() else { return };
        let (what, cmd) = match target {
            Target::Database(db) => ("database", Cmd::DeleteDatabase { db }),
            Target::Dimension(db, dim) => ("dimension", Cmd::DeleteDimension { db, dim }),
            Target::Cube(db, cube) => ("cube", Cmd::DeleteCube { db, cube }),
        };
        self.popup = Some(Popup::Confirm { text: format!("Delete {what} '{name}'? This can't be undone."), cmd });
    }

    fn ask_rename(&mut self) {
        let Some((target, name)) = self.selected_target() else { return };
        self.popup = Some(Popup::Input { title: format!("Rename '{name}' to"), value: name, kind: InputKind::Rename(target) });
    }

    fn open_dimension(&mut self, db: u32, dim: DimensionDto) -> Vec<Cmd> {
        let id = dim.id;
        self.view = View::Dimension(DimView { db, dim, hierarchy: None, expanded: HashSet::new(), sel: 0, scroll: 0 });
        self.issue(vec![Cmd::LoadElements { db, dim: id, roots_only: false }])
    }

    fn open_grid(&mut self, db: u32, cube: CubeDto) -> Vec<Cmd> {
        let cell_count = cube.cell_count;
        let dims: Vec<DimensionDto> = cube
            .dimensions
            .iter()
            .map(|id| {
                self.browser
                    .contents
                    .as_ref()
                    .and_then(|(_, dims, _)| dims.iter().find(|d| d.id == *id).cloned())
                    .unwrap_or(DimensionDto { id: *id, name: format!("#{id}"), dim_type: ObjectType::Normal, element_count: 0, modified: None })
            })
            .collect();
        let n = dims.len();
        // Every element only for the two axis dimensions; the others start
        // as filters on their top element, so only that is loaded now.
        let loads: Vec<(u32, bool)> = dims.iter().enumerate().map(|(i, d)| (d.id, i >= 2)).collect();
        self.view = View::Grid(Box::new(Grid {
            db,
            cube,
            dims,
            hier: vec![None; n],
            complete: vec![false; n],
            defaults: Vec::new(),
            pending_step: None,
            row_dim: 0,
            col_dim: (n > 1).then_some(1),
            fixed: Vec::new(),
            fixed_focus: None,
            expanded_rows: HashSet::new(),
            expanded_cols: HashSet::new(),
            cur_row: 0,
            cur_col: 0,
            top_row: 0,
            left_col: 0,
            viewport: (20, 6),
            cells: HashMap::new(),
            token: 0,
            requested: None,
            hide_empty: true,
            reread: false,
            auto_refresh: auto_refresh_default(cell_count),
        }));
        self.issue(loads.into_iter().map(|(dim, roots_only)| Cmd::LoadElements { db, dim, roots_only }).collect())
    }

    fn back_to_browser(&mut self) -> Vec<Cmd> {
        self.view = View::Browser;
        self.refresh()
    }

    fn on_dimension_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let View::Dimension(view) = &mut self.view else { return Vec::new() };
        let rows = view.rows();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => return self.back_to_browser(),
            KeyCode::Up | KeyCode::Char('k') => view.sel = step(view.sel, -1, rows.len()),
            KeyCode::Down | KeyCode::Char('j') => view.sel = step(view.sel, 1, rows.len()),
            KeyCode::PageUp => view.sel = step(view.sel, -20, rows.len()),
            KeyCode::PageDown => view.sel = step(view.sel, 20, rows.len()),
            KeyCode::Home => view.sel = 0,
            KeyCode::End => view.sel = rows.len().saturating_sub(1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('l') | KeyCode::Left | KeyCode::Char('h') => {
                if let Some(&(id, _)) = rows.get(view.sel) {
                    let open = !matches!(key.code, KeyCode::Left | KeyCode::Char('h'));
                    let toggle = matches!(key.code, KeyCode::Enter | KeyCode::Char(' '));
                    if toggle && view.expanded.contains(&id) || !open {
                        view.expanded.remove(&id);
                    } else if view.hierarchy.as_ref().is_some_and(|h| h.is_consolidated(id)) {
                        view.expanded.insert(id);
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_grid_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let View::Grid(grid) = &mut self.view else { return Vec::new() };
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            return self.back_to_browser();
        }
        if !grid.ready() {
            return Vec::new();
        }
        let (n_rows, n_cols) = (grid.rows().len(), grid.cols().len());
        let page = grid.viewport.0.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => grid.cur_row = step(grid.cur_row, -1, n_rows),
            KeyCode::Down | KeyCode::Char('j') => grid.cur_row = step(grid.cur_row, 1, n_rows),
            KeyCode::Left | KeyCode::Char('h') => grid.cur_col = step(grid.cur_col, -1, n_cols),
            KeyCode::Right | KeyCode::Char('l') => grid.cur_col = step(grid.cur_col, 1, n_cols),
            KeyCode::PageUp => grid.cur_row = step(grid.cur_row, -page, n_rows),
            KeyCode::PageDown => grid.cur_row = step(grid.cur_row, page, n_rows),
            KeyCode::Home => grid.cur_col = 0,
            KeyCode::End => grid.cur_col = n_cols.saturating_sub(1),
            KeyCode::Char(' ') => {
                if let Some(&(id, _)) = grid.rows().get(grid.cur_row) {
                    let consolidated = grid.row_hier().is_consolidated(id);
                    toggle(&mut grid.expanded_rows, id, consolidated);
                }
            }
            KeyCode::Char('x') => {
                if let (Some(&(id, _)), Some(h)) = (grid.cols().get(grid.cur_col), grid.col_hier()) {
                    let consolidated = h.is_consolidated(id);
                    toggle(&mut grid.expanded_cols, id, consolidated);
                }
            }
            KeyCode::Char('s') => {
                if let Some(c) = grid.col_dim {
                    grid.col_dim = Some(grid.row_dim);
                    grid.row_dim = c;
                    std::mem::swap(&mut grid.expanded_rows, &mut grid.expanded_cols);
                    grid.reset_view();
                }
            }
            KeyCode::Char('R') => self.popup = Some(Popup::DimPicker { purpose: DimPurpose::Rows, query: String::new(), sel: 0 }),
            KeyCode::Char('C') => {
                if grid.col_dim.is_some() {
                    self.popup = Some(Popup::DimPicker { purpose: DimPurpose::Columns, query: String::new(), sel: 0 });
                }
            }
            KeyCode::Char('f') => {
                if !grid.fixed_dims().is_empty() {
                    self.popup = Some(Popup::DimPicker { purpose: DimPurpose::Filter, query: String::new(), sel: 0 });
                }
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                if let Some(d) = grid.fixed_focus {
                    let forward = key.code == KeyCode::Char(']');
                    if grid.complete[d] {
                        grid.step_fixed(d, forward);
                    } else {
                        // Only its top element is known yet: load all, then step.
                        grid.pending_step = Some((d, forward));
                        let (db, dim) = (grid.db, grid.dims[d].id);
                        return self.issue(vec![Cmd::LoadElements { db, dim, roots_only: false }]);
                    }
                }
            }
            KeyCode::Char('r') => {
                grid.cells.clear();
                grid.requested = None;
            }
            KeyCode::Char('a') => {
                grid.auto_refresh = !grid.auto_refresh;
                let text = if grid.auto_refresh { "refreshing on its own every 5 s (a: off)" } else { "not refreshing on its own (a: on, r: once)" };
                self.info(text);
            }
            KeyCode::Char('z') => {
                grid.hide_empty = !grid.hide_empty;
                grid.requested = None;
                let text = if grid.hide_empty { "hiding empty rows" } else { "showing empty rows" };
                self.info(text);
            }
            KeyCode::Enter if self.mode == Mode::View => {
                // VIEW: Enter drills into the row, like Space.
                if let Some(&(id, _)) = grid.rows().get(grid.cur_row) {
                    let consolidated = grid.row_hier().is_consolidated(id);
                    toggle(&mut grid.expanded_rows, id, consolidated);
                }
            }
            KeyCode::Enter => {
                if let Some(path) = grid.cursor_path() {
                    let current = grid.cells.get(&path).cloned().flatten();
                    let value = match current {
                        Some(Value::Number(n)) => format_plain(n),
                        Some(Value::Text(t)) => t,
                        None => String::new(),
                    };
                    let title = if grid.cursor_is_consolidated() { "New value (splashes down to the base cells; empty clears them)" } else { "New value (empty clears the cell)" };
                    let (db, cube) = (grid.db, grid.cube.id);
                    self.popup = Some(Popup::Input { title: title.into(), value, kind: InputKind::EditCell { db, cube, path } });
                }
            }
            KeyCode::Delete => return self.clear_cursor_cell(),
            _ => {}
        }
        Vec::new()
    }
}

impl App {
    /// The dimensions a picker offers, filtered by `query` (by name):
    /// `(index into the cube's dimensions, where it is now)`.
    pub fn dim_picker_entries(&self, purpose: DimPurpose, query: &str) -> Vec<(usize, String)> {
        let View::Grid(grid) = &self.view else { return Vec::new() };
        let needle = query.trim().to_lowercase();
        (0..grid.dims.len())
            .filter(|&d| match purpose {
                DimPurpose::Rows => d != grid.row_dim,
                DimPurpose::Columns => Some(d) != grid.col_dim,
                DimPurpose::Filter => d != grid.row_dim && Some(d) != grid.col_dim,
            })
            .filter(|&d| needle.is_empty() || grid.dims[d].name.to_lowercase().contains(&needle))
            .map(|d| {
                let place = if d == grid.row_dim {
                    "on rows".to_string()
                } else if Some(d) == grid.col_dim {
                    "on columns".to_string()
                } else {
                    let name = grid.hier[d].as_ref().map_or("?", |h| h.name(grid.fixed[d]));
                    format!("filter: {name}")
                };
                (d, place)
            })
            .collect()
    }

    fn pick_dim(&mut self, purpose: DimPurpose, d: usize) -> Vec<Cmd> {
        let View::Grid(grid) = &mut self.view else { return Vec::new() };
        let needs_load = match purpose {
            DimPurpose::Rows => grid.set_axis(true, d),
            DimPurpose::Columns => grid.set_axis(false, d),
            DimPurpose::Filter => {
                grid.fixed_focus = Some(d);
                false
            }
        };
        if needs_load {
            let (db, dim) = (grid.db, grid.dims[d].id);
            return self.issue(vec![Cmd::LoadElements { db, dim, roots_only: false }]);
        }
        Vec::new()
    }

    /// True in EDIT mode; in VIEW mode says how to get there.
    fn require_edit(&mut self) -> bool {
        if self.mode == Mode::Edit {
            return true;
        }
        self.info("read-only in VIEW mode -- press i to edit");
        false
    }

    /// `gg`, `G`, `Ctrl-d`, `Ctrl-u` in the current view.
    fn navigate(&mut self, nav: Nav) -> Vec<Cmd> {
        let jump = |current: usize, len: usize, half: usize| -> usize {
            match nav {
                Nav::Top => 0,
                Nav::Bottom => len.saturating_sub(1),
                Nav::HalfDown => step(current, half as isize, len),
                Nav::HalfUp => step(current, -(half as isize), len),
            }
        };
        match &mut self.view {
            View::Browser => {
                let b = &self.browser;
                let (current, len) = match b.focus.unwrap_or(Pane::Databases) {
                    Pane::Databases => (b.db_sel, b.databases.len()),
                    Pane::Contents => (b.item_sel, b.items().len()),
                };
                let target = jump(current, len, 10);
                return self.move_browser(target as isize - current as isize);
            }
            View::Dimension(view) => view.sel = jump(view.sel, view.rows().len(), 10),
            View::Grid(grid) => {
                if grid.ready() {
                    let half = (grid.viewport.0 / 2).max(1);
                    grid.cur_row = jump(grid.cur_row, grid.rows().len(), half);
                }
            }
            View::Stats(_) => {
                let len = self.stats_rows().len();
                if let View::Stats(view) = &mut self.view {
                    view.sel = jump(view.sel, len, 10);
                }
            }
        }
        Vec::new()
    }

    /// `dd`: delete the selected database/cube/dimension (browser) or
    /// clear the cell (grid). EDIT mode only.
    fn delete_selected(&mut self) -> Vec<Cmd> {
        match &self.view {
            View::Browser => {
                if self.require_edit() {
                    self.confirm_delete();
                }
                Vec::new()
            }
            View::Grid(_) => self.clear_cursor_cell(),
            View::Dimension(_) | View::Stats(_) => Vec::new(),
        }
    }

    fn clear_cursor_cell(&mut self) -> Vec<Cmd> {
        if !self.require_edit() {
            return Vec::new();
        }
        let View::Grid(grid) = &self.view else { return Vec::new() };
        let Some(path) = grid.cursor_path() else { return Vec::new() };
        let cmd = Cmd::WriteCell { db: grid.db, cube: grid.cube.id, path, value: None };
        if grid.cursor_is_consolidated() {
            self.popup = Some(Popup::Confirm { text: "Clear every base cell under this consolidated cell?".into(), cmd });
            return Vec::new();
        }
        self.issue(vec![cmd])
    }

    /// The database the current view belongs to.
    fn current_db(&self) -> Option<(u32, String)> {
        let id = match &self.view {
            View::Browser => self.browser.selected_db()?.id,
            View::Dimension(v) => v.db,
            View::Grid(g) => g.db,
            View::Stats(v) => {
                let stats = self.stats.latest.as_ref()?;
                match self.stats_rows().get(v.sel)? {
                    stats::Row::Database(d) | stats::Row::Cube(d, _) => stats.databases[*d].id,
                }
            }
        };
        let name = self.browser.databases.iter().find(|d| d.id == id).map_or_else(|| format!("database-{id}"), |d| d.name.clone());
        Some((id, name))
    }

    fn ask_export(&mut self) {
        if let Some((id, name)) = self.current_db() {
            self.popup = Some(Popup::Input { title: format!("Export '{name}' to file"), value: format!("{name}.bin.zst"), kind: InputKind::Export { db: id } });
        }
    }

    fn on_command_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let Some(mut line) = self.command.take() else { return Vec::new() };
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => return self.run_command(&line),
            KeyCode::Backspace => {
                // Like vim: backspace on an empty line leaves command mode.
                if line.pop().is_some() {
                    self.command = Some(line);
                }
            }
            KeyCode::Char(c) => {
                line.push(c);
                self.command = Some(line);
            }
            _ => self.command = Some(line),
        }
        Vec::new()
    }

    /// Runs a `:` command.
    fn run_command(&mut self, line: &str) -> Vec<Cmd> {
        let line = line.trim();
        let (name, arg) = match line.split_once(char::is_whitespace) {
            Some((n, a)) => (n, Some(a.trim()).filter(|a| !a.is_empty())),
            None => (line, None),
        };
        match name {
            "" => {}
            "q" | "q!" | "qa" | "qa!" | "quit" => self.quit = true,
            "w" | "export" => match (self.current_db(), arg) {
                (Some((db, _)), Some(path)) => return self.issue(vec![Cmd::Export { db, path: path.to_string() }]),
                (Some(_), None) => self.ask_export(),
                (None, _) => self.error("no database selected"),
            },
            "e" | "load" => {
                if self.require_edit() {
                    return match arg {
                        Some(path) => self.issue(vec![Cmd::Import { path: path.to_string() }]),
                        None => self.open_picker(),
                    };
                }
            }
            "r" | "refresh" => {
                if let View::Grid(grid) = &mut self.view {
                    grid.cells.clear();
                    grid.requested = None;
                } else {
                    return self.refresh();
                }
            }
            "h" | "help" => self.popup = Some(Popup::Help),
            "auto" => match &mut self.view {
                View::Grid(grid) => {
                    grid.auto_refresh = match arg {
                        Some("on") => true,
                        Some("off") => false,
                        _ => !grid.auto_refresh,
                    };
                    let text = if grid.auto_refresh { "refreshing on its own every 5 s" } else { "not refreshing on its own" };
                    self.info(text);
                }
                _ => self.error(":auto works in a cube grid"),
            },
            "stats" => {
                if !matches!(self.view, View::Stats(_)) {
                    return self.toggle_stats();
                }
            }
            other => self.error(format!("not a command: :{other}  (:q, :w [file], :e [file], :r, :auto [on|off], :stats, :help)")),
        }
        Vec::new()
    }
}

/// Moves a selection by `delta`, clamped to `0..len`.
/// Whether a grid over a cube with `cell_count` stored cells refreshes on its own.
pub fn auto_refresh_default(cell_count: usize) -> bool {
    cell_count <= AUTO_REFRESH_MAX_CELLS
}

fn step(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
}

fn toggle(expanded: &mut HashSet<u32>, id: u32, consolidated: bool) {
    if !expanded.remove(&id) && consolidated {
        expanded.insert(id);
    }
}

/// A hierarchy's single consolidated top element, opened (so a grid
/// starts on World's regions rather than on World alone).
fn opened_root(h: &Hierarchy) -> HashSet<u32> {
    match h.roots().as_slice() {
        [root] if h.is_consolidated(*root) => HashSet::from([*root]),
        _ => HashSet::new(),
    }
}

/// Cell input: empty clears, a number is a number, anything else text.
pub fn parse_value(text: &str) -> Option<Value> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    match t.replace(',', "").parse::<f64>() {
        Ok(n) if !t.starts_with('+') => Some(Value::Number(n)),
        _ => Some(Value::Text(t.to_string())),
    }
}

/// A number the way it's typed back in: no grouping, no rounding.
fn format_plain(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// A number for display: whole numbers grouped by thousands, others with
/// four significant digits (at most two decimals from 1,000 up), so
/// 8231613565 reads 8,231,613,565 and 316.908333 reads 316.9.
pub fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return group(n);
    }
    let abs = n.abs();
    let decimals = if abs >= 1000.0 {
        2
    } else if abs == 0.0 {
        0
    } else {
        (3 - abs.log10().floor() as i32).clamp(0, 6) as usize
    };
    let s = format!("{n:.decimals$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    match s.split_once('.') {
        Some((int, frac)) => format!("{}.{frac}", group_str(int)),
        None => group_str(&s),
    }
}

/// How many decimals `format_number` shows for `n`.
pub fn decimals_of(n: f64) -> usize {
    format_number(n).split_once('.').map_or(0, |(_, frac)| frac.len())
}

/// `format_number`, padded with zeros to `decimals` decimals, so a
/// column's numbers line up on the decimal point.
pub fn format_aligned(n: f64, decimals: usize) -> String {
    let s = format_number(n);
    if decimals == 0 {
        return s;
    }
    match s.split_once('.') {
        Some((int, frac)) => format!("{int}.{frac:0<decimals$}"),
        None => format!("{s}.{}", "0".repeat(decimals)),
    }
}

/// A whole number grouped by thousands.
pub fn group(n: f64) -> String {
    group_str(&format!("{:.0}", n))
}

/// Groups by thousands from five digits up (10,000), so years and other
/// four-digit numbers stay plain: 2007, 1026.
fn group_str(int: &str) -> String {
    let (sign, digits) = int.strip_prefix('-').map_or(("", int), |d| ("-", d));
    if digits.len() <= 4 {
        return int.to_string();
    }
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_well() {
        assert_eq!(format_number(8231613565.0), "8,231,613,565");
        assert_eq!(format_number(316.9083333333333), "316.9");
        assert_eq!(format_number(0.692), "0.692");
        assert_eq!(format_number(-0.025), "-0.025");
        assert_eq!(format_number(1.1933333333333334), "1.193");
        assert_eq!(format_number(9111106.09), "9,111,106.09");
        assert_eq!(format_number(237391.84000000003), "237,391.84");
        assert_eq!(format_number(-73186.0), "-73,186");
        assert_eq!(format_number(2007.0), "2007");
        assert_eq!(format_number(1026.0), "1026");
        assert_eq!(format_number(6.101013), "6.101");
    }

    #[test]
    fn a_column_aligns_on_the_decimal_point() {
        assert_eq!(decimals_of(1929099.08), 2);
        assert_eq!(decimals_of(16689936.0), 0);
        assert_eq!(format_aligned(16689936.0, 2), "16,689,936.00");
        assert_eq!(format_aligned(14371.1, 2), "14,371.10");
        assert_eq!(format_aligned(9687.32, 2), "9687.32");
        assert_eq!(format_aligned(421.08, 0), "421.1");
    }

    #[test]
    fn typed_values_parse() {
        assert_eq!(parse_value(""), None);
        assert_eq!(parse_value("1,200.5"), Some(Value::Number(1200.5)));
        assert_eq!(parse_value("TRAPPIST-1"), Some(Value::Text("TRAPPIST-1".into())));
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<Cmd> {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn with_databases(n: u32) -> App {
        let mut app = App::new("http://x".into());
        app.start();
        app.on_msg(Msg::Databases(Ok((0..n).map(|id| DatabaseDto { id, name: format!("db{id}"), db_type: Default::default() }).collect())));
        app
    }

    #[test]
    fn vim_keys_move_and_modes_guard_changes() {
        let mut app = with_databases(30);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.browser.db_sel, 29);
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.browser.db_sel, 0);
        app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.browser.db_sel, 10);

        // dd in VIEW: refused.
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('d'));
        assert!(app.popup.is_none());
        assert!(app.status_line().unwrap().0.contains("read-only"));
        // dd in EDIT: asks.
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.mode, Mode::Edit);
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(&app.popup, Some(Popup::Confirm { cmd: Cmd::DeleteDatabase { db: 10 }, .. })));
        press(&mut app, KeyCode::Esc); // "no"
        press(&mut app, KeyCode::Esc); // back to VIEW
        assert_eq!(app.mode, Mode::View);
    }

    #[test]
    fn the_stats_view_opens_closes_and_leads_to_a_cube() {
        use crate::stats::tests::server_stats;
        let mut app = with_databases(2);
        assert!(app.stats.pending, "the stats are read at start");
        let busy = app.busy;
        app.on_msg(Msg::Stats(Ok(Some(Box::new(server_stats(10, 100))))));
        assert_eq!(app.busy, busy, "stats aren't counted as busy");
        assert_eq!(app.stats.header_memory(), Some((100 << 20, Some(1 << 30))));

        assert_eq!(press(&mut app, KeyCode::Char('S')), vec![Cmd::LoadStats], "read right away");
        assert!(matches!(app.view, View::Stats(_)));
        app.on_msg(Msg::Stats(Ok(Some(Box::new(server_stats(12, 120))))));
        // Largest first: Big, its cubes B and A, then Small and Tiny.
        assert_eq!(app.stats_rows().len(), 5);
        assert_eq!(app.current_db().unwrap().0, 1);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Cmd::LoadContents { db: 1 }]);
        assert!(matches!(app.view, View::Browser));
        assert_eq!(app.browser.db_sel, 1);

        // Its contents arrive: cube B (id 4) opens.
        let dim = DimensionDto { id: 7, name: "D".into(), dim_type: ObjectType::Normal, element_count: 2, modified: None };
        let cube = |id: u32, name: &str| CubeDto { id, name: name.into(), cube_type: ObjectType::Normal, dimensions: vec![7], cell_count: 10, modified: None };
        let cmds = app.on_msg(Msg::Contents { db: 1, result: Ok((vec![dim], vec![cube(3, "A"), cube(4, "B")])) });
        assert!(matches!(&app.view, View::Grid(g) if g.cube.id == 4));
        assert_eq!(cmds, vec![Cmd::LoadElements { db: 1, dim: 7, roots_only: false }]);

        // From the grid and back to it.
        press(&mut app, KeyCode::Char('S'));
        assert!(matches!(app.view, View::Stats(_)));
        press(&mut app, KeyCode::Char('q'));
        assert!(matches!(&app.view, View::Grid(g) if g.cube.id == 4));
        for c in ":stats".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.view, View::Stats(_)));
    }

    #[test]
    fn plus_stats_starts_in_the_stats_view() {
        let mut app = App::new("http://x".into());
        app.startup_commands = vec!["stats".into()];
        let cmds = app.start();
        assert!(matches!(&app.view, View::Stats(v) if matches!(*v.back, View::Browser)));
        assert_eq!(cmds.iter().filter(|c| **c == Cmd::LoadStats).count(), 1, "read once: {cmds:?}");
        press(&mut app, KeyCode::Char('q'));
        assert!(matches!(app.view, View::Browser), "q goes to the browser");
    }

    #[test]
    fn grids_of_small_cubes_refresh_on_their_own() {
        assert!(auto_refresh_default(304_369), "WorldPopulation");
        assert!(auto_refresh_default(AUTO_REFRESH_MAX_CELLS));
        assert!(!auto_refresh_default(AUTO_REFRESH_MAX_CELLS + 1), "a big cube: only on request");
    }

    #[test]
    fn an_old_server_without_stats_is_left_alone() {
        let mut app = with_databases(1);
        app.on_msg(Msg::Stats(Ok(None)));
        assert!(!app.stats.supported);
        app.stats.last_poll = None;
        assert!(!app.tick().contains(&Cmd::LoadStats));
        assert_eq!(press(&mut app, KeyCode::Char('S')), vec![], "the view explains, nothing is asked");
        assert!(matches!(app.view, View::Stats(_)));
        assert!(app.stats.header_memory().is_none());
    }

    #[test]
    fn command_line() {
        let mut app = with_databases(2);
        for c in ":w out.bin.zst".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.command.as_deref(), Some("w out.bin.zst"));
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Cmd::Export { db: 0, path: "out.bin.zst".into() }]);
        for c in ":nope".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.status_line().unwrap().0.contains("not a command"));
        press(&mut app, KeyCode::Char(':'));
        press(&mut app, KeyCode::Backspace); // empty line: leaves the command line
        assert!(app.command.is_none());
        for c in ":q".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.quit);
    }

    /// A dimension with Total over A and B; `roots_only` gives just Total.
    fn elements(roots_only: bool) -> Vec<ElementDto> {
        use fluxolap_api_types::{ChildDto, ElementType};
        let leaf = |id: u32, name: &str| ElementDto { id, name: name.into(), elem_type: ElementType::Numeric, position: id, level: 0, parents: vec![0], children: vec![] };
        let total = ElementDto {
            id: 0,
            name: "Total".into(),
            elem_type: ElementType::Consolidated,
            position: 0,
            level: 1,
            parents: vec![],
            children: vec![ChildDto { id: 1, weight: 1.0 }, ChildDto { id: 2, weight: 1.0 }],
        };
        if roots_only {
            vec![total]
        } else {
            vec![total, leaf(1, "A"), leaf(2, "B")]
        }
    }

    fn type_keys(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn a_hundred_dimensions_load_lazily_and_are_chosen_by_search() {
        let mut app = with_databases(1);
        let dims: Vec<DimensionDto> = (0..100).map(|i| DimensionDto { id: i, name: format!("d{i}"), dim_type: ObjectType::Normal, element_count: 3, modified: None }).collect();
        let cube = CubeDto { id: 0, name: "Wide".into(), cube_type: ObjectType::Normal, dimensions: (0..100).collect(), cell_count: 0, modified: None };
        app.on_msg(Msg::Contents { db: 0, result: Ok((dims, vec![cube])) });
        press(&mut app, KeyCode::Tab);
        let cmds = press(&mut app, KeyCode::Enter);

        // Every element only for the two axis dimensions, top elements for the other 98.
        let full: Vec<u32> = cmds.iter().filter_map(|c| match c { Cmd::LoadElements { dim, roots_only: false, .. } => Some(*dim), _ => None }).collect();
        assert_eq!(full, [0, 1]);
        assert_eq!(cmds.iter().filter(|c| matches!(c, Cmd::LoadElements { roots_only: true, .. })).count(), 98);
        for c in cmds {
            if let Cmd::LoadElements { db, dim, roots_only } = c {
                app.on_msg(Msg::Elements { db, dim, roots_only, result: Ok(elements(roots_only)) });
            }
        }
        let View::Grid(grid) = &app.view else { panic!("grid open") };
        assert!(grid.ready());
        assert_eq!(grid.fixed[57], 0, "filters start on their top element");

        // f: search for a filter; ] loads its elements, then steps.
        press(&mut app, KeyCode::Char('f'));
        type_keys(&mut app, "d57");
        assert_eq!(app.dim_picker_entries(DimPurpose::Filter, "d57").len(), 1);
        press(&mut app, KeyCode::Enter);
        let cmds = press(&mut app, KeyCode::Char(']'));
        assert_eq!(cmds, vec![Cmd::LoadElements { db: 0, dim: 57, roots_only: false }]);
        app.on_msg(Msg::Elements { db: 0, dim: 57, roots_only: false, result: Ok(elements(false)) });
        let View::Grid(grid) = &app.view else { panic!() };
        assert_eq!((grid.fixed_focus, grid.fixed[57]), (Some(57), 1), "the step waited for the elements");

        // R: search a dimension for rows; it loads, then the grid is usable again.
        press(&mut app, KeyCode::Char('R'));
        type_keys(&mut app, "d80");
        let cmds = press(&mut app, KeyCode::Enter);
        assert_eq!(cmds, vec![Cmd::LoadElements { db: 0, dim: 80, roots_only: false }]);
        let View::Grid(grid) = &app.view else { panic!() };
        assert_eq!(grid.row_dim, 80);
        assert!(!grid.ready());
        app.on_msg(Msg::Elements { db: 0, dim: 80, roots_only: false, result: Ok(elements(false)) });
        let View::Grid(grid) = &app.view else { panic!() };
        assert!(grid.ready());
        assert_eq!(grid.rows().len(), 3, "Total opened to A and B");
    }

    #[test]
    fn step_clamps() {
        assert_eq!(step(0, -1, 5), 0);
        assert_eq!(step(4, 1, 5), 4);
        assert_eq!(step(2, 10, 5), 4);
        assert_eq!(step(0, 1, 0), 0);
    }
}
