"""The extension in real LibreOffice Calc, headless: builds fluxolap.oxt,
installs it into a temporary profile, types FLUX.* formulas into a sheet
and checks the results. Skipped without LibreOffice or Python UNO (run it
with the Python that has `uno`, e.g. /usr/bin/python3 on Debian/Ubuntu):

    /usr/bin/python3 -m unittest discover clients/libreoffice/tests
"""

import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from server import Fluxolap  # noqa: E402

try:
    import uno
    from com.sun.star.beans import PropertyValue
except ImportError:
    uno = None

WORLD_1950 = 2_493_093_673.0


def clean_env():
    """LibreOffice's embedded Python must not see an activated virtualenv:
    installing a Python extension then fails (std::bad_alloc)."""
    env = {k: v for k, v in os.environ.items() if not k.startswith(("VIRTUAL_ENV", "PYTHON"))}
    venv = os.environ.get("VIRTUAL_ENV")
    if venv:
        env["PATH"] = os.pathsep.join(p for p in env.get("PATH", "").split(os.pathsep) if not p.startswith(venv))
    return env


@unittest.skipIf(uno is None or not shutil.which("soffice"), "needs LibreOffice and Python UNO")
class CalcTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.flux = Fluxolap()
        cls.tmp = tempfile.mkdtemp(prefix="fluxolap-calc-")
        oxt = os.path.join(cls.tmp, "fluxolap.oxt")
        subprocess.run([sys.executable, os.path.join(HERE, "..", "build.py"), oxt], check=True, stdout=subprocess.DEVNULL)
        profile = f"-env:UserInstallation=file://{cls.tmp}/profile"
        subprocess.run(["unopkg", "add", "--suppress-license", profile, oxt], check=True, env=clean_env(), timeout=180)
        pipe = f"fluxtest{os.getpid()}"
        cls.office = subprocess.Popen(["soffice", "--headless", "--norestore", "--nologo", profile, f"--accept=pipe,name={pipe};urp;"],
                                      env=clean_env(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        local = uno.getComponentContext()
        resolver = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
        for _ in range(300):
            try:
                cls.ctx = resolver.resolve(f"uno:pipe,name={pipe};urp;StarOffice.ComponentContext")
                break
            except Exception:
                time.sleep(0.2)
        else:
            raise RuntimeError("LibreOffice didn't start")
        desktop = cls.ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", cls.ctx)
        hidden = PropertyValue()
        hidden.Name, hidden.Value = "Hidden", True
        cls.doc = desktop.loadComponentFromURL("private:factory/scalc", "_blank", 0, (hidden,))
        cls.sheet = cls.doc.Sheets.getByIndex(0)

    @classmethod
    def tearDownClass(cls):
        try:
            cls.doc.close(True)
        except Exception:
            pass
        cls.office.terminate()
        cls.office.wait(30)
        cls.flux.stop()
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def cell(self, address):
        return self.sheet.getCellRangeByName(address)

    def formula(self, address, text):
        self.cell(address).setFormula(text.replace("{S}", f'"{self.flux.address}"'))
        self.doc.calculateAll()
        return self.cell(address)

    def test_data_reads_numbers_and_explains_problems(self):
        c = self.formula("A1", '=FLUX.DATA({S};"WorldPopulation";"Population";"World";1950;"Both sexes";"All ages")')
        self.assertEqual(c.getValue(), WORLD_1950)
        self.assertEqual(c.FormulaResultType2, 1, "a number, not text")
        c = self.formula("A2", '=FLUX.DATA({S};"WorldPopulation";"Population";"Atlantis";1950;"Both sexes";"All ages")')
        self.assertEqual(c.getString(), "#FLUX: element 'Atlantis' not found in dimension 'Location'")
        self.assertTrue(self.formula("A3", "=FLUX.SERVER({S})").getString().startswith("fluxolap "))

    def test_area_from_ranges_of_names(self):
        for address, name in (("B10", "World"), ("B11", "Africa"), ("C9", "1950"), ("D9", "2000")):
            self.cell(address).setString(name)
        self.cell("C10:D11").setArrayFormula(f'=FLUX.AREA("{self.flux.address}";"WorldPopulation";"Population";B10:B11;C9:D9;"Both sexes";"All ages")')
        self.doc.calculateAll()
        self.assertEqual(self.cell("C10").getValue(), WORLD_1950)
        self.assertGreater(self.cell("D11").getValue(), self.cell("C11").getValue(), "Africa grew from 1950 to 2000")

    def test_elements_as_a_column(self):
        self.cell("G1:G3").setArrayFormula(f'=FLUX.ELEMENTS("{self.flux.address}";"NobelPrizes";"Category";"Natural sciences")')
        self.doc.calculateAll()
        self.assertEqual([self.cell(f"G{r}").getString() for r in (1, 2, 3)], ["Physics", "Chemistry", "Physiology or Medicine"])

    def type_into(self, address, text):
        """Types into a cell the way the user does (through the input line),
        which is what write-back reacts to -- setting a cell through the API
        doesn't count as typing."""
        controller = self.doc.getCurrentController()
        controller.select(self.cell(address))
        dispatcher = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.DispatchHelper", self.ctx)
        text_arg = PropertyValue()
        text_arg.Name, text_arg.Value = "StringName", text
        dispatcher.executeDispatch(controller.getFrame(), ".uno:EnterString", "", 0, (text_arg,))

    def wait_for_formula(self, address):
        for _ in range(100):  # write-back runs just after the input
            if self.cell(address).getFormula().startswith("="):
                return
            time.sleep(0.05)

    def server_value(self, path):
        import json
        import urllib.request
        db = self.flux.get("/databases?name=NobelPrizes")["items"][0]["id"]
        cube = self.flux.get(f"/databases/{db}/cubes?name=Laureates")["items"][0]["id"]
        request = urllib.request.Request(f"http://{self.flux.address}/api/v1/databases/{db}/cubes/{cube}/cells/query",
                                         json.dumps({"paths": [path]}).encode(), {"content-type": "application/json"})
        return json.load(urllib.request.urlopen(request))["items"][0]["value"]

    def test_typing_over_a_total_writes_it_back_and_keeps_the_formula(self):
        total = ["Physics", "1901", "World", "All laureates", "Laureates"]
        self.cell("K1").setString("Physics")
        self.type_into("J1", f'=FLUX.DATA("{self.flux.address}";"NobelPrizes";"Laureates";K1;1901;"World";"All laureates";"Laureates")')
        self.assertEqual(self.cell("J1").getValue(), 1.0)
        self.type_into("J1", "5")
        self.wait_for_formula("J1")
        self.assertTrue(self.cell("J1").getFormula().upper().startswith("=ORG.FLUXOLAP.CALC.FLUXOLAPADDIN.DATA("), "the formula stays")
        self.assertEqual(self.cell("J1").getValue(), 5.0, "and shows the new value")
        self.assertEqual(self.server_value(total), 5.0)
        self.assertEqual(self.server_value(["Physics", "1901", "Germany", "Male", "Laureates"]), 5.0, "splashed down to the base cell")
        # Delete only brings the formula back; nothing is cleared on the server.
        self.type_into("J1", "")
        self.wait_for_formula("J1")
        self.assertEqual(self.server_value(total), 5.0)

    def test_ordinary_cells_and_calculated_arguments_are_left_alone(self):
        self.type_into("L1", "hello")
        time.sleep(0.5)
        self.assertEqual(self.cell("L1").getString(), "hello", "an ordinary cell keeps what was typed")
        self.cell("K2").setString("Chemistry")
        self.type_into("J2", f'=FLUX.DATA("{self.flux.address}";"NobelPrizes";"Laureates";K2&"";1901;"World";"All laureates";"Laureates")')
        before = self.server_value(["Chemistry", "1901", "World", "All laureates", "Laureates"])
        self.type_into("J2", "7")
        self.wait_for_formula("J2")
        self.assertTrue(self.cell("J2").getFormula().startswith("="), "the formula comes back")
        self.assertEqual(self.server_value(["Chemistry", "1901", "World", "All laureates", "Laureates"]), before, "but nothing is written")

    # -- the fluxolap menu ------------------------------------------------------

    def commands(self):
        """A visible document (the commands work on the active one) and the Commands service."""
        if not hasattr(self, "_view_doc"):
            desktop = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", self.ctx)
            type(self)._view_doc = desktop.loadComponentFromURL("private:factory/scalc", "_blank", 0, ())
        self._view_doc.getCurrentController().getFrame().activate()  # the commands work on the active document
        # A test can work on its own sheet (on_sheet): views insert whole columns and rows to make room.
        sheets = self._view_doc.Sheets
        name = f"Sheet{getattr(self, 'on_sheet', 0) + 1}"  # by name: a chart's hidden sheet comes in between
        if not sheets.hasByName(name):
            sheets.insertNewByName(name, sheets.getCount())
        sheet = sheets.getByName(name)
        self._view_doc.getCurrentController().setActiveSheet(sheet)
        cmds = self.ctx.ServiceManager.createInstanceWithContext("org.fluxolap.calc.Commands", self.ctx)
        return self._view_doc, sheet, cmds

    def result(self, doc):
        return doc.getDocumentProperties().getUserDefinedProperties().getPropertyValue("fluxolap result")

    def paste(self, at, **spec):
        import json
        doc, sheet, cmds = self.commands()
        doc.getCurrentController().select(sheet.getCellRangeByName(at))
        cmds.trigger("paste:" + json.dumps({"server": self.flux.address, **spec}))
        return doc, sheet

    def test_the_menu_and_its_dialogs(self):
        config = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.configuration.ConfigurationProvider", self.ctx)
        path = PropertyValue()
        path.Name, path.Value = "nodepath", "/org.openoffice.Office.Addons/AddonUI/OfficeMenuBar/org.fluxolap.calc.menu/Submenu"
        menu = config.createInstanceWithArguments("com.sun.star.configuration.ConfigurationAccess", (path,))
        urls = [menu.getByName(n).getByName("URL") for n in sorted(menu.getElementNames())]
        self.assertEqual([u.split("?")[-1] for u in urls if u.startswith("service:")], ["paste", "refresh", "connect", "load"])
        doc, _, cmds = self.commands()
        props = doc.getDocumentProperties().getUserDefinedProperties()
        if props.getPropertySetInfo().hasPropertyByName("fluxolap server"):
            props.setPropertyValue("fluxolap server", self.flux.address)
        else:
            props.addProperty("fluxolap server", 128, self.flux.address)
        cmds.trigger("check-dialogs")
        self.assertEqual(self.result(doc), "ok", "every dialog is built, started and its lists can be picked from")

    def test_paste_view_writes_a_report_whose_filters_can_be_changed(self):
        doc, sheet = self.paste("B2", database="WorldPopulation", cube="Population",
                                rows={"dimension": "Location", "parent": "World", "total": True},
                                columns={"dimension": "Year", "parent": None},
                                filters={"Sex": "Both sexes", "Age": "All ages"}, style="DATA")
        self.assertTrue(self.result(doc).startswith("fluxolap_view_"), self.result(doc))
        cell = lambda a: sheet.getCellRangeByName(a)  # noqa: E731
        self.assertEqual([cell(f"B{r}").getString() for r in range(2, 8)], ["Population", "Server", "Database", "Cube", "Sex", "Age"])
        self.assertEqual((cell("C9").getString(), cell("B10").getFormula(), cell("B11").getFormula()), ("1950", "World", "Africa"))
        self.assertEqual((cell("B10").getString(), cell("B11").getString()), ("▾ World", "▸ Africa"), "markers: open and closed totals")
        self.assertEqual(cell("C10").getValue(), WORLD_1950)
        self.assertIn("$C$6", cell("C10").getFormula(), "the formula refers to the Sex filter cell")
        cell("C6").setString("Female")
        doc.calculateAll()
        self.assertLess(cell("C10").getValue(), WORLD_1950 / 1.9, "another slice: women only")
        self.assertEqual(doc.getDocumentProperties().getUserDefinedProperties().getPropertyValue("fluxolap server"), self.flux.address,
                         "the document remembers its server")
        # Typing over a value of the pasted view writes it back (its arguments are the header cells).
        controller = doc.getCurrentController()
        controller.select(cell("D11"))  # Africa, 1955, Female
        dispatcher = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.DispatchHelper", self.ctx)
        text = PropertyValue()
        text.Name, text.Value = "StringName", "123456789"
        dispatcher.executeDispatch(controller.getFrame(), ".uno:EnterString", "", 0, (text,))
        for _ in range(100):
            if cell("D11").getFormula().startswith("="):
                break
            time.sleep(0.05)
        self.assertTrue(cell("D11").getFormula().startswith("="), "the formula stays")
        self.assertEqual(cell("D11").getValue(), 123456789.0, "and shows the written value")
        # Refresh reads everything again (and the value is really on the server).
        before = self.flux.requests()
        self.commands()[2].trigger("refresh")
        self.assertGreater(self.flux.requests(), before)
        self.assertEqual(cell("D11").getValue(), 123456789.0)

    def test_paste_view_as_one_block_and_refused_mistakes(self):
        doc, sheet = self.paste("H20", database="NobelPrizes", cube="Laureates",
                                rows={"dimension": "Category", "parent": "All prizes", "total": True},
                                columns={"dimension": "Gender", "parent": "All laureates", "total": False},
                                filters={"Year": "All years", "Birth country": "World", "Measure": "Laureates"}, style="AREA")
        self.assertTrue(self.result(doc).startswith("fluxolap_view_"), self.result(doc))
        # Title, 3 settings and 3 filters (rows 20-26), a blank row, the column
        # headers in row 28 (Female, Male, Organization), the rows from 29.
        self.assertEqual((sheet.getCellRangeByName("H28").getString(), sheet.getCellRangeByName("I28").getString()), ("Category / Gender", "Female"))
        self.assertEqual(sheet.getCellRangeByName("H29").getFormula(), "All prizes")
        self.assertTrue(sheet.getCellRangeByName("I29").getFormula().upper().startswith("{=ORG.FLUXOLAP.CALC.FLUXOLAPADDIN.AREA("), "one array formula")
        women = sheet.getCellRangeByName("I29").getValue()
        self.assertGreater(women, 50)
        self.paste("A40", database="NobelPrizes", cube="Laureates", rows={"dimension": "Category"}, columns=None, filters={})
        self.assertIn("choose an element for", self.result(doc))

    # -- navigating a pasted view (double-click) ---------------------------------

    def navigate(self, address):
        doc, _, cmds = self.commands()
        cmds.trigger(f"navigate:{address}")
        return self.result(doc)

    def column(self, sheet, col, first, last):
        """The raw texts of a column's cells (not the displayed "▸ " markers)."""
        out = []
        for r in range(first, last + 1):
            c = sheet.getCellRangeByName(f"{col}{r}")
            out.append(c.getFormula().lstrip("'"))
        return out

    def test_double_click_opens_and_closes_totals(self):
        # Title B60, settings 61-63, filters Year (64) and Age (65), headers 67, rows from 68.
        doc, sheet = self.paste("B60", database="WorldPopulation", cube="Population",
                                rows={"dimension": "Location", "parent": "World", "total": True},
                                columns={"dimension": "Sex", "parent": "Both sexes", "total": True},
                                filters={"Year": "1950", "Age": "All ages"})
        cell = lambda a: sheet.getCellRangeByName(a)  # noqa: E731
        start = self.column(sheet, "B", 68, 70)
        self.assertEqual(start[:2], ["World", "Africa"])
        self.assertEqual(cell("B69").getString(), "▸ Africa", "a closed total shows ▸ (only formatting)")
        self.assertEqual(cell("B68").getString(), "▾ World")
        self.assertEqual(self.navigate("$B$69"), "toggle")
        rows = self.column(sheet, "B", 68, 72)
        self.assertEqual(rows[:2], ["World", "Africa"])
        self.assertNotEqual(rows[2], start[2], "Africa's regions follow it")
        self.assertEqual(cell("B69").getString(), "▾ Africa")
        self.assertGreater(cell("C70").getValue(), 0, "and their values are there")
        self.assertEqual(self.navigate("$B$69"), "toggle")
        self.assertEqual(self.column(sheet, "B", 68, 70), start, "closed again")
        # Columns: Both sexes is open (Male, Female next to it); close it.
        self.assertEqual(self.column(sheet, "C", 67, 67) + [cell("D67").getFormula(), cell("E67").getFormula()], ["Both sexes", "Male", "Female"])
        self.assertEqual(self.navigate("$C$67"), "toggle")
        self.assertEqual((cell("C67").getFormula(), cell("D67").getFormula()), ("Both sexes", ""), "the columns it no longer needs are cleared")
        # A filter typed by hand stays when the view is rebuilt.
        cell("C64").setString("2000")
        self.assertEqual(self.navigate("$C$67"), "toggle")
        self.assertEqual(cell("C64").getFormula().lstrip("'"), "2000")
        # Filters and the title open dialogs (here only reported).
        self.assertEqual(self.navigate("$C$64"), "filter")
        self.assertEqual(self.navigate("$B$60"), "edit")
        self.assertEqual(self.navigate("$C$70"), "ok", "a value cell: nothing to do")

    def click(self, address):
        """A double-click as Calc hands it to the extension's click handler."""
        doc, _, cmds = self.commands()
        cmds.trigger("click-state")
        self.assertEqual(self.result(doc), "attached", "the window has a double-click handler")
        cmds.trigger(f"click:{address}")
        result = self.result(doc)
        time.sleep(0.3)  # the action runs right after the click
        return result

    def test_double_click_through_the_handler_with_two_views_side_by_side(self):
        # Two identical views next to each other: B120 and G120 (3 filters each, header 128, rows from 129: World, Africa).
        choice = dict(database="WorldPopulation", cube="Population", rows={"dimension": "Location", "parent": "World", "total": True},
                      columns=None, filters={"Year": "1950", "Sex": "Both sexes", "Age": "All ages"})
        doc, sheet = self.paste("B120", **choice)
        self.paste("G120", **choice)
        cell = lambda a: sheet.getCellRangeByName(a)  # noqa: E731
        self.assertEqual((cell("B130").getString(), cell("G130").getString()), ("▸ Africa", "▸ Africa"))
        self.assertEqual(self.click("$G$130"), "handled", "a total: handled (Calc doesn't start editing)")
        self.assertEqual(cell("G130").getString(), "▾ Africa", "the right view opened Africa")
        self.assertEqual(cell("B130").getString(), "▸ Africa", "the left one is untouched")
        self.assertEqual(self.click("$B$130"), "handled")
        self.assertEqual(cell("B130").getString(), "▾ Africa", "now the left one too")
        self.assertEqual(self.click("$G$130"), "handled")
        self.assertEqual(cell("G130").getString(), "▸ Africa", "and the right one closed again")
        self.assertEqual(self.click("$C$129"), "passed on", "a value cell: Calc edits it as usual")
        self.assertEqual(self.click("$A$1"), "passed on", "outside the views")

    def test_a_view_beside_another_makes_room_when_it_grows(self):
        # Your case: two identical views side by side; the left one opens a column total.
        # Title 140, filters Year and Age (144-145), headers 147, rows from 148 (World, Africa).
        choice = dict(database="WorldPopulation", cube="Population", rows={"dimension": "Location", "parent": "World", "total": True},
                      columns={"dimension": "Sex", "parent": "Both sexes", "total": True}, filters={"Year": "1950", "Age": "All ages"})
        doc, sheet = self.paste("B140", **choice)
        self.assertEqual(self.navigate("$C$147"), "toggle", "close Both sexes: the left view is now B:C")
        self.paste("D140", **choice)  # the right view right next to it, D:F (Both sexes, Male, Female)
        cell = lambda a: sheet.getCellRangeByName(a)  # noqa: E731
        self.assertEqual((cell("D140").getString(), cell("D147").getFormula()), ("Population", "Location / Sex"))
        # Opening Both sexes in the left view needs D:E -- where the right view is.
        self.assertEqual(self.click("$C$147"), "handled")
        self.assertEqual([cell(f"{c}147").getFormula() for c in "CDE"], ["Both sexes", "Male", "Female"], "the left view grew")
        self.assertEqual((cell("F140").getString(), cell("F147").getFormula(), cell("G147").getFormula()), ("Population", "Location / Sex", "Both sexes"),
                         "the right view moved two columns to the right, as a whole")
        self.assertGreater(cell("G148").getValue(), 0, "with its values")
        # ... and it's still a view: its Africa opens.
        self.assertEqual(self.click("$F$149"), "handled")
        self.assertEqual(cell("F149").getString(), "▾ Africa")
        # Closing Both sexes again: the left view shrinks, the right one moves back next to it.
        self.assertEqual(self.click("$C$147"), "handled")
        self.assertEqual((cell("C147").getFormula(), cell("D140").getString(), cell("D149").getString()), ("Both sexes", "Population", "▾ Africa"),
                         "the right view is back at D, still with Africa open")

    # -- pivots: several dimensions per axis ------------------------------------------

    def raw(self, sheet, address):
        return sheet.getCellRangeByName(address).getFormula().lstrip("'")

    def test_a_pivot_with_two_row_dimensions(self):
        self.on_sheet = 1  # pivots are wide: on a sheet of their own
        # Rows Location x Sex, columns Year; filter Age. Title B200, settings 201-203,
        # filter 204, header 206, rows from 207; row headers in B and C, values from D.
        doc, sheet = self.paste("B200", database="WorldPopulation", cube="Population",
                                rows=[{"dimension": "Location", "parent": "World", "total": True},
                                      {"dimension": "Sex", "parent": "Both sexes", "total": True}],
                                columns=[{"dimension": "Year", "parent": None}], filters={"Age": "All ages"})
        self.assertTrue(self.result(doc).startswith("fluxolap_view_"), self.result(doc))
        cell = lambda a: sheet.getCellRangeByName(a)  # noqa: E731
        self.assertEqual((self.raw(sheet, "B206"), self.raw(sheet, "C206"), self.raw(sheet, "D206")), ("Location", "Sex / Year", "1950"))
        self.assertEqual([(self.raw(sheet, f"B{r}"), self.raw(sheet, f"C{r}")) for r in (207, 208, 209, 210)],
                         [("World", "Both sexes"), ("World", "Male"), ("World", "Female"), ("Africa", "Both sexes")])
        self.assertEqual((cell("B207").getString(), cell("B208").getString()), ("▾ World", ""), "a repeated label is hidden")
        self.assertEqual(cell("D207").getValue(), WORLD_1950)
        self.assertAlmostEqual(cell("D208").getValue() + cell("D209").getValue(), WORLD_1950, delta=1)
        self.assertIn("$B207", cell("D207").getFormula())
        self.assertIn("$C207", cell("D207").getFormula())
        # Opening Africa (outer dimension): its regions follow, for every sex.
        self.assertEqual(self.navigate("$B$210"), "toggle")
        region = self.raw(sheet, "B213")
        self.assertNotIn(region, ("Africa", "Americas", ""), "an African region")
        self.assertEqual(self.raw(sheet, "C213"), "Both sexes")
        # Closing Both sexes (inner dimension): every location keeps one row.
        self.assertEqual(self.navigate("$C$207"), "toggle")
        self.assertEqual([(self.raw(sheet, f"B{r}"), self.raw(sheet, f"C{r}")) for r in (207, 208)], [("World", "Both sexes"), ("Africa", "Both sexes")])

    def test_a_pivot_with_two_column_dimensions(self):
        self.on_sheet = 2
        # Rows Location; columns Sex x Age; filter Year. Title B240, settings 241-243, filter 244,
        # header rows 246 (Sex) and 247 (Age), rows from 248; values from C.
        doc, sheet = self.paste("B240", database="WorldPopulation", cube="Population",
                                rows=[{"dimension": "Location", "parent": "World", "total": True}],
                                columns=[{"dimension": "Sex", "parent": "Both sexes", "total": True},
                                         {"dimension": "Age", "parent": "All ages", "total": False}], filters={"Year": "1950"})
        cell = lambda a: sheet.getCellRangeByName(a)  # noqa: E731
        self.assertEqual((self.raw(sheet, "B246"), self.raw(sheet, "B247")), ("Sex", "Location / Age"))
        self.assertEqual((self.raw(sheet, "C246"), self.raw(sheet, "D246"), cell("D246").getString()), ("Both sexes", "Both sexes", ""),
                         "Sex repeats over the ages, hidden")
        first_age = self.raw(sheet, "C247")
        self.assertNotEqual(first_age, self.raw(sheet, "D247"), "the ages vary fastest")
        self.assertGreater(cell("C248").getValue(), 0)
        width = len([c for c in range(2, 200) if sheet.getCellByPosition(c, 246).getFormula()])
        self.assertEqual(self.navigate("$C$246"), "toggle", "close Both sexes")
        narrower = len([c for c in range(2, 200) if sheet.getCellByPosition(c, 246).getFormula()])
        self.assertEqual(narrower * 3, width, "Male and Female are gone: a third of the columns")

    def test_one_block_needs_one_dimension_per_axis(self):
        doc, _ = self.paste("B300", database="WorldPopulation", cube="Population",
                            rows=[{"dimension": "Location", "parent": "World"}, {"dimension": "Sex", "parent": "Both sexes"}],
                            columns=[{"dimension": "Year", "parent": None}], filters={"Age": "All ages"}, style="AREA")
        self.assertIn("one block", self.result(doc))

    def test_a_view_saved_in_the_old_form_still_navigates(self):
        import json
        doc, sheet = self.paste("B320", database="WorldPopulation", cube="Population",
                                rows={"dimension": "Location", "parent": "World", "total": True}, columns=None,
                                filters={"Year": "1950", "Sex": "Both sexes", "Age": "All ages"})
        name = self.result(doc)
        props = doc.getDocumentProperties().getUserDefinedProperties()
        spec = json.loads(props.getPropertyValue(name))
        # The form before pivots: one dimension per axis, shown entries as [path, consolidated].
        spec["rows"], spec["columns"] = spec["rows"][0], None
        spec["shown"] = {"rows": [r[0] for r in spec["shown"]["rows"]], "cols": []}
        props.setPropertyValue(name, json.dumps(spec))
        # Title B320, 3 filters (324-326), header 328, rows from 329: World, Africa.
        self.assertEqual(self.navigate("$B$330"), "toggle")
        self.assertEqual(sheet.getCellRangeByName("B330").getString(), "▾ Africa")

    # -- charts ----------------------------------------------------------------------

    def chart(self, sheet, name):
        """(chart type, stacked, [(series name, values)], categories, title) of a view's chart."""
        doc_chart = sheet.getCharts().getByName(name).getEmbeddedObject()
        system = doc_chart.getFirstDiagram().getCoordinateSystems()[0]
        kind = system.getChartTypes()[0]
        series = []
        stacked = False
        for s in kind.getDataSeries():
            seq = s.getDataSequences()[0]
            series.append((seq.getLabel().getData()[0], seq.getValues().getData()))
            stacked = stacked or s.StackingDirection.value == "Y_STACKING"
        categories = system.getAxisByDimension(0, 0).getScaleData().Categories.getValues().getData()
        return kind.getChartType().split(".")[-1], stacked, series, categories, doc_chart.getTitle().String

    def test_a_view_with_a_chart_follows_it(self):
        self.on_sheet = 3
        # Rows Location, columns Sex; filters Year and Age. Title B2, filters 6-7, header 9, rows from 10:
        # World (opened), its regions. Columns C (Both sexes, opened), D (Male), E (Female).
        choice = dict(database="WorldPopulation", cube="Population", rows=[{"dimension": "Location", "parent": "World", "total": True}],
                      columns=[{"dimension": "Sex", "parent": "Both sexes", "total": True}], filters={"Year": "1950", "Age": "All ages"})
        doc, sheet = self.paste("B2", chart="bar", **choice)
        name = self.result(doc)
        self.assertTrue(name.startswith("fluxolap_view_"), name)
        cell = lambda a: sheet.getCellRangeByName(a)  # noqa: E731
        regions = [r for r in self.column(sheet, "B", 11, 30) if r]
        kind, stacked, series, categories, title = self.chart(sheet, name)
        self.assertEqual((kind, stacked, title), ("ColumnChartType", False, "Population"))
        self.assertEqual(categories, ("Male", "Female"), "the opened total Both sexes is left out of bars")
        self.assertEqual([s[0] for s in series], regions, "the rows are the series; the opened World is left out")
        self.assertEqual(series[0][1], (cell("D11").getValue(), cell("E11").getValue()), "linked to the view's cells")
        shape = next(s for s in self.shapes(sheet) if s.PersistName == name)
        self.assertEqual((shape.Position.X, shape.Position.Y), (cell("G2").Position.X, cell("G2").Position.Y), "one column right of the view")
        # Opening Africa: the chart follows, Africa itself goes (its regions are shown).
        self.assertEqual(self.navigate("$B$11"), "toggle")
        names = [s[0] for s in self.chart(sheet, name)[2]]
        self.assertNotIn("Africa", names)
        self.assertGreater(len(names), len(regions) - 1)
        # Stacked, then lines (which keep the totals), then no chart.
        self.change("B2", chart="stacked", **choice)
        self.assertEqual(self.chart(sheet, name)[:2], ("ColumnChartType", True))
        self.change("B2", chart="line", **choice)
        kind, _, series, categories, _ = self.chart(sheet, name)
        self.assertEqual((kind, categories[0], series[0][0]), ("LineChartType", "Both sexes", "World"))
        self.change("B2", chart=None, **choice)
        self.assertFalse(sheet.getCharts().hasByName(name))

    def shapes(self, sheet):
        page = sheet.getDrawPage()
        return [page.getByIndex(i) for i in range(page.getCount())]

    def change(self, at, **choice):
        import json
        doc, _, cmds = self.commands()
        cmds.trigger("change:" + json.dumps({"at": at, "server": self.flux.address, **choice}))
        self.assertTrue(self.result(doc).startswith("fluxolap_view_"), self.result(doc))

    def test_a_pivot_chart_names_its_series_by_every_row_dimension_and_stays_where_you_put_it(self):
        import os
        self.on_sheet = 4
        # Rows Location x Sex, columns Year; filter Age. Title B2, filter 6, header 8, rows from 9.
        doc, sheet = self.paste("B2", database="WorldPopulation", cube="Population", chart="line",
                                rows=[{"dimension": "Location", "parent": "World", "total": True},
                                      {"dimension": "Sex", "parent": "Both sexes", "total": True}],
                                columns=[{"dimension": "Year", "parent": None}], filters={"Age": "All ages"})
        name = self.result(doc)
        _, _, series, categories, _ = self.chart(sheet, name)
        self.assertEqual([s[0] for s in series[:3]], ["World · Both sexes", "World · Male", "World · Female"])
        self.assertEqual(categories[0], "1950")
        self.assertEqual(series[0][1][0], WORLD_1950)
        # Moved by hand: it stays there when the view changes.
        shape = next(s for s in self.shapes(sheet) if s.PersistName == name)
        moved = uno.createUnoStruct("com.sun.star.awt.Point")
        moved.X, moved.Y = 1000, 50000
        shape.Position = moved
        self.assertEqual(self.navigate("$C$9"), "toggle", "close Both sexes")
        self.assertEqual((shape.Position.X, shape.Position.Y), (1000, 50000))
        self.assertEqual([s[0] for s in self.chart(sheet, name)[2]][:2], ["World · Both sexes", "Africa · Both sexes"])
        # Saved and reopened: still the chart, still with its names and values.
        url = uno.systemPathToFileUrl(os.path.join(self.tmp, "chart.ods"))
        doc.storeToURL(url, ())
        desktop = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", self.ctx)
        reopened = desktop.loadComponentFromURL(url, "_blank", 0, ())
        try:
            again = reopened.Sheets.getByName(sheet.Name)
            _, _, series2, categories2, _ = self.chart(again, name)
            self.assertEqual([s[0] for s in series2][:2], ["World · Both sexes", "Africa · Both sexes"])
            self.assertEqual(series2[0][1][0], WORLD_1950)
            self.assertEqual(categories2[0], "1950")
            self.assertFalse(reopened.Sheets.getByName("fluxolap charts").IsVisible, "the names' sheet stays hidden")
        finally:
            reopened.close(True)

    def test_a_view_never_overwrites_other_cells(self):
        doc, sheet = self.paste("H60", database="NobelPrizes", cube="Laureates",
                                rows={"dimension": "Category", "parent": "All prizes", "total": True}, columns=None,
                                filters={"Year": "All years", "Birth country": "World", "Gender": "All laureates", "Measure": "Laureates"})
        # Title H60, 3 settings, 4 filters (64-67), header 69, rows 70-75 (All prizes + 5 children)
        self.assertEqual(self.column(sheet, "H", 70, 71), ["All prizes", "Natural sciences"])
        last = 70 + 6 - 1
        sheet.getCellRangeByName(f"H{last + 2}").setString("mine")
        # Opening Natural sciences (3 more rows) needs the row with "mine": rows are inserted, "mine" moves down.
        self.assertEqual(self.navigate("$H$71"), "toggle")
        self.assertEqual(self.column(sheet, "H", 70, 72), ["All prizes", "Natural sciences", "Physics"])
        self.assertEqual(sheet.getCellRangeByName(f"H{last + 5}").getString(), "mine", "moved down, not overwritten")
        # Closing it again: the rows go away, "mine" moves back up.
        self.assertEqual(self.navigate("$H$71"), "toggle")
        self.assertEqual(sheet.getCellRangeByName(f"H{last + 2}").getString(), "mine", "back where it was")
        # Pasting onto cells that aren't empty is refused as well.
        self.paste(f"H{last + 2}", database="NobelPrizes", cube="Laureates", rows={"dimension": "Category", "parent": "All prizes"}, columns=None,
                   filters={"Year": "All years", "Birth country": "World", "Gender": "All laureates", "Measure": "Laureates"})
        self.assertIn("move it", self.result(doc))

    def test_hiding_empty_rows(self):
        choice = dict(database="NobelPrizes", cube="Laureates", rows={"dimension": "Birth country", "parent": "Europe", "total": False},
                      columns=None, filters={"Category": "Economic Sciences", "Year": "1969", "Gender": "All laureates", "Measure": "Laureates"})
        # Title, 3 settings, 4 filters (64-67), header 69, rows from 70.
        doc, sheet = self.paste("N60", zero=False, **choice)
        all_rows = len([r for r in self.column(sheet, "N", 70, 140) if r])
        self.paste("P60", zero=True, **choice)
        shown = [r for r in self.column(sheet, "P", 70, 140) if r]
        self.assertLess(len(shown), all_rows)
        self.assertEqual(shown, ["Norway", "the Netherlands"], "1969's economics laureates: Frisch and Tinbergen")

    def test_a_saved_view_can_still_be_navigated(self):
        import os
        doc, sheet = self.paste("B90", database="WorldPopulation", cube="Population",
                                rows={"dimension": "Location", "parent": "World", "total": True}, columns=None,
                                filters={"Year": "1950", "Sex": "Both sexes", "Age": "All ages"})
        url = uno.systemPathToFileUrl(os.path.join(self.tmp, "view.ods"))
        doc.storeToURL(url, ())
        desktop = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", self.ctx)
        reopened = desktop.loadComponentFromURL(url, "_blank", 0, ())
        try:
            cmds = self.ctx.ServiceManager.createInstanceWithContext("org.fluxolap.calc.Commands", self.ctx)
            # Title B90, 3 settings, 3 filters (94-96), header 98, rows from 99: World, Africa
            cmds.trigger("navigate:$B$100")
            self.assertEqual(self.result(reopened), "toggle")
            sheet2 = reopened.Sheets.getByIndex(0)
            self.assertEqual(sheet2.getCellRangeByName("B100").getString(), "▾ Africa")
        finally:
            reopened.close(True)

    def test_a_new_sheet_full_of_formulas_is_read_in_a_few_requests(self):
        doc, sheet, _ = self.commands()
        years = list(range(1950, 2101, 5))  # 31 columns
        regions = ["World", "Africa", "Asia", "Europe", "Oceania", "Northern America"]
        doc.enableAutomaticCalculation(False)
        for j, y in enumerate(years):
            sheet.getCellByPosition(30 + j, 199).setString(str(y))
        for i, region in enumerate(regions):
            sheet.getCellByPosition(29, 200 + i).setString(region)
            for j in range(len(years)):
                col = sheet.getCellByPosition(30 + j, 199).AbsoluteName.split(".")[-1].replace("$", "")
                row = 201 + i
                sheet.getCellByPosition(30 + j, 200 + i).setFormula(
                    f'=FLUX.DATA("{self.flux.address}";"WorldPopulation";"Population";$AD{row};{col[:-3]}$200;"Female";"0-4")')
        doc.enableAutomaticCalculation(True)
        before = self.flux.requests()
        doc.calculateAll()
        for _ in range(100):  # the batch and the second pass run right after the calculation
            time.sleep(0.05)
            if all(sheet.getCellByPosition(30 + j, 200 + i).getType().value == "FORMULA" and sheet.getCellByPosition(30 + j, 200 + i).FormulaResultType2 == 1
                   for i in range(len(regions)) for j in range(len(years))):
                break
        n = len(regions) * len(years)
        values = [sheet.getCellByPosition(30 + j, 200 + i).getValue() for i in range(len(regions)) for j in range(len(years))]
        self.assertTrue(all(v > 0 for v in values), "every cell has its value (no … left)")
        self.assertLessEqual(self.flux.requests() - before, 10, f"{n} new formulas: a few requests, not {n}")

    def test_the_sidebar_panel(self):
        doc, sheet = self.paste("Z300", database="NobelPrizes", cube="Laureates",
                                rows={"dimension": "Category", "parent": "All prizes", "total": True}, columns=None,
                                filters={"Year": "All years", "Birth country": "World", "Gender": "All laureates", "Measure": "Laureates"})
        config = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.configuration.ConfigurationProvider", self.ctx)
        path = PropertyValue()
        path.Name, path.Value = "nodepath", "/org.openoffice.Office.UI.Sidebar/Content/PanelList/FluxolapPanel"
        panel_config = config.createInstanceWithArguments("com.sun.star.configuration.ConfigurationAccess", (path,))
        self.assertEqual(panel_config.getByName("DeckId"), "FluxolapDeck")
        # Built the way the sidebar builds it: through the registered factory, into the document's window.
        frame = doc.getCurrentController().getFrame()
        factory = self.ctx.ServiceManager.createInstanceWithContext("org.fluxolap.calc.PanelFactory", self.ctx)
        args = []
        for name, value in (("Frame", frame), ("ParentWindow", frame.getContainerWindow())):
            p = PropertyValue()
            p.Name, p.Value = name, value
            args.append(p)
        element = factory.createUIElement(panel_config.getByName("ImplementationURL"), tuple(args))
        panel = element.getRealInterface()
        self.assertIsNotNone(panel.Window, "the sidebar reads the panel's Window attribute")
        size = panel.getHeightForWidth(260)
        self.assertGreater(size.Preferred, 200, "server, status, 4 buttons, the views and a button")
        self.assertEqual(panel.getMinimalWidth(), 180)
        panel.Window.dispose()

    def test_the_function_wizard_knows_them(self):
        descriptions = self.ctx.ServiceManager.createInstanceWithContext("com.sun.star.sheet.FunctionDescriptions", self.ctx)
        names = set()
        for i in range(descriptions.getCount()):
            props = {p.Name: p.Value for p in descriptions.getByIndex(i)}
            if str(props.get("Name", "")).startswith("FLUX."):
                names.add(props["Name"])
        self.assertEqual(names, {"FLUX.DATA", "FLUX.AREA", "FLUX.ELEMENTS", "FLUX.SERVER"})


if __name__ == "__main__":
    unittest.main()
