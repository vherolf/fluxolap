"""Pasted views that can be navigated: double-click a
total in the row or column headers to open or close it, a filter cell to
pick another element, the title to change the whole view.

A view describes itself in the document, so it survives saving and
reopening and needs no state in memory:
- a named range `fluxolap_view_<n>` covers it (it moves along when rows
  or columns are inserted above or left of it);
- a document property of the same name holds its definition as JSON:
  server, database, cube, each axis as its top entries plus the opened
  elements (paths of element names), the filters, the options, and which
  entries it currently shows.

Rebuilding a view rewrites only its own cells. If it grows into cells
that aren't empty -- another view beside or below it -- whole columns
(rows) are inserted right after it, so what's there moves aside as a
whole and nothing is overwritten; unless that would cut through another
view, then the change is refused. When it shrinks again, the columns
(rows) it gave up are removed, if they are empty across the whole sheet
and something stands beside (below) it: the neighbour moves back, and a
gap left between them on purpose stays.
"""

import json

import uno
import unohelper
from com.sun.star.awt import XCallback, XEnhancedMouseClickHandler

import fluxolap_chart as fch
import fluxolap_client as fc
import fluxolap_layout as fl

PREFIX = "fluxolap_view_"
MAX_ROWS = 5000
MAX_COLS = 300
# CellFlags: values, dates, strings, formulas, hard formatting, styles, edit attributes, formatted text
CLEAR = 1 | 2 | 4 | 16 | 32 | 64 | 256 | 512
MARKERS = {"+": '"▸ "@', "-": '"▾ "@'}

STYLES = {
    "fluxolap Title": {"CharWeight": "BOLD", "CharHeight": 13.0},
    "fluxolap Setting": {"CharColor": 0x707070},
    "fluxolap Filter": {"CellBackColor": 0xE8F0FE, "CharColor": 0x1A4FA0},
    "fluxolap Header": {"CharWeight": "BOLD", "CellBackColor": 0xF2F2F2},
    "fluxolap Element": {},
    "fluxolap Element Total": {"CharWeight": "BOLD"},
    "fluxolap Value": {"NumberFormat": "#,##0.###"},
    "fluxolap Value Total": {"NumberFormat": "#,##0.###", "CharWeight": "BOLD"},
}


# -- reading cells ----------------------------------------------------------------

def raw_text(cell):
    """A cell's value as an element name -- not as displayed (a header's
    "▸ " marker is only number formatting)."""
    kind = cell.getType().value
    if kind == "VALUE":
        return fc.element_name(cell.getValue())
    if kind == "FORMULA":
        return fc.element_name(cell.getValue()) if cell.FormulaResultType2 == 1 else cell.getString()
    if kind == "TEXT":
        text = cell.getFormula()
        return text[1:] if text.startswith("'") else text
    return ""


# -- the views of a document ---------------------------------------------------------

def _properties(doc):
    return doc.getDocumentProperties().getUserDefinedProperties()


def load(doc, name):
    props = _properties(doc)
    if not props.getPropertySetInfo().hasPropertyByName(name):
        return None
    try:
        return json.loads(props.getPropertyValue(name))
    except ValueError:
        return None


def _save(doc, name, spec, absolute_range):
    props = _properties(doc)
    text = json.dumps(spec, separators=(",", ":"))
    if props.getPropertySetInfo().hasPropertyByName(name):
        props.setPropertyValue(name, text)
    else:
        props.addProperty(name, 128, text)  # REMOVABLE
    ranges = doc.NamedRanges
    if ranges.hasByName(name):
        ranges.removeByName(name)
    ranges.addNewByName(name, absolute_range, uno.createUnoStruct("com.sun.star.table.CellAddress"), 0)


def _new_name(doc):
    ranges = doc.NamedRanges
    n = 1
    while ranges.hasByName(f"{PREFIX}{n}") or load(doc, f"{PREFIX}{n}") is not None:
        n += 1
    return f"{PREFIX}{n}"


