"""The layout of a pasted view: which cell gets which label or formula.
Pure data, tested without LibreOffice (tests/test_layout.py); the views
module writes it into the sheet.

    Population                                   <- title (the cube)
    Server     localhost:7777                    <- settings the formulas refer to:
    Database   WorldPopulation                      change a filter cell to see
    Cube       Population                           another slice
    Age        All ages

                          Year   1950    1950    <- one header row per column dimension
    Location   Sex / Sex  Sex    Male    Female
    World      Both sexes        =FLUX.DATA(...) <- one header column per row dimension
    World      Male              ...                (a repeated label is there for the
      Africa   Both sexes        ...                formulas, but hidden: ;;; format)

A pivot, like fluxoscope's: each axis holds one or more dimensions, its
entries are all combinations of their elements (the outer dimension
varies slowest). Formula arguments follow the cube's dimension order:
each dimension is a row header cell, a column header cell or its filter.
"""

from dataclasses import dataclass, field
from itertools import product

DATA = "DATA"  # one FLUX.DATA per cell: writable
AREA = "AREA"  # one FLUX.AREA block: one request, read-only (one dimension per axis)


@dataclass
class Cell:
    row: int  # absolute sheet position
    col: int
    text: str = ""  # a constant (label, element name), or ...
    formula: str = ""  # ... a formula (set via the API, English names)
    bold: bool = False
    indent: int = 0  # hierarchy depth of a row header
    total: bool = False  # a consolidated element's row or column
    kind: str = ""  # title, setting, filter, header, element, value
    marker: str = ""  # "+" a closed total (can be opened), "-" an open one
    hidden: bool = False  # a repeated header label: there for the formulas, not shown


@dataclass
class View:
    cells: list = field(default_factory=list)
    # (first row, first col, last row, last col) of the FLUX.AREA block, absolute.
    area: tuple = None
    rows: int = 0  # height and width of the whole view
    cols: int = 0


def column_name(c):
    """0 -> A, 25 -> Z, 26 -> AA."""
    name = ""
    c += 1
    while c:
        c, r = divmod(c - 1, 26)
        name = chr(65 + r) + name
    return name


def address(row, col, abs_row=True, abs_col=True):
    """An absolute (or partly absolute) A1 reference, 0-based input."""
    return f"{'$' if abs_col else ''}{column_name(col)}{'$' if abs_row else ''}{row + 1}"


def quote(text):
    return '"' + str(text).replace('"', '""') + '"'


def tuples(lists):
    """An axis's entries: every combination of its dimensions' visible
    elements, the first dimension varying slowest (like fluxoscope)."""
    return [tuple(t) for t in product(*lists)] if lists else []


def _key(entry):
    return (entry[0], tuple(entry[3])) if len(entry) > 3 else (entry[0],)


def starts_group(entries, i, level):
    """Whether entry `i` starts a new group at `level` (its label is shown)."""
    return i == 0 or [_key(e) for e in entries[i][: level + 1]] != [_key(e) for e in entries[i - 1][: level + 1]]


