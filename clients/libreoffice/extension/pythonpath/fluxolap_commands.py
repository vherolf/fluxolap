"""The fluxolap menu in Calc (Addons.xcu): Connect, Paste view, Load
database, Refresh -- and the dialogs a pasted view opens on double-click
(fluxolap_views): an element picker for a filter, Paste view again to
change the view. The dialogs are built here in Python on LibreOffice's
dialog toolkit; the work behind them is in fluxolap_client (the API),
fluxolap_views (pasted views) and fluxolap_layout (where things go).

`run(ctx, command)` is called by the Commands service (fluxolap_addin.py)
with the menu item's command. For tests and macros there are also
`paste:<json>` (paste a view without the dialog), `change:<json>` (what
the dialog's Change does to the view at `"at"`) and `navigate:<cell>`
(what a double-click on that cell does, e.g. `navigate:$Sheet1.$B$11`).
The commands only the tests use are at the end ("for the tests").
"""

import json
import os
import time
import traceback

import uno
import unohelper
from com.sun.star.awt import XActionListener, XItemListener, XTextListener

import fluxolap_client as fc
import fluxolap_layout as fl
import fluxolap_views as fv

SERVER_PROPERTY = "fluxolap server"
DEFAULT_SERVER = "localhost:7777"
ALL = fv.ALL
KEEP = fv.KEEP
NONE = "(none)"


def run(ctx, command):
    doc = _document(ctx)
    if doc is None or not doc.supportsService("com.sun.star.sheet.SpreadsheetDocument"):
        return
    try:
        if command == "connect":
            ConnectDialog(ctx, doc).run()
        elif command == "paste":
            PasteDialog(ctx, doc).run()
        elif command == "load":
            LoadDialog(ctx, doc).run()
        elif command == "refresh":
            refresh(doc)
        elif command.startswith("paste:"):
            _report(doc, lambda: paste(doc, json.loads(command[len("paste:"):])))
        elif command.startswith("change:"):
            choice = json.loads(command[len("change:"):])
            cell = doc.getCurrentController().getActiveSheet().getCellRangeByName(choice.pop("at")).getCellAddress()
            _report(doc, lambda: change(doc, fv.find(doc, cell.Sheet, cell.Row, cell.Column), choice))
        elif command.startswith("navigate:"):
            cell = doc.getCurrentController().getActiveSheet().getCellRangeByName(command[len("navigate:"):])
            a = cell.getCellAddress()
            _report(doc, lambda: fv.act(ctx, doc, a.Sheet, a.Row, a.Column, interactive=False))
        elif command in TEST_COMMANDS or command.startswith("click:"):
            _test_command(ctx, doc, command)
    except Exception:
        message(ctx, doc, traceback.format_exc(limit=4))


def _report(doc, fn):
    """Runs a test/macro command; its outcome goes into the document
    property "fluxolap result" ("ok", what it did, or the error)."""
    _remember(doc, "fluxolap result", "")
    try:
        result = fn()
        _remember(doc, "fluxolap result", result if isinstance(result, str) else "ok")
    except Exception as e:
        _remember(doc, "fluxolap result", f"{type(e).__name__}: {e}")


def _document(ctx):
    desktop = ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", ctx)
    return desktop.getCurrentComponent()


# -- the document remembers its server ----------------------------------------

def _remember(doc, name, value):
    props = doc.getDocumentProperties().getUserDefinedProperties()
    if props.getPropertySetInfo().hasPropertyByName(name):
        props.setPropertyValue(name, value)
    else:
        props.addProperty(name, 128, value)  # REMOVABLE


def document_server(doc):
    props = doc.getDocumentProperties().getUserDefinedProperties()
    if props.getPropertySetInfo().hasPropertyByName(SERVER_PROPERTY):
        return props.getPropertyValue(SERVER_PROPERTY) or DEFAULT_SERVER
    return DEFAULT_SERVER


# -- messages -------------------------------------------------------------------

