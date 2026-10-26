//! A multidimensional pivot over a cube: any dimensions on rows and on
//! columns (nested, in order), every other dimension a filter on one
//! element. Pure data -- no browser APIs -- so it's tested on the host.

use super::hierarchy::Hierarchy;
use fluxolap_api_types::Value;
use std::collections::{HashMap, HashSet};

/// Most cells one view shows: beyond this the browser table gets slow, so
/// the view asks to collapse or move a dimension to the filters instead.
pub const MAX_VIEW_CELLS: usize = 40_000;

/// Most cells one view reads while empty rows and columns are hidden: the
/// server then sends only the cells with a value, so a sparse view can
/// read far more than it shows. (Reading costs server time: about 0.3 s
/// for 60,000 cells, 3 s for 200,000 of a 9-dimensional cube.)
pub const MAX_READ_CELLS: usize = 200_000;

/// One header entry on an axis: `(element, depth)` per axis dimension.
pub type Tuple = Vec<(u32, usize)>;

#[derive(Debug, Clone, PartialEq)]
pub struct Pivot {
    /// Dimension indices (into the cube's dimensions) on rows, outermost first.
    pub rows: Vec<usize>,
    /// Dimension indices on columns, outermost first.
    pub cols: Vec<usize>,
    /// The selected element of every dimension (used by the filters).
    pub fixed: Vec<u32>,
    /// Where each dimension started: its first top-level element.
    pub defaults: Vec<u32>,
    /// Opened consolidated elements, per dimension.
    pub expanded: Vec<HashSet<u32>>,
    /// Hide rows and columns without any value (zero suppression).
    pub hide_empty: bool,
}

impl Pivot {
    /// The defaults for a cube whose dimensions' top elements are known:
    /// the first two dimensions on rows and columns, every other dimension
    /// a filter on its first top-level element, a single consolidated top
    /// element opened on each axis (so a grid starts on World's regions,
    /// not on World alone), empty rows and columns hidden.
    pub fn new(hier: &[Hierarchy]) -> Self {
        let n = hier.len();
        let defaults: Vec<u32> = hier.iter().map(|h| h.roots().first().copied().unwrap_or(0)).collect();
        let mut pivot = Self {
            rows: vec![0],
            cols: if n > 1 { vec![1] } else { vec![] },
            fixed: defaults.clone(),
            defaults,
            expanded: vec![HashSet::new(); n],
            hide_empty: true,
        };
        for &d in pivot.rows.clone().iter().chain(pivot.cols.clone().iter()) {
            pivot.open_single_root(d, &hier[d]);
        }
        pivot
    }

    /// Opens `d`'s only top-level element if it's consolidated.
    pub fn open_single_root(&mut self, d: usize, h: &Hierarchy) {
        if let [root] = h.roots().as_slice() {
            if h.is_consolidated(*root) {
                self.expanded[d].insert(*root);
            }
        }
    }

    /// The dimensions on neither axis.
    pub fn filters(&self) -> Vec<usize> {
        (0..self.fixed.len()).filter(|d| !self.rows.contains(d) && !self.cols.contains(d)).collect()
    }

    pub fn on_axis(&self, d: usize) -> bool {
        self.rows.contains(&d) || self.cols.contains(&d)
    }

    /// Moves `d` to the end of rows (or columns), taking it off wherever
    /// it was. The last dimension can't leave an axis empty of rows.
    pub fn move_to_axis(&mut self, d: usize, to_rows: bool) {
        self.rows.retain(|&x| x != d);
        self.cols.retain(|&x| x != d);
        if to_rows {
            self.rows.push(d);
        } else {
            self.cols.push(d);
        }
        if self.rows.is_empty() {
            if let Some(c) = self.cols.pop() {
                self.rows.push(c);
            }
        }
    }

    /// Takes `d` off its axis, making it a filter again (on its current element).
    pub fn move_to_filters(&mut self, d: usize) {
        if self.rows == [d] {
            return; // rows can't be empty
        }
        self.rows.retain(|&x| x != d);
        self.cols.retain(|&x| x != d);
    }

    pub fn swap_axes(&mut self) {
        if !self.cols.is_empty() {
            std::mem::swap(&mut self.rows, &mut self.cols);
        }
    }

