"""The Calc add-in: FLUX.DATA, FLUX.AREA, FLUX.ELEMENTS and FLUX.SERVER
(names and help texts in CalcAddIns.xcu, the interface in
idl/fluxolap.idl). The work is in pythonpath/fluxolap_client.py.

Also the job that starts write-back (typing over a FLUX.DATA cell, see
pythonpath/fluxolap_writeback.py) in every spreadsheet that is created or
opened (Jobs.xcu), and the commands behind the fluxolap menu (Addons.xcu,
pythonpath/fluxolap_commands.py).

A problem shows in the cell as text, "#FLUX: <what went wrong>", rather
than as an anonymous Calc error.
"""

import time

import unohelper
from com.sun.star.awt import XCallback
from com.sun.star.task import XJob, XJobExecutor
from com.sun.star.ui import XUIElementFactory
from org.fluxolap.calc import XFluxolap

import fluxolap_client as fc

# The first calculation of a sheet full of FLUX.DATA formulas: Calc asks
# for one cell at a time, which would be one request per cell. After a few
# misses in quick succession (a whole sheet is being calculated), further
# misses show LOADING and are collected; right after the calculation they
# are read in one batch and the sheet calculates again, from the cache.
BURST = 3  # misses answered right away (single edits stay instant) ...
BURST_WINDOW = 0.25  # ... within this many seconds
LOADING = "…"

IMPLEMENTATION = "org.fluxolap.calc.FluxolapAddIn"
SERVICES = ("com.sun.star.sheet.AddIn", IMPLEMENTATION)


def problem(e):
    return f"#FLUX: {e}"


def flatten(args):
    """Calc's arguments as a flat list: a range arrives as rows of values."""
    out = []
    for a in args:
        if isinstance(a, tuple):
            out.extend(flatten(a))
        elif a is not None and a != "":
            out.append(a)
    return out


def element_lists(args):
    """Per argument (one per cube dimension): its element names."""
    return [[fc.element_name(v) for v in flatten([a])] for a in args]


def cell_value(v):
    """A cube value for Calc: numbers and strings as they are, empty as 0
    (so sums over a report work), a per-cell problem as text."""
    if v is None:
        return 0.0
    if isinstance(v, Exception):
        return problem(v)
    return v


class _Later(unohelper.Base, XCallback):
    def __init__(self, fn):
        self.fn = fn

    def notify(self, _data):
        self.fn()


class FluxolapAddIn(unohelper.Base, XFluxolap):
    def __init__(self, ctx):
        self.ctx = ctx
        self.burst = (0.0, 0)  # (since, misses)
        self.pending = {}  # (server, database, cube) -> {path: None}
        self.scheduled = False
        self.rounds = 0  # second passes in a row (a guard against looping)

    def _miss(self, server, database, cube, path):
        """A cell that isn't cached: True if it should wait for the batch."""
        now = time.monotonic()
        since, misses = self.burst
        if now - since > BURST_WINDOW:
            since, misses = now, 0
        self.burst = (since, misses + 1)
        if misses < BURST or self.rounds > 3:
            return False
        self.pending.setdefault((server, database, cube), {})[path] = None
        if not self.scheduled:
            self.scheduled = True
            later = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.awt.AsyncCallback", self.ctx)
            later.addCallback(_Later(self._flush), None)
        return True

    def _flush(self):
        """Right after the calculation: the collected cells in one batch per
        cube, then the active document calculates again."""
        pending, self.pending, self.scheduled = self.pending, {}, False
        for (server, database, cube), paths in pending.items():
            try:
                fc.server(server).prefetch(database, cube, list(paths))
            except Exception:
                pass  # the second pass shows the problem in the cells
        desktop = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", self.ctx)
        doc = desktop.getCurrentComponent()
        if doc is not None and doc.supportsService("com.sun.star.sheet.SpreadsheetDocument"):
            self.rounds += 1
            try:
                doc.calculateAll()
            finally:
                self.rounds -= 1

    # XServiceName / XServiceInfo, needed by Calc to list the add-in.
    def getServiceName(self):
        return IMPLEMENTATION

    def getImplementationName(self):
        return IMPLEMENTATION

    def supportsService(self, name):
        return name in SERVICES

    def getSupportedServiceNames(self):
        return SERVICES

    # The functions.
    def data(self, server, database, cube, elements):
        try:
            path = tuple(fc.element_name(v) for v in flatten(elements))
            s = fc.server(server)
            cached, value = s.peek(database, cube, path)
            if cached:
                return cell_value(value)
            if self._miss(server, database, cube, path):
                return LOADING
            return cell_value(s.values(database, cube, [path])[0])
        except fc.FluxError as e:
            return problem(e)
        except Exception as e:  # never let Calc see a Python exception
            return problem(f"{type(e).__name__}: {e}")

    def area(self, server, database, cube, elements):
        try:
            lists = element_lists(elements)
            if any(not names for names in lists):
                raise fc.FluxError("every dimension needs at least one element")
            _, _, grid = fc.server(server).area(database, cube, lists)
            return tuple(tuple(cell_value(v) for v in line) for line in grid)
        except fc.FluxError as e:
            return ((problem(e),),)
        except Exception as e:
            return ((problem(f"{type(e).__name__}: {e}"),),)

    def elements(self, server, database, dimension, parent):
        try:
            parent = None if parent is None or parent == "" else fc.element_name(parent)
            names = fc.server(server).elements(database, dimension, parent)
            return tuple((n,) for n in names) or (("",),)
        except fc.FluxError as e:
            return ((problem(e),),)
        except Exception as e:
            return ((problem(f"{type(e).__name__}: {e}"),),)

    def server(self, server):
        try:
            return fc.server(server).info()
        except fc.FluxError as e:
            return problem(e)
        except Exception as e:
            return problem(f"{type(e).__name__}: {e}")