def message(ctx, doc, text, kind="ERRORBOX", buttons=1):
    """A message box; returns the button pressed (1 OK, 2 YES, 3 NO ...)."""
    window = doc.getCurrentController().getFrame().getContainerWindow()
    toolkit = ctx.ServiceManager.createInstanceWithContext("com.sun.star.awt.Toolkit", ctx)
    box = toolkit.createMessageBox(window, uno.Enum("com.sun.star.awt.MessageBoxType", kind), buttons, "fluxolap", text)
    return box.execute()


# -- commands without a dialog -----------------------------------------------------

def refresh(doc):
    """Reads every FLUX value again, right away: unlike Ctrl+Shift+F9, also
    the values cached less than `FRESH_FOR` ago."""
    for s in list(fc._servers.values()):
        s.forget()
    doc.calculateAll()


def _axis_list(value):
    """An axis from a choice: a list of dimensions (one dict, or none, is accepted too)."""
    if value is None:
        return []
    return [value] if isinstance(value, dict) else list(value)


def view_spec(choice, old=None):
    """A view's definition from the Paste view dialog's choice:
    {server, database, cube, rows: [{dimension, parent, total}, ...],
     columns: [...], filters: {dimension: element}, style, zero}.
    `old`: the view being changed -- a dimension with parent "(as it is)"
    keeps what's open in it. `chart`: "line", "bar", "stacked" or None."""
    server = fc.server(choice["server"])
    old = fv.normalize(old) if old else None
    old_axes = {a["dimension"]: a for a in (old["rows"] + old["columns"])} if old else {}

    def axes(value):
        return [fv.axis_spec(server, choice["database"], a, old_axes.get(a["dimension"])) for a in _axis_list(value)]

    return {
        "server": choice["server"],
        "database": choice["database"],
        "cube": choice["cube"],
        "rows": axes(choice["rows"]),
        "columns": axes(choice.get("columns")),
        "filters": dict(choice.get("filters", {})),
        "style": choice.get("style", fl.DATA),
        "zero": bool(choice.get("zero", False)),
        **({"chart": _chart(choice["chart"], old)} if choice.get("chart") else {}),
    }


def _chart(kind, old):
    """A view's chart; a chart the view already had keeps its place."""
    chart = {"type": kind}
    if old and old.get("chart") and "offset" in old["chart"]:
        chart["offset"] = old["chart"]["offset"]
    return chart


def paste(doc, choice):
    """Pastes a new view at the current cell."""
    sheet, top, left = _cursor(doc)
    name = fv.render(doc, sheet, top, left, view_spec(choice))
    _remember(doc, SERVER_PROPERTY, choice["server"])
    return name


def change(doc, view, choice):
    """Changes a pasted view -- `view`: (name, definition, address) -- to the dialog's choice."""
    if view is None:
        raise fc.FluxError("there is no pasted view there")
    name, old, a = view
    return fv.render(doc, doc.Sheets.getByIndex(a.Sheet), a.StartRow, a.StartColumn, view_spec(choice, old), name)


def _cursor(doc):
    sel = doc.getCurrentController().getSelection()
    try:
        a = sel.getRangeAddress()
    except AttributeError:  # several ranges selected: use the active sheet's A1
        return doc.getCurrentController().getActiveSheet(), 0, 0
    return doc.Sheets.getByIndex(a.Sheet), a.StartRow, a.StartColumn


def pick_element(ctx, doc, server, database, dimension, current):
    """Lets the user pick an element of `dimension`; returns its name or None."""
    picker = ElementPicker(ctx, doc, server, database, dimension, current)
    picker.run()
    return picker.chosen


def edit_view(ctx, doc, name, spec, address):
    """Paste view, filled in with the view `name`, changes it in place."""
    PasteDialog(ctx, doc, view=(name, spec, address)).run()


# -- dialogs ------------------------------------------------------------------------

class _Listener(unohelper.Base, XActionListener, XItemListener, XTextListener):
    """Calls `fn` for a button's click, a list's selection change or typing in a field."""

    def __init__(self, fn):
        self.fn = fn

    def actionPerformed(self, _event):
        self.fn()

    def itemStateChanged(self, _event):
        self.fn()

    def textChanged(self, _event):
        self.fn()

    def disposing(self, _event):
        pass