    pub fn toggle(&mut self, d: usize, element: u32, h: &Hierarchy) {
        if !self.expanded[d].remove(&element) && h.is_consolidated(element) {
            self.expanded[d].insert(element);
        }
    }

    /// The header tuples of an axis: every combination of each axis
    /// dimension's visible members, outermost dimension varying slowest.
    /// An axis without dimensions has one empty tuple (a single column).
    pub fn tuples(&self, dims: &[usize], hier: &[Hierarchy]) -> Vec<Tuple> {
        let mut out: Vec<Tuple> = vec![Vec::new()];
        for &d in dims {
            let members = hier[d].visible(&self.expanded[d]);
            out = out.iter().flat_map(|t| members.iter().map(move |&m| t.iter().copied().chain([m]).collect())).collect();
        }
        out
    }

    /// Number of cells the view would show before suppression.
    pub fn size(&self, hier: &[Hierarchy]) -> usize {
        let count = |dims: &[usize]| dims.iter().map(|&d| hier[d].visible(&self.expanded[d]).len()).product::<usize>();
        count(&self.rows).saturating_mul(count(&self.cols))
    }

    /// The query area: one element list per cube dimension -- the visible
    /// members for axis dimensions, the selected element for filters.
    pub fn area(&self, hier: &[Hierarchy]) -> Vec<Vec<u32>> {
        (0..self.fixed.len())
            .map(|d| if self.on_axis(d) { hier[d].visible(&self.expanded[d]).into_iter().map(|(e, _)| e).collect() } else { vec![self.fixed[d]] })
            .collect()
    }

    /// The cell path for a row and a column tuple.
    pub fn path(&self, row: &Tuple, col: &Tuple) -> Vec<u32> {
        let mut path = self.fixed.clone();
        for (&d, &(e, _)) in self.rows.iter().zip(row) {
            path[d] = e;
        }
        for (&d, &(e, _)) in self.cols.iter().zip(col) {
            path[d] = e;
        }
        path
    }
}

