"""The fluxolap deck in Calc's sidebar (Sidebar.xcu, Factories.xcu): the
document's server and whether it answers, the menu's commands as buttons,
and the document's pasted views -- pick one to go to it, Change... to
edit it. It only calls what the menu does (fluxolap_commands) and the
views (fluxolap_views); controls are placed in pixels and stretch with
the sidebar's width.
"""

import traceback

import uno
import unohelper
from com.sun.star.awt import XActionListener, XItemListener, XWindowListener
from com.sun.star.ui import XSidebarPanel, XToolPanel, XUIElement

import fluxolap_client as fc
import fluxolap_commands as fcmd
import fluxolap_views as fv

MARGIN, LINE, BUTTON, GAP = 6, 18, 26, 4
BUTTONS = [("paste", "Paste view…"), ("refresh", "Refresh values"), ("load", "Load database…"), ("connect", "Connect…")]


class PanelElement(unohelper.Base, XUIElement):
    def __init__(self, ctx, frame, parent, url):
        self.Frame, self.ResourceURL = frame, url
        self.Type = uno.getConstantByName("com.sun.star.ui.UIElementType.TOOLPANEL")
        self.panel = Panel(ctx, frame, parent)

    def getRealInterface(self):
        return self.panel


class _Listener(unohelper.Base, XActionListener, XItemListener, XWindowListener):
    def __init__(self, fn):
        self.fn = fn

    def actionPerformed(self, _event):
        self.fn()

    def itemStateChanged(self, _event):
        self.fn()

    def windowResized(self, _event):
        self.fn()

    def windowMoved(self, _event):
        pass

    def windowShown(self, _event):
        pass

    def windowHidden(self, _event):
        pass

    def disposing(self, _event):
        pass


class Panel(unohelper.Base, XToolPanel, XSidebarPanel):
    def __init__(self, ctx, frame, parent):
        self.ctx, self.frame = ctx, frame
        smgr = ctx.ServiceManager
        self.toolkit = smgr.createInstanceWithContext("com.sun.star.awt.Toolkit", ctx)
        self.container = smgr.createInstanceWithContext("com.sun.star.awt.UnoControlContainer", ctx)
        self.container.setModel(smgr.createInstanceWithContext("com.sun.star.awt.UnoControlContainerModel", ctx))
        self.container.createPeer(self.toolkit, parent)
        self.window = self.container.getPeer()
        self.Window = self.window  # XToolPanel's attribute: the sidebar reads it
        self.controls = {}
        self.views = []  # (name, address) as listed
        self.updating = False
        self._build()
        self.window.addWindowListener(_Listener(self._layout))
        self.update()

    # -- building ---------------------------------------------------------------

    def _add(self, kind, name, **props):
        smgr = self.ctx.ServiceManager
        control = smgr.createInstanceWithContext(f"com.sun.star.awt.UnoControl{kind}", self.ctx)
        model = smgr.createInstanceWithContext(f"com.sun.star.awt.UnoControl{kind}Model", self.ctx)
        for k, v in props.items():
            setattr(model, k, v)
        control.setModel(model)
        self.container.addControl(name, control)
        self.controls[name] = control
        return control

    def _build(self):
        self._add("FixedText", "server", Label="")
        self._add("FixedText", "status", Label="")
        for command, label in BUTTONS:
            button = self._add("Button", command, Label=label)
            button.addActionListener(_Listener(lambda c=command: self._run(c)))
        self._add("FixedText", "views_label", Label="Views in this document")
        views = self._add("ListBox", "views", Dropdown=False)
        views.addItemListener(_Listener(self._go_to))
        change = self._add("Button", "change", Label="Change view…")
        change.addActionListener(_Listener(self._change))
        self._layout()

    def _layout(self):
        """Places the controls for the panel's current width."""
        width = max(self.getMinimalWidth(), self.window.getPosSize().Width) - 2 * MARGIN
        y = MARGIN
        place = 15  # PosSize.POSSIZE
        self.controls["server"].setPosSize(MARGIN, y, width, LINE, place)
        y += LINE
        self.controls["status"].setPosSize(MARGIN, y, width, LINE, place)
        y += LINE + GAP
        for command, _ in BUTTONS:
            self.controls[command].setPosSize(MARGIN, y, width, BUTTON, place)
            y += BUTTON + GAP
        y += GAP
        self.controls["views_label"].setPosSize(MARGIN, y, width, LINE, place)
        y += LINE
        self.controls["views"].setPosSize(MARGIN, y, width, 110, place)
        y += 110 + GAP
        self.controls["change"].setPosSize(MARGIN, y, width, BUTTON, place)
        self.height = y + BUTTON + MARGIN

    # -- content ------------------------------------------------------------------

    def _doc(self):
        controller = self.frame.getController() if self.frame else None
        return controller.getModel() if controller else None

    def update(self):
        """The server's state and the document's views."""
        doc = self._doc()
        if doc is None or not doc.supportsService("com.sun.star.sheet.SpreadsheetDocument"):
            return
        server = fcmd.document_server(doc)
        self.controls["server"].setText(f"Server: {server}")
        try:
            self.controls["status"].setText(fc.server(server).info())
        except fc.FluxError as e:
            self.controls["status"].setText(f"not reachable: {e}")
        self.views = []
        ranges = doc.NamedRanges
        for name in sorted(ranges.getElementNames()):
            if not name.startswith(fv.PREFIX):
                continue
            cells = ranges.getByName(name).getReferredCells()
            spec = fv.load(doc, name)
            if cells is None or spec is None:
                continue
            where = cells.AbsoluteName.split(":")[0].replace("$", "")
            self.views.append((f"{spec['cube']} · {where}", cells.getRangeAddress()))
        self.updating = True
        try:
            box = self.controls["views"]
            box.removeItems(0, box.getItemCount())
            box.addItems(tuple(label for label, _ in self.views), 0)
        finally:
            self.updating = False

    def _selected_view(self):
        pos = self.controls["views"].getSelectedItemPos()
        return self.views[pos][1] if 0 <= pos < len(self.views) else None

    def _go_to(self):
        """Selects the view's title cell, which scrolls it into sight."""
        a = self._selected_view()
        doc = self._doc()
        if self.updating or a is None or doc is None:
            return
        sheet = doc.Sheets.getByIndex(a.Sheet)
        doc.getCurrentController().setActiveSheet(sheet)
        doc.getCurrentController().select(sheet.getCellByPosition(a.StartColumn, a.StartRow))

    def _change(self):
        a = self._selected_view()
        doc = self._doc()
        if a is None or doc is None:
            return
        try:
            fv.act(self.ctx, doc, a.Sheet, a.StartRow, a.StartColumn)  # like a double-click on the title
        except Exception:
            fcmd.message(self.ctx, doc, traceback.format_exc(limit=4))
        self.update()

    def _run(self, command):
        fcmd.run(self.ctx, command)
        self.update()

    # -- XToolPanel, XSidebarPanel ------------------------------------------------------

    def getWindow(self):
        return self.window

    def createAccessible(self, parent):
        return self.window  # the window peer is the XAccessible

    def getHeightForWidth(self, width):
        return uno.createUnoStruct("com.sun.star.ui.LayoutSize", self.height, self.height, self.height)

    def getMinimalWidth(self):
        return 180