class Dialog:
    """A dialog built in code; positions and sizes in dialog units."""

    def __init__(self, ctx, doc, title, width, height):
        self.ctx, self.doc = ctx, doc
        smgr = ctx.ServiceManager
        self.toolkit = smgr.createInstanceWithContext("com.sun.star.awt.Toolkit", ctx)
        self.model = smgr.createInstanceWithContext("com.sun.star.awt.UnoControlDialogModel", ctx)
        self.model.Width, self.model.Height, self.model.Title = width, height, title
        self.dialog = smgr.createInstanceWithContext("com.sun.star.awt.UnoControlDialog", ctx)
        self.dialog.setModel(self.model)
        # Above 0 while the dialog changes its own controls: filling or
        # selecting a list fires the same events as a user's click, and a
        # handler reacting to its own changes would loop until LibreOffice
        # crashes. Only a user's action runs a handler.
        self.updating = 0
        self.handled = 0  # user actions handled so far (the tests wait for one)
        self.build()

    def add(self, kind, name, x, y, w, h, **props):
        m = self.model.createInstance(f"com.sun.star.awt.UnoControl{kind}Model")
        m.PositionX, m.PositionY, m.Width, m.Height = x, y, w, h
        for k, v in props.items():
            setattr(m, k, v)
        self.model.insertByName(name, m)
        return m

    def label(self, name, x, y, w, text, **props):
        return self.add("FixedText", name, x, y + 2, w, 10, Label=text, **props)

    def ctl(self, name):
        return self.dialog.getControl(name)

    def on(self, name, fn):
        def guarded():
            if self.updating:
                return
            self.updating += 1
            try:
                fn()
            finally:
                self.updating -= 1
                self.handled += 1

        c = self.ctl(name)
        listener = _Listener(guarded)
        # A button is asked first: it has addItemListener too (for toggle
        # buttons), which a plain click never calls.
        if c.getModel().supportsService("com.sun.star.awt.UnoControlButtonModel"):  # clicked
            c.addActionListener(listener)
        elif hasattr(c, "addItemListener"):  # a list or a check box: its selection changed
            c.addItemListener(listener)
        elif hasattr(c, "addTextListener"):  # a text field: typed into
            c.addTextListener(listener)
        else:
            c.addActionListener(listener)

    def items(self, name, values, select=0):
        c = self.ctl(name)
        c.removeItems(0, c.getItemCount())
        c.addItems(tuple(values), 0)
        if values and select is not None:
            c.selectItemPos(min(select, len(values) - 1), True)

    def selected(self, name):
        c = self.ctl(name)
        return c.getSelectedItem() if c.getSelectedItemPos() >= 0 else None

    def status(self, text):
        self.ctl("status").setText(text)

    def run(self):
        window = self.doc.getCurrentController().getFrame().getContainerWindow()
        self.dialog.createPeer(self.toolkit, window)
        self.begin()
        result = self.dialog.execute()
        if result == 1:
            self.ok()
        self.dialog.dispose()

    def begin(self):
        """Registers the handlers and fills the controls (as one quiet change)."""
        self.updating += 1
        try:
            self.start()
        finally:
            self.updating -= 1

    def start(self):
        pass

    def ok(self):
        pass


class ConnectDialog(Dialog):
    """The document's fluxolap server."""

    def __init__(self, ctx, doc):
        super().__init__(ctx, doc, "fluxolap: connect", 220, 72)

    def build(self):
        self.label("l_server", 6, 8, 40, "Server")
        self.add("Edit", "server", 48, 8, 120, 13, Text=document_server(self.doc))
        self.add("Button", "test", 172, 8, 42, 13, Label="Test")
        self.label("status", 6, 28, 208, "", MultiLine=True)
        self.add("Button", "ok", 112, 52, 50, 14, Label="OK", PushButtonType=1, DefaultButton=True)
        self.add("Button", "cancel", 164, 52, 50, 14, Label="Cancel", PushButtonType=2)

    def start(self):
        self.on("test", self.test)

    def test(self):
        try:
            self.status(f"Connected: {fc.server(self.ctl('server').getText()).info()}")
        except fc.FluxError as e:
            self.status(f"Not reachable: {e}")

    def ok(self):
        _remember(self.doc, SERVER_PROPERTY, self.ctl("server").getText().strip())