JOB = "org.fluxolap.calc.WriteBackJob"


class WriteBackJob(unohelper.Base, XJob):
    """Runs when a document view is created; spreadsheets get write-back and
    double-click navigation of pasted views."""

    def __init__(self, ctx):
        self.ctx = ctx

    def execute(self, args):
        env = next((a.Value for a in args if a.Name == "Environment"), ())
        model = next((v.Value for v in env if v.Name == "Model"), None)
        if model is not None and model.supportsService("com.sun.star.sheet.SpreadsheetDocument"):
            import fluxolap_views  # only loaded when there's a spreadsheet
            import fluxolap_writeback

            fluxolap_writeback.attach(self.ctx, model)
            fluxolap_views.attach(self.ctx, model)
        return None

    def getImplementationName(self):
        return JOB

    def supportsService(self, name):
        return name == JOB

    def getSupportedServiceNames(self):
        return (JOB,)


COMMANDS = "org.fluxolap.calc.Commands"


class Commands(unohelper.Base, XJobExecutor):
    """The fluxolap menu: its items call `service:org.fluxolap.calc.Commands?<command>`."""

    def __init__(self, ctx):
        self.ctx = ctx

    def trigger(self, command):
        import fluxolap_commands  # loaded on first use

        fluxolap_commands.run(self.ctx, command)

    def getImplementationName(self):
        return COMMANDS

    def supportsService(self, name):
        return name == COMMANDS

    def getSupportedServiceNames(self):
        return (COMMANDS,)


PANEL_FACTORY = "org.fluxolap.calc.PanelFactory"


class PanelFactory(unohelper.Base, XUIElementFactory):
    """The sidebar's fluxolap panel (Sidebar.xcu, Factories.xcu)."""

    def __init__(self, ctx):
        self.ctx = ctx

    def createUIElement(self, url, args):
        import fluxolap_sidebar  # loaded when the panel is first shown

        props = {a.Name: a.Value for a in args}
        return fluxolap_sidebar.PanelElement(self.ctx, props.get("Frame"), props.get("ParentWindow"), url)

    def getImplementationName(self):
        return PANEL_FACTORY

    def supportsService(self, name):
        return name == PANEL_FACTORY

    def getSupportedServiceNames(self):
        return (PANEL_FACTORY,)


g_ImplementationHelper = unohelper.ImplementationHelper()
g_ImplementationHelper.addImplementation(FluxolapAddIn, IMPLEMENTATION, SERVICES)
g_ImplementationHelper.addImplementation(WriteBackJob, JOB, (JOB,))
g_ImplementationHelper.addImplementation(Commands, COMMANDS, (COMMANDS,))
g_ImplementationHelper.addImplementation(PanelFactory, PANEL_FACTORY, (PANEL_FACTORY,))