def find(doc, sheet_index, row, col):
    """The view containing a cell: (name, spec, range address), or None."""
    ranges = doc.NamedRanges
    for name in ranges.getElementNames():
        if not name.startswith(PREFIX):
            continue
        cells = ranges.getByName(name).getReferredCells()
        if cells is None:
            continue
        a = cells.getRangeAddress()
        if a.Sheet == sheet_index and a.StartRow <= row <= a.EndRow and a.StartColumn <= col <= a.EndColumn:
            spec = load(doc, name)
            if spec is not None:
                return name, spec, a
    return None


def normalize(spec):
    """A view's definition in the current form: each axis a list of
    dimensions (views saved before pivots had one per axis)."""
    spec = dict(spec)
    for axis in ("rows", "columns"):
        value = spec.get(axis)
        if value is None:
            spec[axis] = []
        elif isinstance(value, dict):
            spec[axis] = [value]
    shown = spec.get("shown")
    if shown:
        def entries(items):
            # old: [path, consolidated] per entry; now: one such pair per axis dimension
            return [[e] if e and isinstance(e[0], list) and e[0] and isinstance(e[0][0], str) else e for e in items]

        spec["shown"] = {"rows": entries(shown.get("rows", [])), "cols": entries(shown.get("cols", []))}
    return spec


def role(doc, sheet_index, row, col):
    """What a double-click on this cell does: ("edit"), ("filter", dimension)
    or ("toggle", axis, k, path) -- open or close `path` in the axis's k-th
    dimension -- with (name, spec, address) of its view, or None (an
    ordinary cell)."""
    found = find(doc, sheet_index, row, col)
    if found is None:
        return None
    name, spec, a = found
    spec = normalize(spec)
    r, c = row - a.StartRow, col - a.StartColumn
    rows_n, cols_n = len(spec["rows"]), len(spec["columns"])
    first_filter, head, first = fl.geometry(len(spec["filter_order"]), cols_n)
    sheet = doc.Sheets.getByIndex(sheet_index)
    shown = spec["shown"]
    action = None
    if (r, c) == (0, 0):
        action = ("edit",)
    elif c == 1 and first_filter <= r < head - 1:
        action = ("filter", spec["filter_order"][r - first_filter])
    elif head <= r < head + cols_n and c >= rows_n and c - rows_n < len(shown["cols"]):
        k = r - head
        path, consolidated = shown["cols"][c - rows_n][k]
        if consolidated:
            action = ("toggle", "columns", k, path)
    elif c < rows_n and r >= first and r - first < len(shown["rows"]):
        path, consolidated = shown["rows"][r - first][c]
        if consolidated:
            action = ("toggle", "rows", c, path)
    if action and action[0] == "toggle" and raw_text(sheet.getCellByPosition(col, row)) != action[3][-1]:
        return None  # the sheet was changed under the view: don't guess
    return (action, (name, spec, a)) if action else None


# -- building a view ----------------------------------------------------------------

def axis_spec(server, database, choice, old=None):
    """An axis's definition from the dialog's choice {dimension, parent, total}:
    its top entries and opened elements. `parent` "(as it is)" keeps `old`."""
    if old is not None and choice.get("parent") == KEEP and old["dimension"] == choice["dimension"]:
        return dict(old)
    tree = server.element_tree(database, choice["dimension"])
    parent = choice.get("parent")
    roots, expanded = fl.axis_start(tree, None if parent in (None, "", ALL, KEEP) else parent, choice.get("total", True))
    return {"dimension": choice["dimension"], "roots": roots, "expanded": expanded}


ALL = "(all elements)"
KEEP = "(as it is)"