def build(server, database, cube, dimensions, row_dims, col_dims, row_entries, col_entries, filters, style=DATA, top=0, left=0):
    """The view at sheet position (top, left), 0-based.

    dimensions: the cube's dimension names, in order.
    row_dims / col_dims: indices of the row and the column dimensions, outer first (col_dims may be empty).
    row_entries / col_entries: the axes' entries, each a tuple with one element per axis dimension:
        (name, depth, consolidated[, path, opened]) -- see tuples().
    filters: {dimension index: element name} for every other dimension.
    """
    rows_n, cols_n = len(row_dims), len(col_dims)
    if not rows_n or not row_entries:
        raise ValueError("no rows")
    if cols_n and not col_entries:
        raise ValueError("no columns")
    if style == AREA and (rows_n > 1 or cols_n > 1):
        raise ValueError("one block (FLUX.AREA) takes one dimension on the rows and at most one on the columns -- choose formulas")
    on_axis = set(row_dims) | set(col_dims)
    missing = [d for d in range(len(dimensions)) if d not in on_axis and d not in filters]
    if missing:
        raise ValueError(f"no filter element for {', '.join(dimensions[d] for d in missing)}")
    view = View()
    put = view.cells.append

    def at(r, c, **kw):
        put(Cell(top + r, left + c, **kw))

    at(0, 0, text=cube, bold=True, kind="title")
    settings = [("Server", server), ("Database", database), ("Cube", cube)]
    filter_dims = [d for d in range(len(dimensions)) if d not in on_axis]
    settings += [(dimensions[d], filters[d]) for d in filter_dims]
    setting_ref = {}
    for i, (label, value) in enumerate(settings):
        at(1 + i, 0, text=label, kind="setting")
        at(1 + i, 1, text=str(value), kind="setting" if i < 3 else "filter")
        setting_ref[label if i < 3 else filter_dims[i - 3]] = address(top + 1 + i, left + 1)

    _, head, first = geometry(len(filter_dims), cols_n)
    cols = col_entries if cols_n else [()]
    # The corner: the row dimensions' names above their columns; the column
    # dimensions' names left of their header rows.
    for k, d in enumerate(row_dims):
        if not (cols_n and k == rows_n - 1):  # that cell names both: "Location / Year"
            at(first - 1, k, text=dimensions[d], bold=True, kind="header")
    for k, d in enumerate(col_dims):
        label = dimensions[d] if k < cols_n - 1 else f"{dimensions[row_dims[-1]]} / {dimensions[d]}"
        at(head + k, rows_n - 1, text=label, bold=True, kind="header")
    if not cols_n:
        at(head, rows_n, text="value", bold=True, kind="header")
    for j, entry in enumerate(col_entries if cols_n else []):
        for k, element in enumerate(entry):
            name, _, consolidated = element[:3]
            at(head + k, rows_n + j, text=name, bold=True, total=consolidated, kind="element", marker=_marker(element),
               hidden=not starts_group(col_entries, j, k))
    for i, entry in enumerate(row_entries):
        for k, element in enumerate(entry):
            name, depth, consolidated = element[:3]
            at(first + i, k, text=name, indent=depth, bold=consolidated, total=consolidated, kind="element", marker=_marker(element),
               hidden=not starts_group(row_entries, i, k))

    def args(row_refs, col_refs):
        out = [setting_ref["Server"], setting_ref["Database"], setting_ref["Cube"]]
        for d in range(len(dimensions)):
            if d in row_dims:
                out.append(row_refs[row_dims.index(d)])
            elif d in col_dims:
                out.append(col_refs[col_dims.index(d)])
            else:
                out.append(setting_ref[d])
        return ";".join(out)

    if style == DATA:
        for i, rentry in enumerate(row_entries):
            row_refs = [address(top + first + i, left + k, abs_row=False) for k in range(rows_n)]
            for j, centry in enumerate(cols):
                col_refs = [address(top + head + k, left + rows_n + j, abs_col=False) for k in range(cols_n)]
                total = any(e[2] for e in rentry) or any(e[2] for e in centry)
                at(first + i, rows_n + j, formula=f"=FLUX.DATA({args(row_refs, col_refs)})", total=total, kind="value")
    else:
        rows_range = f"{address(top + first, left)}:{address(top + first + len(row_entries) - 1, left)}"
        cols_range = f"{address(top + head, left + 1)}:{address(top + head, left + len(cols))}" if cols_n else None
        view.area = (top + first, left + 1, top + first + len(row_entries) - 1, left + len(cols))
        view.cells.append(Cell(top + first, left + 1, formula=f"=FLUX.AREA({args([rows_range], [cols_range])})", kind="value"))
    view.rows = first + len(row_entries)
    view.cols = rows_n + len(cols)
    return view


def _marker(element):
    """An element's open/closed marker: elements may come with (path, opened)."""
    if len(element) < 5 or not element[2]:
        return ""
    return "-" if element[4] else "+"


def geometry(n_filters, n_col_dims=1):
    """Rows of a view relative to its title: (first filter, first column
    header row, first data row) -- one header row per column dimension."""
    head = 4 + n_filters + 1
    return 4, head, head + max(n_col_dims, 1)


# -- navigating an axis -----------------------------------------------------------
#
# An axis is its top entries (`roots`) plus the set of opened elements
# (`expanded`), both as paths: the element names from a top element down,
# so an element under two parents is two different entries.

def _index(tree):
    return {e["id"]: e for e in tree}, {e["name"]: e for e in tree}