CHART_CHOICES = [(None, "No chart"), ("line", "Line"), ("bar", "Bars"), ("stacked", "Stacked bars")]


class PasteDialog(Dialog):
    """Choose a cube, the dimensions on the rows and the columns (several
    each: a pivot) and the filters; writes the view at the cursor -- or,
    with `view` (name, definition, address), changes that view."""

    AXES = (("rows", "Rows"), ("cols", "Columns"))

    def __init__(self, ctx, doc, view=None):
        self.cubes = []  # [(name, [dimension names])]
        self.trees = {}  # dimension name -> elements
        self.filters = {}  # dimension name -> element name
        self.axes = {"rows": [], "cols": []}  # per axis: [{dimension, parent, total}], outer first
        self.view = view
        self.original = fv.normalize(view[1]) if view else None
        self.prefill = dict(self.original) if view else None  # used up while the lists are first filled
        super().__init__(ctx, doc, "fluxolap: change view" if view else "fluxolap: paste view", 300, 336)

    def build(self):
        y = 8
        self.label("l_server", 6, y, 44, "Server")
        self.add("Edit", "server", 52, y, 190, 13, Text=self.original["server"] if self.original else document_server(self.doc))
        self.add("Button", "connect", 246, y, 48, 13, Label="Connect")
        y += 18
        self.label("l_db", 6, y, 44, "Database")
        self.add("ListBox", "database", 52, y, 242, 13, Dropdown=True)
        y += 17
        self.label("l_cube", 6, y, 44, "Cube")
        self.add("ListBox", "cube", 52, y, 242, 13, Dropdown=True)
        y += 22
        for axis, title in self.AXES:
            # The axis's dimensions (outer first), and what to add, remove or move up.
            self.label(f"l_{axis}", 6, y, 44, title, FontWeight=150)
            self.add("ListBox", f"{axis}_list", 52, y, 120, 36)
            self.add("ListBox", f"{axis}_add_dim", 176, y, 70, 13, Dropdown=True)
            self.add("Button", f"{axis}_add", 250, y, 44, 13, Label="Add")
            self.add("Button", f"{axis}_remove", 176, y + 16, 56, 13, Label="Remove")
            self.add("Button", f"{axis}_up", 236, y + 16, 58, 13, Label="Move up")
            y += 40
            # What the selected dimension shows.
            self.label(f"l_{axis}_of", 52, y, 22, "show")
            self.add("ListBox", f"{axis}_parent", 76, y, 124, 13, Dropdown=True)
            self.add("CheckBox", f"{axis}_total", 206, y + 1, 88, 10, Label="with the total", State=1)
            y += 20
        self.label("l_filters", 6, y, 44, "Filters", FontWeight=150)
        self.add("ListBox", "filters", 52, y, 242, 50)
        y += 53
        self.label("l_filter_el", 52, y, 50, "Element")
        self.add("ListBox", "filter_element", 104, y, 190, 13, Dropdown=True)
        y += 18
        self.add("RadioButton", "style_data", 52, y, 242, 10, Label="Formulas (FLUX.DATA): you can type values to write them back", State=1)
        y += 12
        self.add("RadioButton", "style_area", 52, y, 242, 10, Label="One block (FLUX.AREA): one request, read-only, one dimension per axis")
        y += 14
        self.add("CheckBox", "zero", 52, y, 242, 10, Label="Hide empty rows and columns")
        y += 14
        self.label("l_chart", 6, y, 44, "Chart")
        self.add("ListBox", "chart", 52, y, 120, 13, Dropdown=True)
        self.label("l_chart_note", 176, y, 118, "rows are the series", TextColor=0x707070)
        y += 18
        self.label("status", 6, y + 4, 180, "")
        self.add("Button", "ok", 190, y + 4, 50, 14, Label="Change" if self.view else "Paste", PushButtonType=1, DefaultButton=True)
        self.add("Button", "cancel", 244, y + 4, 50, 14, Label="Cancel", PushButtonType=2)

    def start(self):
        self.items("chart", [label for _, label in CHART_CHOICES], 0)
        self.on("connect", self.connect)
        self.on("database", self.load_cubes)
        self.on("cube", self.load_dimensions)
        for axis, _ in self.AXES:
            self.on(f"{axis}_list", lambda a=axis: self.show_axis_entry(a))
            self.on(f"{axis}_add", lambda a=axis: self.add_dimension(a))
            self.on(f"{axis}_remove", lambda a=axis: self.remove_dimension(a))
            self.on(f"{axis}_up", lambda a=axis: self.move_up(a))
            self.on(f"{axis}_parent", lambda a=axis: self.set_entry(a))
            self.on(f"{axis}_total", lambda a=axis: self.set_entry(a))
        self.on("filters", self.show_filter)
        self.on("filter_element", self.set_filter)
        self.connect()

    def server(self):
        return fc.server(self.ctl("server").getText())

    def connect(self):
        try:
            names = self.server().databases()
            self.items("database", names, _pos(names, self.prefill and self.prefill["database"]))
            self.status(f"{self.server().info()}: {len(names)} databases")
            self.load_cubes()
        except fc.FluxError as e:
            self.items("database", [])
            self.status(f"Not reachable: {e}")
        if self.prefill:
            self.ctl("style_area").setState(self.prefill.get("style") == fl.AREA)
            self.ctl("style_data").setState(self.prefill.get("style") != fl.AREA)
            self.ctl("zero").setState(1 if self.prefill.get("zero") else 0)
            chart = (self.prefill.get("chart") or {}).get("type")
            self.ctl("chart").selectItemPos(_pos([k for k, _ in CHART_CHOICES], chart), True)
            self.prefill = None

    def load_cubes(self):
        db = self.selected("database")
        if db is None:
            return
        try:
            self.cubes = self.server().cubes(db)
        except fc.FluxError as e:
            self.status(str(e))
            return
        self.trees.clear()
        names = [name for name, _ in self.cubes]
        self.items("cube", names, _pos(names, self.prefill and self.prefill["cube"]))
        self.load_dimensions()

    def dims(self):
        cube = self.selected("cube")
        return next((d for name, d in self.cubes if name == cube), [])

    def tree(self, dim):
        if dim not in self.trees:
            self.trees[dim] = self.server().element_tree(self.selected("database"), dim)
        return self.trees[dim]

    def _default_parent(self, dim):
        """A new axis dimension shows its first total's children, with the total."""
        consolidated = [name for name, _, c in fl.element_list(self.tree(dim)) if c]
        return consolidated[0] if consolidated else ALL

    def load_dimensions(self):
        dims = self.dims()
        p = self.prefill
        if p:
            self.axes = {"rows": [{"dimension": a["dimension"], "parent": KEEP, "total": True} for a in p["rows"]],
                         "cols": [{"dimension": a["dimension"], "parent": KEEP, "total": True} for a in p["columns"]]}
            self.filters = dict(p["filters"])
        else:
            # Like fluxoscope: the first dimension on the rows, the second on the columns.
            self.axes = {"rows": [{"dimension": d, "parent": self._default_parent(d), "total": True} for d in dims[:1]],
                         "cols": [{"dimension": d, "parent": self._default_parent(d), "total": True} for d in dims[1:2]]}
            self.filters = {}
        for axis, _ in self.AXES:
            self.show_axis(axis)
        self.show_filters()

    # -- an axis's dimensions ---------------------------------------------------------

    def used(self):
        return {e["dimension"] for entries in self.axes.values() for e in entries}

    def show_axis(self, axis, select=0):
        entries = self.axes[axis]
        self.items(f"{axis}_list", [e["dimension"] for e in entries], select if entries else None)
        free = [d for d in self.dims() if d not in self.used()]
        self.items(f"{axis}_add_dim", free, 0)
        self.show_axis_entry(axis)

    def selected_entry(self, axis):
        pos = self.ctl(f"{axis}_list").getSelectedItemPos()
        entries = self.axes[axis]
        return entries[pos] if 0 <= pos < len(entries) else None

    def show_axis_entry(self, axis):
        entry = self.selected_entry(axis)
        if entry is None:
            self.items(f"{axis}_parent", [])
            return
        consolidated = [name for name, _, c in fl.element_list(self.tree(entry["dimension"])) if c]
        choices = ([KEEP] if self._was_on_axis(entry["dimension"]) else []) + [ALL] + consolidated
        self.items(f"{axis}_parent", choices, _pos(choices, entry["parent"], 0))
        self.ctl(f"{axis}_total").setState(1 if entry["total"] else 0)

    def _was_on_axis(self, dim):
        return bool(self.original) and any(a["dimension"] == dim for a in self.original["rows"] + self.original["columns"])

    def set_entry(self, axis):
        entry = self.selected_entry(axis)
        if entry is not None:
            entry["parent"] = self.selected(f"{axis}_parent") or ALL
            entry["total"] = self.ctl(f"{axis}_total").getState() == 1

    def add_dimension(self, axis):
        dim = self.selected(f"{axis}_add_dim")
        if dim is None:
            return
        self.axes[axis].append({"dimension": dim, "parent": KEEP if self._was_on_axis(dim) else self._default_parent(dim), "total": True})
        self.filters.pop(dim, None)
        for a, _ in self.AXES:
            self.show_axis(a, len(self.axes[a]) - 1 if a == axis else 0)
        self.show_filters()

    def remove_dimension(self, axis):
        pos = self.ctl(f"{axis}_list").getSelectedItemPos()
        if 0 <= pos < len(self.axes[axis]):
            del self.axes[axis][pos]
            for a, _ in self.AXES:
                self.show_axis(a, max(pos - 1, 0) if a == axis else 0)
            self.show_filters()

    def move_up(self, axis):
        pos = self.ctl(f"{axis}_list").getSelectedItemPos()
        entries = self.axes[axis]
        if 0 < pos < len(entries):
            entries[pos - 1], entries[pos] = entries[pos], entries[pos - 1]
            self.show_axis(axis, pos - 1)

    # -- filters ----------------------------------------------------------------------

    def show_filters(self):
        used = self.used()
        lines = []
        for d in self.dims():
            if d in used:
                continue
            if d not in self.filters:
                roots = [e for e in self.tree(d) if not e["parents"]]
                roots.sort(key=lambda e: e["position"])
                self.filters[d] = roots[0]["name"] if roots else ""
            lines.append(f"{d}: {self.filters[d]}")
        keep = self.ctl("filters").getSelectedItemPos()
        self.items("filters", lines, keep if 0 <= keep < len(lines) else 0)
        self.show_filter()

    def filter_dim(self):
        line = self.selected("filters")
        return line.split(": ", 1)[0] if line else None

    def show_filter(self):
        d = self.filter_dim()
        if d is None:
            self.items("filter_element", [])
            return
        names = [name for name, _, _ in fl.element_list(self.tree(d))]
        self.items("filter_element", names, names.index(self.filters[d]) if self.filters[d] in names else 0)

    def set_filter(self):
        d, e = self.filter_dim(), self.selected("filter_element")
        if d and e:
            self.filters[d] = e
            pos = self.ctl("filters").getSelectedItemPos()
            c = self.ctl("filters")
            c.removeItems(pos, 1)
            c.addItem(f"{d}: {e}", pos)
            c.selectItemPos(pos, True)

    def spec(self):
        used = self.used()
        return {
            "server": self.ctl("server").getText().strip(),
            "database": self.selected("database"),
            "cube": self.selected("cube"),
            "rows": [dict(e) for e in self.axes["rows"]],
            "columns": [dict(e) for e in self.axes["cols"]],
            "filters": {d: e for d, e in self.filters.items() if d not in used},
            "style": fl.AREA if self.ctl("style_area").getState() else fl.DATA,
            "zero": self.ctl("zero").getState() == 1,
            "chart": CHART_CHOICES[max(self.ctl("chart").getSelectedItemPos(), 0)][0],
        }

    def ok(self):
        spec = self.spec()
        if not spec["database"] or not spec["cube"] or not spec["rows"]:
            message(self.ctx, self.doc, "Choose a database, a cube and at least one dimension for the rows.")
            return
        try:
            if self.view:
                change(self.doc, self.view, spec)
            else:
                paste(self.doc, spec)
        except (fc.FluxError, ValueError) as e:
            message(self.ctx, self.doc, f"The view could not be {'changed' if self.view else 'pasted'}:\n\n{e}")