def _ensure_styles(doc):
    family = doc.StyleFamilies.getByName("CellStyles")
    bold = uno.getConstantByName("com.sun.star.awt.FontWeight.BOLD")
    for name, props in STYLES.items():
        if family.hasByName(name):
            continue
        style = doc.createInstance("com.sun.star.style.CellStyle")
        family.insertByName(name, style)
        for key, value in props.items():
            if key == "CharWeight":
                value = bold
            elif key == "NumberFormat":
                value = _number_format(doc, value)
            style.setPropertyValue(key, value)


def _number_format(doc, code):
    formats = doc.getNumberFormats()
    locale = uno.createUnoStruct("com.sun.star.lang.Locale")
    key = formats.queryKey(code, locale, False)
    return key if key != -1 else formats.addNew(code, locale)


def render(doc, sheet, top, left, spec, name=None):
    """Writes the view `spec` with its title at (top, left) -- in place of
    the view `name`, if given -- and saves it. Returns its name."""
    spec = normalize(spec)
    server = fc.server(spec["server"])
    db = spec["database"]
    cubes = dict(server.cubes(db))
    if spec["cube"] not in cubes:
        raise fc.FluxError(f"cube '{spec['cube']}' not found")
    dims = cubes[spec["cube"]]
    axes = spec["rows"] + spec["columns"]
    if not spec["rows"]:
        raise fc.FluxError("choose at least one dimension for the rows")
    for axis in axes:
        if axis["dimension"] not in dims:
            raise fc.FluxError(f"'{axis['dimension']}' isn't a dimension of {spec['cube']}")
    used = [axis["dimension"] for axis in axes]
    if len(set(used)) != len(used):
        raise fc.FluxError("each dimension can be on the rows or the columns only once")
    row_dims = [dims.index(axis["dimension"]) for axis in spec["rows"]]
    col_dims = [dims.index(axis["dimension"]) for axis in spec["columns"]]
    trees = {axis["dimension"]: server.element_tree(db, axis["dimension"]) for axis in axes}

    def lists(axis_specs):
        return [fl.visible(trees[a["dimension"]], a["roots"], a["expanded"]) for a in axis_specs]

    row_lists, col_lists = lists(spec["rows"]), lists(spec["columns"])
    count = 1
    for elements in row_lists + col_lists:
        count *= max(len(elements), 1)
    if count > 5_000_000:
        raise fc.FluxError(f"{count:,} combinations are too many -- open fewer elements or move a dimension to the filters")
    rows, cols = fl.tuples(row_lists), fl.tuples(col_lists)
    filter_dims = [d for d in dims if d not in used]
    missing = [d for d in filter_dims if not spec["filters"].get(d)]
    if missing:
        raise fc.FluxError(f"choose an element for {', '.join(missing)}")
    filters = {dims.index(d): spec["filters"][d] for d in filter_dims}
    if spec.get("zero") and rows:
        rows, cols = _without_empty(server, spec, dims, row_dims, col_dims, rows, cols, filters, trees)
    if not rows or (col_dims and not cols):
        raise fc.FluxError("the view would be empty (every row or column is empty)")
    if len(rows) > MAX_ROWS or len(cols) > MAX_COLS:
        raise fc.FluxError(f"{len(rows):,} rows x {len(cols):,} columns is too big for a sheet view (at most {MAX_ROWS:,} x {MAX_COLS})")
    style = spec.get("style", fl.DATA)
    if style == fl.DATA:
        # Every cell in one request now: the new formulas' first calculation
        # then comes from the cache instead of cell by cell.
        def path(r, c):
            names = [None] * len(dims)
            for k, d in enumerate(row_dims):
                names[d] = r[k][0]
            for k, d in enumerate(col_dims):
                names[d] = c[k][0]
            for d, e in filters.items():
                names[d] = e
            return tuple(names)

        server.values(db, spec["cube"], [path(r, c) for r in rows for c in (cols or [()])])
    try:
        view = fl.build(spec["server"], db, spec["cube"], dims, row_dims, col_dims, rows, cols, filters, style=style, top=top, left=left)
    except ValueError as e:
        raise fc.FluxError(str(e)) from None
    area = (left, top, left + view.cols - 1, top + view.rows - 1)
    old = None
    if name and doc.NamedRanges.hasByName(name):
        cells = doc.NamedRanges.getByName(name).getReferredCells()
        old = cells.getRangeAddress() if cells is not None else None
    chart = spec.get("chart")
    if chart and chart.get("type") not in fl.CHARTS:
        raise fc.FluxError(f"a chart is one of {', '.join(fl.CHARTS)}, not '{chart.get('type')}'")
    # Asked before the view changes: a growing view moves its chart along.
    chart_moved = bool(chart and name) and fch.moved_by_user(sheet, name, chart, top, left)
    conflicts = _make_room(doc, sheet, area, old, name)
    # By position, after any insertion: a range object grows with inserted columns or rows.
    target = sheet.getCellRangeByPosition(*area)
    if conflicts:
        where = ", ".join(_address_text(sheet, c) for c in conflicts)
        raise fc.FluxError(f"the view needs {target.AbsoluteName.replace('$', '')}, but {where} is in the way -- move it, or paste the view elsewhere")

    spec = dict(spec)
    spec["filter_order"] = filter_dims
    spec["shown"] = {"rows": [[[e[3], e[2]] for e in r] for r in rows], "cols": [[[e[3], e[2]] for e in c] for c in cols]}
    doc.lockControllers()
    doc.enableAutomaticCalculation(False)
    try:
        if old is not None:
            sheet.getCellRangeByPosition(old.StartColumn, old.StartRow, old.EndColumn, old.EndRow).clearContents(CLEAR)
        _ensure_styles(doc)
        _write(doc, sheet, view)
        if old is not None:
            _close_gap(sheet, area, old)
    finally:
        doc.enableAutomaticCalculation(True)
        doc.unlockControllers()
    doc.calculateAll()
    name = name or _new_name(doc)
    if chart:
        _, _, first = fl.geometry(len(filter_dims), len(col_dims))
        spec["chart"] = fch.update(doc, sheet, name, chart, rows, cols, top, left, first, spec["cube"], chart_moved)
    else:
        spec.pop("chart", None)
        fch.remove(doc, sheet, name)
    _save(doc, name, spec, target.AbsoluteName)
    return name