/// A pivot's tuples with the empty ones taken out (when `hide_empty`):
/// a row stays if any of its cells has a value, a column likewise.
///
/// `complete`: `cells` is the whole answer for this view, so a cell that
/// isn't in it is empty (the server leaves empty cells out). Otherwise a
/// missing cell is still being read and counts as filled -- unless the view
/// is too big to check cell by cell, then nothing is taken out yet.
pub fn visible_tuples(pivot: &Pivot, hier: &[Hierarchy], cells: &HashMap<Vec<u32>, Option<Value>>, complete: bool) -> (Vec<Tuple>, Vec<Tuple>) {
    let rows = pivot.tuples(&pivot.rows, hier);
    let cols = pivot.tuples(&pivot.cols, hier);
    if !pivot.hide_empty {
        return (rows, cols);
    }
    if complete {
        // From the cells with a value, not from every combination: a
        // sparse view can be far bigger than what it has values for.
        let key = |dims: &[usize], path: &[u32]| dims.iter().map(|&d| path[d]).collect::<Vec<u32>>();
        let (mut filled_rows, mut filled_cols) = (HashSet::new(), HashSet::new());
        for (path, value) in cells {
            if value.is_some() {
                filled_rows.insert(key(&pivot.rows, path));
                filled_cols.insert(key(&pivot.cols, path));
            }
        }
        let elements = |t: &Tuple| t.iter().map(|&(e, _)| e).collect::<Vec<u32>>();
        return (
            rows.into_iter().filter(|r| filled_rows.contains(&elements(r))).collect(),
            cols.into_iter().filter(|c| filled_cols.contains(&elements(c))).collect(),
        );
    }
    if rows.len().saturating_mul(cols.len()) > MAX_VIEW_CELLS {
        return (rows, cols);
    }
    let filled = |r: &Tuple, c: &Tuple| matches!(cells.get(&pivot.path(r, c)), Some(Some(_)) | None);
    let keep_rows: Vec<Tuple> = rows.iter().filter(|r| cols.iter().any(|c| filled(r, c))).cloned().collect();
    let keep_cols: Vec<Tuple> = cols.iter().filter(|c| keep_rows.iter().any(|r| filled(r, c))).cloned().collect();
    (keep_rows, keep_cols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxolap_api_types::{ChildDto, ElementDto, ElementType};

    fn dim(names: &[&str], total: Option<&str>) -> Hierarchy {
        let mut elements = Vec::new();
        let n = names.len() as u32;
        for (i, name) in names.iter().enumerate() {
            let parents = if total.is_some() { vec![n] } else { vec![] };
            elements.push(ElementDto { id: i as u32, name: name.to_string(), elem_type: ElementType::Numeric, position: i as u32 + 1, level: 0, parents, children: vec![] });
        }
        if let Some(t) = total {
            elements.push(ElementDto {
                id: n,
                name: t.to_string(),
                elem_type: ElementType::Consolidated,
                position: 0,
                level: 1,
                parents: vec![],
                children: (0..n).map(|id| ChildDto { id, weight: 1.0 }).collect(),
            });
        }
        Hierarchy::new(elements)
    }

    /// Location (World over 2 regions) x Year (3, flat) x Sex (Both over 2) x Age (All over 2).
    fn cube() -> Vec<Hierarchy> {
        vec![dim(&["Africa", "Asia"], Some("World")), dim(&["1950", "2000", "2050"], None), dim(&["Male", "Female"], Some("Both")), dim(&["Young", "Old"], Some("All"))]
    }

    #[test]
    fn defaults_follow_the_examples() {
        let h = cube();
        let p = Pivot::new(&h);
        assert_eq!((p.rows.clone(), p.cols.clone(), p.filters()), (vec![0], vec![1], vec![2, 3]));
        assert_eq!(p.fixed, vec![2, 0, 2, 2], "filters on their top element");
        let rows = p.tuples(&p.rows, &h);
        let names: Vec<&str> = rows.iter().map(|t| h[0].name(t[0].0)).collect();
        assert_eq!(names, ["World", "Africa", "Asia"], "World opened to its regions");
        assert_eq!(p.area(&h), vec![vec![2, 0, 1], vec![0, 1, 2], vec![2], vec![2]]);
    }

    #[test]
    fn nested_axes_combine_their_members() {
        let h = cube();
        let mut p = Pivot::new(&h);
        p.move_to_axis(2, true); // rows: Location x Sex
        p.expanded[2].insert(2); // Both opened
        let rows = p.tuples(&p.rows, &h);
        assert_eq!(rows.len(), 3 * 3);
        assert_eq!(rows[1], vec![(2, 0), (0, 1)], "World, Male");
        assert_eq!(p.size(&h), 9 * 3);
        let path = p.path(&rows[4], &vec![(1, 0)]);
        assert_eq!(path, vec![0, 1, 0, 2], "Africa, 2000, Male, All");
        p.move_to_filters(2);
        assert_eq!(p.rows, vec![0]);
        p.move_to_filters(0);
        assert_eq!(p.rows, vec![0], "rows never become empty");
    }

    #[test]
    fn empty_rows_and_columns_are_hidden() {
        let h = cube();
        let p = Pivot::new(&h);
        let (rows, cols) = (p.tuples(&p.rows, &h), p.tuples(&p.cols, &h));
        let mut cells = HashMap::new();
        for r in &rows {
            for c in &cols {
                let v = (h[0].name(r[0].0) != "Asia" && h[1].name(c[0].0) != "2050").then_some(Value::Number(1.0));
                cells.insert(p.path(r, c), v);
            }
        }
        let (rows, cols) = visible_tuples(&p, &h, &cells, false);
        assert_eq!(rows.iter().map(|t| h[0].name(t[0].0)).collect::<Vec<_>>(), ["World", "Africa"]);
        assert_eq!(cols.iter().map(|t| h[1].name(t[0].0)).collect::<Vec<_>>(), ["1950", "2000"]);

        // The same from a sparse answer (only the cells with a value).
        cells.retain(|_, v| v.is_some());
        let (sparse_rows, sparse_cols) = visible_tuples(&p, &h, &cells, true);
        assert_eq!((sparse_rows, sparse_cols), (rows, cols));
        // While a sparse answer is still coming, nothing is taken out.
        assert_eq!(visible_tuples(&p, &h, &HashMap::new(), false).0.len(), 3);
        assert!(visible_tuples(&p, &h, &HashMap::new(), true).0.is_empty(), "a complete empty answer: no rows");
    }
}