def _pos(values, value, default=0):
    """The position of `value` in `values` (for selecting it), or `default`."""
    return values.index(value) if value in values else default


class ElementPicker(Dialog):
    """Pick one element of a dimension: its hierarchy, or what matches a search."""

    def __init__(self, ctx, doc, server, database, dimension, current):
        self.server_address, self.database, self.dimension, self.current = server, database, dimension, current
        self.entries = []  # [(name, depth)] of the whole hierarchy
        self.shown = []  # names in the list, in order
        self.chosen = None
        super().__init__(ctx, doc, f"fluxolap: {dimension}", 200, 222)

    def build(self):
        self.label("l_search", 6, 6, 40, "Search")
        self.add("Edit", "search", 48, 6, 146, 13)
        self.add("ListBox", "elements", 6, 24, 188, 166)
        self.label("status", 6, 196, 80, "")
        self.add("Button", "ok", 90, 202, 50, 14, Label="OK", PushButtonType=1, DefaultButton=True)
        self.add("Button", "cancel", 144, 202, 50, 14, Label="Cancel", PushButtonType=2)

    def start(self):
        try:
            tree = fc.server(self.server_address).element_tree(self.database, self.dimension)
        except fc.FluxError as e:
            self.status(str(e))
            return
        self.entries = [(name, depth) for name, depth, _ in fl.element_list(tree)]
        self.on("search", self.search)
        self.search()

    def search(self):
        text = self.ctl("search").getText().strip().lower()
        if text:
            self.shown = sorted({name for name, _ in self.entries if text in name.lower()}, key=str.lower)
            lines = self.shown
        else:
            self.shown = [name for name, _ in self.entries]
            lines = ["    " * depth + name for name, depth in self.entries]
        self.items("elements", lines, _pos(self.shown, self.current))
        self.status(f"{len(self.shown):,} elements")

    def ok(self):
        pos = self.ctl("elements").getSelectedItemPos()
        if 0 <= pos < len(self.shown):
            self.chosen = self.shown[pos]