def _empty(sheet, c1, r1, c2, r2):
    """No values, text, formulas or comments in the block."""
    return len(sheet.getCellRangeByPosition(c1, r1, c2, r2).queryContentCells(1 | 2 | 4 | 8 | 16).getRangeAddresses()) == 0


def _close_gap(sheet, area, old):
    """A view that shrank: the columns (rows) it gave up are removed, so
    what stands beside (below) it moves back -- only if they are empty
    across the whole sheet (nothing elsewhere is lost, no other view is
    cut) and there is something beside (below) the view at all."""
    left, top, right, bottom = area
    last_col = sheet.getColumns().getCount() - 1
    last_row = sheet.getRows().getCount() - 1
    if right < old.EndColumn:
        first, n = right + 1, old.EndColumn - right
        band = (top, max(bottom, old.EndRow))
        if _empty(sheet, first, 0, first + n - 1, last_row) and old.EndColumn < last_col and not _empty(sheet, old.EndColumn + 1, band[0], last_col, band[1]):
            sheet.getColumns().removeByIndex(first, n)
    if bottom < old.EndRow:
        first, n = bottom + 1, old.EndRow - bottom
        band = (left, max(right, old.EndColumn))
        if _empty(sheet, 0, first, last_col, first + n - 1) and old.EndRow < last_row and not _empty(sheet, band[0], old.EndRow + 1, band[1], last_row):
            sheet.getRows().removeByIndex(first, n)


