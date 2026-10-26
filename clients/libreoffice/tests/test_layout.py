"""The layout of a pasted view (fluxolap_layout), plain Python."""

import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "extension", "pythonpath"))
import fluxolap_layout as fl  # noqa: E402

DIMS = ["Location", "Year", "Sex", "Age"]
ROWS = fl.tuples([[("World", 0, True), ("Africa", 1, True)]])
COLS = fl.tuples([[("1950", 0, False), ("2000", 0, False)]])
FILTERS = {2: "Both sexes", 3: "All ages"}


def grid(view):
    return {(c.row, c.col): c.formula or c.text for c in view.cells}


class LayoutTest(unittest.TestCase):
    def test_settings_headers_and_formulas_in_cube_order(self):
        g = grid(fl.build("localhost:7777", "WorldPopulation", "Population", DIMS, [0], [1], ROWS, COLS, FILTERS))
        self.assertEqual(g[(0, 0)], "Population")
        self.assertEqual([g[(r, 0)] for r in range(1, 6)], ["Server", "Database", "Cube", "Sex", "Age"])
        self.assertEqual(g[(4, 1)], "Both sexes")
        self.assertEqual(g[(7, 0)], "Location / Year")
        self.assertEqual((g[(7, 1)], g[(7, 2)]), ("1950", "2000"))
        self.assertEqual((g[(8, 0)], g[(9, 0)]), ("World", "Africa"))
        self.assertEqual(g[(8, 1)], "=FLUX.DATA($B$2;$B$3;$B$4;$A9;B$8;$B$5;$B$6)")
        self.assertEqual(g[(9, 2)], "=FLUX.DATA($B$2;$B$3;$B$4;$A10;C$8;$B$5;$B$6)")

    def test_rows_and_columns_need_not_be_the_first_dimensions(self):
        # Years down, sexes across; Location and Age are filters, still in cube order.
        g = grid(fl.build("s", "d", "c", DIMS, [1], [2], COLS, fl.tuples([[("Male", 0, False)]]), {0: "World", 3: "All ages"}, top=2, left=1))
        self.assertEqual(g[(2, 1)], "c")
        self.assertEqual([g[(r, 1)] for r in range(3, 8)], ["Server", "Database", "Cube", "Location", "Age"])
        self.assertEqual(g[(10, 2)], "=FLUX.DATA($C$4;$C$5;$C$6;$C$7;$B11;C$10;$C$8)")

    def test_one_area_block(self):
        view = fl.build("s", "d", "c", DIMS, [0], [1], ROWS, COLS, FILTERS, style=fl.AREA)
        self.assertEqual(view.area, (8, 1, 9, 2))
        self.assertEqual(grid(view)[(8, 1)], "=FLUX.AREA($B$2;$B$3;$B$4;$A$9:$A$10;$B$8:$C$8;$B$5;$B$6)")

    def test_every_other_dimension_needs_a_filter(self):
        with self.assertRaises(ValueError):
            fl.build("s", "d", "c", DIMS, [0], [1], ROWS, COLS, {2: "Both sexes"})

    def test_element_lists_from_the_hierarchy(self):
        tree = [
            {"id": 0, "name": "World", "position": 0, "parents": [], "children": [{"id": 1}, {"id": 2}]},
            {"id": 1, "name": "Africa", "position": 1, "parents": [0], "children": [{"id": 3}]},
            {"id": 2, "name": "Asia", "position": 2, "parents": [0], "children": []},
            {"id": 3, "name": "Kenya", "position": 3, "parents": [1], "children": []},
        ]
        self.assertEqual(fl.element_list(tree), [("World", 0, True), ("Africa", 1, True), ("Kenya", 2, False), ("Asia", 1, False)])
        self.assertEqual(fl.element_list(tree, "World"), [("World", 0, True), ("Africa", 1, True), ("Asia", 1, False)])
        self.assertEqual(fl.element_list(tree, "World", with_parent=False), [("Africa", 0, True), ("Asia", 0, False)])

    TREE = [
        {"id": 0, "name": "World", "position": 0, "parents": [], "children": [{"id": 1}, {"id": 2}]},
        {"id": 1, "name": "Africa", "position": 1, "parents": [0], "children": [{"id": 3}]},
        {"id": 2, "name": "Asia", "position": 2, "parents": [0], "children": [{"id": 3}]},
        {"id": 3, "name": "Shared", "position": 3, "parents": [1, 2], "children": []},
    ]

    def test_axis_navigation_by_paths(self):
        roots, expanded = fl.axis_start(self.TREE, "World")
        self.assertEqual([(n, d, o) for n, d, _, _, o in fl.visible(self.TREE, roots, expanded)], [("World", 0, True), ("Africa", 1, False), ("Asia", 1, False)])
        expanded = fl.toggle(expanded, ["World", "Africa"])
        self.assertEqual([(n, p) for n, _, _, p, _ in fl.visible(self.TREE, roots, expanded)],
                         [("World", ["World"]), ("Africa", ["World", "Africa"]), ("Shared", ["World", "Africa", "Shared"]), ("Asia", ["World", "Asia"])])
        # "Shared" is under two parents: opening Asia shows it a second time, as another path.
        expanded = fl.toggle(expanded, ["World", "Asia"])
        self.assertEqual([n for n, *_ in fl.visible(self.TREE, roots, expanded)], ["World", "Africa", "Shared", "Asia", "Shared"])
        # Closing World closes everything under it.
        expanded = fl.toggle(expanded, ["World"])
        self.assertEqual(expanded, [])
        self.assertEqual([n for n, *_ in fl.visible(self.TREE, roots, expanded)], ["World"])

    def test_axis_starts(self):
        self.assertEqual(fl.axis_start(self.TREE, "World", with_parent=False), ([["Africa"], ["Asia"]], []))
        roots, expanded = fl.axis_start(self.TREE)
        self.assertEqual(roots, [["World"]])
        self.assertEqual(len(fl.visible(self.TREE, roots, expanded)), 5, "everything open: Shared twice")

    def test_markers_and_kinds(self):
        rows = fl.tuples([fl.visible(self.TREE, *fl.axis_start(self.TREE, "World"))])
        view = fl.build("s", "d", "c", ["Region", "Measure"], [0], [], rows, [], {1: "Value"})
        markers = {c.text: c.marker for c in view.cells if c.kind == "element"}
        self.assertEqual(markers, {"World": "-", "Africa": "+", "Asia": "+"})
        self.assertEqual({c.kind for c in view.cells}, {"title", "setting", "filter", "header", "element", "value"})
        self.assertEqual(fl.geometry(1), (4, 6, 7))
        self.assertEqual(fl.geometry(1, 2), (4, 6, 8), "two column dimensions: two header rows")

    def test_nested_axes_like_a_pivot(self):
        # Rows Location x Sex, columns Year x Age; no filters left.
        locations = [("World", 0, True, ["World"], True), ("Africa", 1, True, ["World", "Africa"], False)]
        sexes = [("Male", 0, False), ("Female", 0, False)]
        years = [("1950", 0, False), ("2000", 0, False)]
        ages = [("0-4", 0, False)]
        rows, cols = fl.tuples([locations, sexes]), fl.tuples([years, ages])
        self.assertEqual([(r[0][0], r[1][0]) for r in rows], [("World", "Male"), ("World", "Female"), ("Africa", "Male"), ("Africa", "Female")])
        view = fl.build("s", "d", "c", DIMS, [0, 2], [1, 3], rows, cols, {})
        g = grid(view)
        # Title, 3 settings (no filters), blank, header rows 5 (Year) and 6 (Age), data from 7.
        self.assertEqual((g[(5, 1)], g[(6, 0)], g[(6, 1)]), ("Year", "Location", "Sex / Age"), "the corner names the dimensions")
        self.assertEqual((g[(5, 2)], g[(5, 3)], g[(6, 2)]), ("1950", "2000", "0-4"))
        self.assertEqual([g[(r, 0)] for r in range(7, 11)], ["World", "World", "Africa", "Africa"], "every row has its labels, for the formulas")
        hidden = {(c.row, c.col) for c in view.cells if c.hidden}
        self.assertEqual(hidden & {(7, 0), (8, 0), (9, 0), (10, 0)}, {(8, 0), (10, 0)}, "but a repeated one is hidden")
        self.assertEqual(g[(8, 3)], "=FLUX.DATA($B$2;$B$3;$B$4;$A9;D$6;$B9;D$7)", "location, year, sex, age: each from its header")
        self.assertEqual((view.rows, view.cols), (11, 4))

    def test_one_block_takes_one_dimension_per_axis(self):
        rows = fl.tuples([[("World", 0, True)], [("Male", 0, False)]])
        with self.assertRaises(ValueError):
            fl.build("s", "d", "c", DIMS, [0, 2], [1], rows, COLS, {3: "All ages"}, style=fl.AREA)

    def test_charts_leave_opened_totals_out_of_bars(self):
        # (name, depth, consolidated, path, opened)
        world = ("World", 0, True, ["World"], True)
        africa = ("Africa", 1, True, ["World", "Africa"], False)
        asia = ("Asia", 1, True, ["World", "Asia"], False)
        both, male = ("Both sexes", 0, True, ["Both sexes"], True), ("Male", 1, False, ["Both sexes", "Male"], False)
        rows = [(world, both), (world, male), (africa, both), (africa, male), (asia, male)]
        cols = [(("1950", 0, False, ["1950"], False),), (("1955", 0, False, ["1955"], False),)]
        self.assertEqual(fl.chart_plan(rows, cols, "bar"), ([3, 4], [0, 1], 0), "a row with any opened total goes")
        self.assertEqual(fl.chart_plan(rows, cols, "stacked")[0], [3, 4])
        self.assertEqual(fl.chart_plan(rows, cols, "line"), ([0, 1, 2, 3, 4], [0, 1], 0), "lines keep the totals")
        self.assertEqual(fl.chart_plan(rows, [], "bar")[1], [0], "no column dimensions: the one value column")
        self.assertEqual(fl.tuple_name(rows[3]), "Africa · Male")
        many = [((f"r{i}", 0, False, [f"r{i}"], False),) for i in range(20)]
        self.assertEqual(fl.chart_plan(many, cols, "line"), (list(range(fl.MAX_SERIES)), [0, 1], 20 - fl.MAX_SERIES))

    def test_runs_of_neighbours(self):
        self.assertEqual(fl.runs([1, 2, 3, 5, 7, 8]), [(1, 3), (5, 5), (7, 8)])
        self.assertEqual(fl.runs([]), [])

    def test_column_names(self):
        self.assertEqual([fl.column_name(c) for c in (0, 25, 26, 701, 702)], ["A", "Z", "AA", "ZZ", "AAA"])


if __name__ == "__main__":
    unittest.main()