class LoadDialog(Dialog):
    """Loads a database into the server: one of its examples, or a file."""

    def __init__(self, ctx, doc):
        self.examples = []
        super().__init__(ctx, doc, "fluxolap: load database", 240, 176)

    def build(self):
        self.label("l_server", 6, 8, 40, "Server")
        self.add("Edit", "server", 48, 8, 186, 13, Text=document_server(self.doc), ReadOnly=True)
        self.label("l_examples", 6, 28, 228, "Examples on the server")
        self.add("ListBox", "examples", 6, 40, 228, 90)
        self.add("Button", "load", 6, 134, 60, 14, Label="Load")
        self.add("Button", "load_all", 70, 134, 60, 14, Label="Load all")
        self.add("Button", "file", 134, 134, 100, 14, Label="From a file…")
        self.label("status", 6, 156, 174, "")
        self.add("Button", "close", 184, 156, 50, 14, Label="Close", PushButtonType=2)

    def server(self):
        return fc.server(self.ctl("server").getText())

    def start(self):
        self.on("load", lambda: self.load([self.ctl("examples").getSelectedItemPos()]))
        self.on("load_all", lambda: self.load([i for i, e in enumerate(self.examples) if not e["loaded"]]))
        self.on("file", self.load_file)
        self.refresh()

    def refresh(self):
        try:
            self.examples = self.server().examples()
        except fc.FluxError as e:
            self.status(f"Not reachable: {e}")
            return
        lines = [f"{e['name']}  ({e['size'] // 1024:,} KB){'  -- loaded' if e['loaded'] else ''}" for e in self.examples]
        self.items("examples", lines, 0)
        if not lines:
            self.status("This server offers no examples.")

    def load(self, positions):
        done = []
        for i in positions:
            if 0 <= i < len(self.examples) and not self.examples[i]["loaded"]:
                try:
                    done.append(self.server().load_example(self.examples[i]["file"]))
                except fc.FluxError as e:
                    self.status(str(e))
                    return
        self.refresh()
        self.status(f"Loaded {', '.join(done)}" if done else "Nothing to load: already on the server.")

    def load_file(self):
        picker = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.ui.dialogs.FilePicker", self.ctx)
        picker.appendFilter("fluxolap database (*.bin.zst)", "*.bin.zst")
        picker.appendFilter("All files", "*")
        if picker.execute() != 1:
            return
        path = uno.fileUrlToSystemPath(picker.getSelectedFiles()[0])
        try:
            with open(path, "rb") as f:
                name = self.server().load_file(f.read())
            self.refresh()
            self.status(f"Loaded {name} from {os.path.basename(path)}")
        except (OSError, fc.FluxError) as e:
            self.status(str(e))