def _rect(sheet_index, c1, r1, c2, r2):
    a = uno.createUnoStruct("com.sun.star.table.CellRangeAddress")
    a.Sheet, a.StartColumn, a.StartRow, a.EndColumn, a.EndRow = sheet_index, c1, r1, c2, r2
    return a


def _address_text(sheet, a):
    return sheet.getCellRangeByPosition(a.StartColumn, a.StartRow, a.EndColumn, a.EndRow).AbsoluteName.replace("$", "")


def _conflicts(target, old):
    """The filled cells in `target` that aren't the view's own (outside
    `old`, its area before): the parts right of it and below it. A new
    view (no `old`) has none of its own."""
    out = []
    for a in target.queryContentCells(1 | 2 | 4 | 16).getRangeAddresses():  # values, dates, strings, formulas
        if old is None:
            out.append(a)
            continue
        if a.EndColumn > old.EndColumn:
            out.append(_rect(a.Sheet, max(a.StartColumn, old.EndColumn + 1), a.StartRow, a.EndColumn, a.EndRow))
        if a.EndRow > old.EndRow and a.StartColumn <= old.EndColumn:
            out.append(_rect(a.Sheet, a.StartColumn, max(a.StartRow, old.EndRow + 1), min(a.EndColumn, old.EndColumn), a.EndRow))
    return out


def _view_ranges(doc, sheet_index, other_than):
    """The areas of the sheet's other views."""
    ranges = doc.NamedRanges
    for name in ranges.getElementNames():
        if name.startswith(PREFIX) and name != other_than:
            cells = ranges.getByName(name).getReferredCells()
            if cells is not None and cells.getRangeAddress().Sheet == sheet_index:
                yield cells.getRangeAddress()


def _make_room(doc, sheet, area, old, name):
    """When a view grows into filled cells beside (below) it, inserts as
    many whole columns (rows) right after it: what's there moves aside as
    a whole, a view with its named range. Refused if that would cut
    through another view. `area`: (left, top, right, bottom) the view
    needs. Returns what is still in the way."""
    conflicts = _conflicts(sheet.getCellRangeByPosition(*area), old)
    if not conflicts or old is None:
        return conflicts
    _, _, right_col, bottom_row = area
    grow_cols, grow_rows = right_col - old.EndColumn, bottom_row - old.EndRow
    right = any(c.StartColumn > old.EndColumn for c in conflicts)
    below = any(c.StartRow > old.EndRow and c.StartColumn <= old.EndColumn for c in conflicts)
    others = list(_view_ranges(doc, old.Sheet, name))
    if right and grow_cols > 0:
        cut = [v for v in others if v.StartColumn <= old.EndColumn < v.EndColumn]
        if cut:
            raise fc.FluxError(f"to make room, columns would be inserted after {_address_text(sheet, old)}, "
                               f"but that would cut through the view at {_address_text(sheet, cut[0])} -- move one of them")
        sheet.getColumns().insertByIndex(old.EndColumn + 1, grow_cols)
    if below and grow_rows > 0:
        cut = [v for v in others if v.StartRow <= old.EndRow < v.EndRow]
        if cut:
            raise fc.FluxError(f"to make room, rows would be inserted below {_address_text(sheet, old)}, "
                               f"but that would cut through the view at {_address_text(sheet, cut[0])} -- move one of them")
        sheet.getRows().insertByIndex(old.EndRow + 1, grow_rows)
    return _conflicts(sheet.getCellRangeByPosition(*area), old)