def _children(by_id, e):
    return [by_id[c["id"]] for c in e.get("children", []) if c["id"] in by_id]


def axis_start(tree, parent=None, with_parent=True):
    """(roots, expanded) for "the children of `parent`" (with `parent` itself
    first and open, as the total), or -- without a parent -- every element:
    all top elements, everything open."""
    by_id, by_name = _index(tree)
    if parent is None:
        roots, expanded = [], []

        def walk(e, path):
            if e.get("children"):
                expanded.append(path)
                for c in _children(by_id, e):
                    walk(c, path + [c["name"]])

        for root in sorted((e for e in tree if not e["parents"]), key=lambda e: e["position"]):
            roots.append([root["name"]])
            walk(root, [root["name"]])
        return roots, expanded
    p = by_name[parent]
    if with_parent:
        return [[p["name"]]], [[p["name"]]]
    return [[c["name"]] for c in _children(by_id, p)], []


def visible(tree, roots, expanded):
    """The axis's entries in display order: [(name, depth, consolidated, path, opened)]."""
    by_id, by_name = _index(tree)
    opened = {tuple(p) for p in expanded}
    out = []

    def walk(e, path, depth):
        is_open = tuple(path) in opened and bool(e.get("children"))
        out.append((e["name"], depth, bool(e.get("children")), path, is_open))
        if is_open:
            for c in _children(by_id, e):
                walk(c, path + [c["name"]], depth + 1)

    for root in roots:
        e = by_name.get(root[-1])
        if e is not None:  # an element deleted on the server meanwhile is left out
            walk(e, list(root), 0)
    return out


def toggle(expanded, path):
    """Opens `path` if it's closed; closes it -- and everything under it -- if it's open."""
    path = list(path)
    if path in expanded:
        return [p for p in expanded if p[: len(path)] != path]
    return expanded + [path]


def element_list(tree, parent=None, with_parent=True):
    """Display rows for a dimension from its elements (the API's element
    list): the children of `parent` (and `parent` itself first, as the
    total), or -- without a parent -- every element in hierarchy order.
    Returns [(name, depth, consolidated)]."""
    by_id = {e["id"]: e for e in tree}
    by_name = {e["name"]: e for e in tree}
    out = []
    if parent is None:
        seen = set()

        def walk(e, depth):
            if e["id"] in seen:
                return
            seen.add(e["id"])
            out.append((e["name"], depth, bool(e.get("children"))))
            for c in e.get("children", []):
                if c["id"] in by_id:
                    walk(by_id[c["id"]], depth + 1)

        for root in sorted((e for e in tree if not e["parents"]), key=lambda e: e["position"]):
            walk(root, 0)
        return out
    p = by_name[parent]
    if with_parent:
        out.append((p["name"], 0, True))
    for c in p.get("children", []):
        child = by_id[c["id"]]
        out.append((child["name"], 1 if with_parent else 0, bool(child.get("children"))))
    return out


# -- charts ----------------------------------------------------------------------

CHARTS = ("line", "bar", "stacked")
MAX_SERIES = 12  # like fluxoscope


def tuple_name(entries):
    """A row's or column's name for a chart: its elements, outer first ("Africa · Male")."""
    return " · ".join(e[0] for e in entries)


def chart_plan(rows, cols, kind):
    """What a view's chart shows, like fluxoscope's: the rows are the
    series, the columns the x axis. Bars add up, so in bar and stacked
    charts an opened total (its children are shown too) is left out --
    it would count them twice. Returns (row indexes, column indexes, how
    many more rows there were than MAX_SERIES); a view without column
    dimensions has its one value column."""
    def kept(entries):
        return [i for i, t in enumerate(entries) if kind == "line" or not any(e[4] for e in t)]

    series = kept(rows)
    points = kept(cols) if cols else [0]
    return series[:MAX_SERIES], points, max(len(series) - MAX_SERIES, 0)


def runs(indexes):
    """Sorted indexes as runs of neighbours: [1, 2, 3, 5] -> [(1, 3), (5, 5)]."""
    out = []
    for i in indexes:
        if out and out[-1][1] == i - 1:
            out[-1] = (out[-1][0], i)
        else:
            out.append((i, i))
    return out
