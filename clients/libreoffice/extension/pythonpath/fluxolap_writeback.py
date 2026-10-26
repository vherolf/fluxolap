"""Writing back by typing over a FLUX.DATA cell: the value goes to the
server (a total splashes down to its base cells), the formula stays and
shows the new value.

How: Calc reports typed input to an XChangesListener, but by then the cell
already holds the typed value. Remembering every FLUX.DATA cell by
address would go wrong as soon as rows are inserted, so instead, right
after the input, the edit is undone: if the cell held a FLUX.DATA formula,
the typed value is written and the formula stays (the undo brought it
back); otherwise the edit is redone, as if nothing happened. That can't
happen inside the change notification itself (LibreOffice crashes), so it
runs just after it, on LibreOffice's main thread (AsyncCallback).

An emptied cell (Delete) only gets its formula back: deleting must not
clear data on the server. Typing a new formula over a FLUX.DATA cell is an
ordinary edit.
"""

import traceback

import uno
import unohelper
from com.sun.star.awt import XCallback
from com.sun.star.util import XChangesListener

import fluxolap_client as fc
import fluxolap_formula as ff

# Most cells one edit may span for write-back (a huge paste is left alone).
MAX_CELLS = 10_000

_listeners = {}  # document RuntimeUID -> WriteBack (keeps them alive)
# The last problem a write-back ran into, for tests and headless use.
last_problem = None


def attach(ctx, doc):
    """Starts watching `doc` (a spreadsheet document) for typed values."""
    key = doc.RuntimeUID
    if key not in _listeners:
        listener = WriteBack(ctx, doc)
        doc.addChangesListener(listener)
        _listeners[key] = listener


class _Later(unohelper.Base, XCallback):
    def __init__(self, fn):
        self.fn = fn

    def notify(self, _data):
        self.fn()


class WriteBack(unohelper.Base, XChangesListener):
    def __init__(self, ctx, doc):
        self.ctx = ctx
        self.doc = doc
        self.busy = False  # our own undo/redo/writes cause change events too
        self.callback = ctx.ServiceManager.createInstanceWithContext("com.sun.star.awt.AsyncCallback", ctx)

    def changesOccurred(self, event):
        if self.busy:
            return
        ranges = [c.ReplacedElement for c in event.Changes if c.Accessor == "cell-change" and c.ReplacedElement is not None]
        if ranges:
            self.callback.addCallback(_Later(lambda: self._handle(ranges)), None)

    def disposing(self, _event):
        _listeners.pop(getattr(self.doc, "RuntimeUID", None), None)

    # -----------------------------------------------------------------

    def _handle(self, ranges):
        global last_problem
        self.busy = True
        problems = []
        try:
            for cells in ranges:
                problems += self._handle_range(cells)
        except Exception:
            problems.append(traceback.format_exc(limit=3))
        finally:
            self.busy = False
        if problems:
            last_problem = "\n".join(problems)
            self._report(last_problem)

    def _handle_range(self, cells):
        a = cells.getRangeAddress()
        if (a.EndColumn - a.StartColumn + 1) * (a.EndRow - a.StartRow + 1) > MAX_CELLS:
            return []
        undo = self.doc.getUndoManager()
        if not undo.isUndoPossible():
            return []
        typed_formulas = cells.getFormulaArray()
        typed_values = cells.getDataArray()
        undo.undo()
        before = cells.getFormulaArray()
        ours = [
            (r, c)
            for r, row in enumerate(before)
            for c, formula in enumerate(row)
            if ff.is_data(formula) and not typed_formulas[r][c].startswith("=") and typed_formulas[r][c] != formula
        ]
        if not ours:
            undo.redo()  # not a FLUX.DATA cell: the edit stays as typed
            return []
        # The FLUX.DATA cells keep their formula (the undo brought it back);
        # the other cells of the edit get what was typed into them again.
        mine = set(ours)
        for r, row in enumerate(typed_formulas):
            for c, formula in enumerate(row):
                if (r, c) not in mine and formula != before[r][c]:
                    cells.getCellByPosition(c, r).setFormula(formula)
        problems = []
        written = False
        for r, c in ours:
            value = typed_values[r][c]
            if value == "":
                continue  # emptied: the formula is back, nothing is written
            try:
                server, database, cube, path = self._target(cells, before[r][c])
                fc.server(server).write(database, cube, path, value)
                written = True
            except (fc.FluxError, ff.NotWritable) as e:
                problems.append(f"{cells.getCellByPosition(c, r).AbsoluteName.split('.')[-1].replace('$', '')}: {e}")
        if written:
            self.doc.calculateAll()
        return problems

    def _target(self, cells, formula):
        """(server, database, cube, element names) of a FLUX.DATA formula."""
        sheet = cells.getSpreadsheet()
        values = []
        for arg in ff.arguments(formula):
            kind = ff.classify(arg)
            if kind[0] == "ref":
                _, sheet_name, address = kind
                source = self.doc.Sheets.getByName(sheet_name) if sheet_name else sheet
                values.append(_cell_value(source.getCellRangeByName(address)))
            else:
                values.append(kind[1])
        if len(values) < 4:
            raise ff.NotWritable("FLUX.DATA needs a server, a database, a cube and elements")
        server, database, cube, *elements = values
        return str(server), str(database), str(cube), tuple(fc.element_name(v) for v in elements)

    def _report(self, text):
        """A message box -- only when the document is on screen (a hidden or
        headless one would wait forever for the OK)."""
        try:
            window = self.doc.getCurrentController().getFrame().getContainerWindow()
            if not window.isVisible():
                return
            toolkit = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.awt.Toolkit", self.ctx)
            box = toolkit.createMessageBox(window, uno.Enum("com.sun.star.awt.MessageBoxType", "ERRORBOX"), 1,
                                           "fluxolap", f"The value could not be written:\n\n{text}")
            box.execute()
        except Exception:
            pass


def _cell_value(cell):
    """A referenced cell's content -- raw, not as displayed (a pasted
    view's headers show a "▸ " marker that is only number formatting)."""
    kind = cell.getType().value
    if kind == "VALUE":
        return cell.getValue()
    if kind == "FORMULA":
        return cell.getValue() if cell.FormulaResultType2 == 1 else cell.getString()
    if kind == "TEXT":
        text = cell.getFormula()
        return text[1:] if text.startswith("'") else text
    return ""