# -- for the tests ---------------------------------------------------------------------
# Commands the headless tests (clients/libreoffice/tests/test_calc.py) drive
# the extension with; nothing in the menu or the sidebar calls them.

TEST_COMMANDS = ("click-state", "check-dialogs")  # and click:<cell>


def _test_command(ctx, doc, command):
    if command.startswith("click:"):
        # A double-click on that cell, through the attached handler.
        cell = doc.getCurrentController().getActiveSheet().getCellRangeByName(command[len("click:"):])
        result = fv.click(doc, cell)
        _remember(doc, "fluxolap result", "no handler" if result is None else ("passed on" if result else "handled"))
    elif command == "click-state":
        # Is double-click navigation attached to this window?
        controller = doc.getCurrentController()
        _remember(doc, "fluxolap result", "attached" if any(c == controller for c, _ in fv._handlers) else "not attached")
    elif command == "check-dialogs":
        _remember(doc, "fluxolap result", _check_dialogs(ctx, doc))


def _check_dialogs(ctx, doc):
    """Every dialog is built and started (connecting, filling its lists)
    without being shown, then a user picking the second entry of each
    list is simulated; then Paste view's buttons are pressed."""
    server = document_server(doc)
    dialogs = [ConnectDialog(ctx, doc), PasteDialog(ctx, doc), LoadDialog(ctx, doc),
               ElementPicker(ctx, doc, server, *_first_dimension(server))]
    for d in dialogs:
        d.dialog.createPeer(d.toolkit, None)
        d.begin()
        for name in d.model.getElementNames():
            c = d.ctl(name)
            if hasattr(c, "selectItemPos") and c.getItemCount() > 1:
                c.selectItemPos(1, True)
        d.dialog.dispose()
    return _check_paste_buttons(ctx, doc)