def _without_empty(server, spec, dims, row_dims, col_dims, rows, cols, filters, trees):
    """Zero suppression: rows and columns (combinations) without any value are left out."""
    lists = [[filters[d]] if d in filters else [] for d in range(len(dims))]
    for k, d in enumerate(row_dims):
        lists[d] = sorted({r[k][0] for r in rows})
    for k, d in enumerate(col_dims):
        lists[d] = sorted({c[k][0] for c in cols})
    size = 1
    for d in row_dims + col_dims:
        size *= len(lists[d])
    if size > 1_000_000:
        raise fc.FluxError(f"{size:,} cells are too many to look for empty rows -- open fewer elements")
    filled = server.filled(spec["database"], spec["cube"], lists)
    names = {d: {e["id"]: e["name"] for e in trees[dims[d]]} for d in row_dims + col_dims}
    rows_with = {tuple(names[d].get(p[d]) for d in row_dims) for p in filled}
    cols_with = {tuple(names[d].get(p[d]) for d in col_dims) for p in filled}
    rows = [r for r in rows if tuple(e[0] for e in r) in rows_with]
    if col_dims:
        cols = [c for c in cols if tuple(e[0] for e in c) in cols_with]
    return rows, cols


def _write(doc, sheet, view):
    styles = {"title": "fluxolap Title", "setting": "fluxolap Setting", "filter": "fluxolap Filter", "header": "fluxolap Header"}
    markers = {k: _number_format(doc, v) for k, v in MARKERS.items()}
    hidden = _number_format(doc, ";;;")
    for c in view.cells:
        cell = sheet.getCellByPosition(c.col, c.row)
        if c.formula and view.area and (c.row, c.col) == view.area[:2]:
            continue  # the block's array formula comes below
        if c.formula:
            cell.setFormula(c.formula)
            cell.CellStyle = "fluxolap Value Total" if c.total else "fluxolap Value"
            continue
        cell.setString(c.text)
        if c.kind == "element":
            cell.CellStyle = "fluxolap Element Total" if c.total else "fluxolap Element"
        else:
            cell.CellStyle = styles.get(c.kind, "fluxolap Element")
        if c.hidden:
            cell.NumberFormat = hidden  # a repeated label: there for the formulas
        elif c.marker:
            cell.NumberFormat = markers[c.marker]
        if c.indent:
            cell.ParaIndent = 350 * c.indent  # 1/100 mm
    if view.area:
        r1, c1, r2, c2 = view.area
        block = sheet.getCellRangeByPosition(c1, r1, c2, r2)
        block.setArrayFormula(next(c.formula for c in view.cells if c.formula))
        block.CellStyle = "fluxolap Value"
    cols = [c.col for c in view.cells]
    rows = [c.row for c in view.cells]
    sheet.getCellRangeByPosition(min(cols), min(rows), max(cols), max(rows)).getColumns().OptimalWidth = True


def current_filters(doc, spec, address):
    """The filter elements as they are in the sheet now (the user may have
    typed another element into a filter cell)."""
    sheet = doc.Sheets.getByIndex(address.Sheet)
    first_filter, _, _ = fl.geometry(len(spec["filter_order"]))
    filters = dict(spec["filters"])
    for i, d in enumerate(spec["filter_order"]):
        text = raw_text(sheet.getCellByPosition(address.StartColumn + 1, address.StartRow + first_filter + i))
        if text:
            filters[d] = text
    return filters


# -- double-click --------------------------------------------------------------------

def act(ctx, doc, sheet_index, row, col, interactive=True):
    """Does what a double-click on the cell means. Returns the action's
    kind ("toggle", "filter", "edit") or None. Not interactive (tests,
    macros): "filter" and "edit" only report what they would open."""
    found = role(doc, sheet_index, row, col)
    if found is None:
        return None
    action, (name, spec, a) = found
    sheet = doc.Sheets.getByIndex(sheet_index)
    spec = normalize(spec)
    spec["filters"] = current_filters(doc, spec, a)
    if action[0] == "toggle":
        _, axis, k, path = action
        spec[axis] = [dict(x) for x in spec[axis]]
        spec[axis][k]["expanded"] = fl.toggle(spec[axis][k]["expanded"], path)
        render(doc, sheet, a.StartRow, a.StartColumn, spec, name)
    elif interactive:
        import fluxolap_commands as fcmd  # the dialogs

        if action[0] == "filter":
            element = fcmd.pick_element(ctx, doc, spec["server"], spec["database"], action[1], spec["filters"][action[1]])
            if element is not None:
                set_filter(doc, name, spec, a, action[1], element)
        else:
            fcmd.edit_view(ctx, doc, name, spec, a)
    return action[0]


def set_filter(doc, name, spec, address, dimension, element):
    """Puts another element into a filter cell; the formulas follow."""
    sheet = doc.Sheets.getByIndex(address.Sheet)
    spec = dict(spec)
    spec["filters"] = dict(spec["filters"], **{dimension: element})
    if spec.get("zero"):
        render(doc, sheet, address.StartRow, address.StartColumn, spec, name)  # other rows may be empty now
        return
    first_filter, _, _ = fl.geometry(len(spec["filter_order"]))
    i = spec["filter_order"].index(dimension)
    sheet.getCellByPosition(address.StartColumn + 1, address.StartRow + first_filter + i).setString(element)
    doc.calculateAll()
    _save(doc, name, spec, doc.NamedRanges.getByName(name).getContent())


class _Later(unohelper.Base, XCallback):
    def __init__(self, fn):
        self.fn = fn

    def notify(self, _data):
        self.fn()


class ClickHandler(unohelper.Base, XEnhancedMouseClickHandler):
    """Double-clicks on a view's totals, filters and title.

    Calc's own click interface: its event names the clicked cell (`Target`),
    and Calc only hands clicks in the grid to this kind of handler -- the
    general XMouseClickHandler never sees them. Mind the return value:
    False means "handled" (Calc then doesn't start editing the cell),
    True lets Calc go on as usual."""

    def __init__(self, ctx, doc, controller):
        self.ctx, self.doc, self.controller = ctx, doc, controller
        self.later = ctx.ServiceManager.createInstanceWithContext("com.sun.star.awt.AsyncCallback", ctx)

    def mousePressed(self, event):
        return not self.handle(event)

    def handle(self, event):
        """True if the double-click was on a view's total, filter or title
        (its action then runs right after the click)."""
        if event.ClickCount != 2 or event.Buttons != 1:
            return False
        try:
            a = event.Target.getCellAddress()
        except AttributeError:  # not a cell (a shape, a chart ...)
            return False
        try:
            if role(self.doc, a.Sheet, a.Row, a.Column) is None:
                return False
        except Exception:
            return False
        self.later.addCallback(_Later(lambda: self._act(a)), None)
        return True

    def _act(self, a):
        try:
            act(self.ctx, self.doc, a.Sheet, a.Row, a.Column)
        except fc.FluxError as e:
            import fluxolap_commands as fcmd

            fcmd.message(self.ctx, self.doc, f"The view could not be changed:\n\n{e}")
        except Exception:
            import traceback

            import fluxolap_commands as fcmd

            fcmd.message(self.ctx, self.doc, traceback.format_exc(limit=4))

    def mouseReleased(self, _event):
        return True  # go on as usual

    def disposing(self, _event):
        pass


_handlers = []  # (controller, handler): keeps them alive, one per window


def attach(ctx, doc):
    """Double-click navigation for the document's current view (window)."""
    controller = doc.getCurrentController()
    if controller is None or any(c == controller for c, _ in _handlers):  # == compares the UNO objects
        return
    handler = ClickHandler(ctx, doc, controller)
    controller.addEnhancedMouseClickHandler(handler)
    _handlers.append((controller, handler))


def click(doc, cell):
    """A left double-click on `cell`, handed to this window's handler as
    Calc hands it over (for the tests). Returns what mousePressed returned."""
    controller = doc.getCurrentController()
    handler = next((h for c, h in _handlers if c == controller), None)
    if handler is None:
        return None
    event = uno.createUnoStruct("com.sun.star.awt.EnhancedMouseEvent")
    event.Buttons, event.ClickCount, event.Target = 1, 2, cell
    return handler.mousePressed(event)