def _check_paste_buttons(ctx, doc):
    """Paste view's Add, Move up and Remove, pressed like a
    user does (the button's accessible "click" action). "ok", or what went wrong."""
    d = PasteDialog(ctx, doc)
    d.dialog.createPeer(d.toolkit, None)
    d.begin()

    def press(name):
        # The click is handled on LibreOffice's main thread, this runs on another: wait for it.
        handled = d.handled
        d.ctl(name).getAccessibleContext().doAccessibleAction(0)
        for _ in range(100):
            if d.handled > handled:
                return
            time.sleep(0.02)
        raise RuntimeError(f"pressing {name} wasn't handled")

    try:
        dims = d.dims()
        if len(dims) < 3:
            return f"the first cube has only {len(dims)} dimensions"
        before = [e["dimension"] for e in d.axes["rows"]]
        press("rows_add")
        rows = [e["dimension"] for e in d.axes["rows"]]
        if len(rows) != len(before) + 1:
            return f"Add did nothing: rows {before} -> {rows}"
        if rows[-1] in d.filters:
            return f"{rows[-1]} is still a filter"
        press("rows_up")
        if [e["dimension"] for e in d.axes["rows"]] != [rows[-1]] + rows[:-1]:
            return f"Move up did nothing: {[e['dimension'] for e in d.axes['rows']]}"
        press("cols_add")
        if len(d.axes["cols"]) < 2 and len(dims) > 3:
            return f"Add on the columns did nothing: {[e['dimension'] for e in d.axes['cols']]}"
        press("rows_remove")
        if len(d.axes["rows"]) != len(rows) - 1:
            return "Remove did nothing"
        return "ok"
    except Exception:
        return traceback.format_exc(limit=4)
    finally:
        d.dialog.dispose()


def _first_dimension(server):
    db = fc.server(server).databases()[0]
    cube, dims = fc.server(server).cubes(db)[0]
    return db, dims[0], None
