"""fluxolap_client against a real server (plain Python, no LibreOffice):

    python3 -m unittest discover clients/libreoffice/tests
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "extension", "pythonpath"))
import fluxolap_client as fc  # noqa: E402
from server import Fluxolap  # noqa: E402

WORLD_1950 = 2_493_093_673.0


def let_time_pass(s):
    """Every value `s` has cached goes stale, as if `FRESH_FOR` had passed:
    the cache ages by the clock, and waiting for real would take that long."""
    with s._lock:
        for cells in s._cells.values():
            for k, (v, read_at) in cells.items():
                cells[k] = (v, read_at - fc.FRESH_FOR - 0.1)


class ClientTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.flux = Fluxolap()

    @classmethod
    def tearDownClass(cls):
        cls.flux.stop()

    def server(self):
        s = fc.Server(self.flux.address)  # a fresh cache per test
        self.addCleanup(s.close)
        return s

    def test_one_cell_and_names_from_numbers(self):
        s = self.server()
        self.assertEqual(s.values("WorldPopulation", "Population", [("World", fc.element_name(1950.0), "Both sexes", "All ages")]), [WORLD_1950])
        self.assertEqual(s.values("NobelPrizes", "Laureates", [("Physics", "All years", "World", "All laureates", "Laureates")]), [230.0])
        self.assertEqual(s.info().split()[0], "fluxolap")

    def test_problems_are_readable(self):
        s = self.server()
        for args, text in [
            (("Nope", "Population", [("World",)]), "database 'Nope' not found"),
            (("WorldPopulation", "Nope", [("World",)]), "cube 'Nope' not found"),
            (("WorldPopulation", "Population", [("World", "1950")]), "has 4 dimensions, 2 elements given"),
            (("WorldPopulation", "Population", [("Atlantis", "1950", "Both sexes", "All ages")]), "'Atlantis' not found"),
        ]:
            with self.assertRaises(fc.FluxError) as e:
                s.values(*args)
            self.assertIn(text, str(e.exception))
        with self.assertRaises(fc.FluxError) as e:
            nowhere = fc.Server("localhost:1")
            self.addCleanup(nowhere.close)
            nowhere.info()
        self.assertIn("can't reach", str(e.exception))

    def test_a_recalculation_reads_everything_it_asked_for_in_one_request(self):
        s = self.server()
        paths = [(loc, str(y), "Both sexes", "All ages") for loc in ("World", "Africa", "Asia", "Europe") for y in range(1950, 2101, 10)]
        s.cube(s.database("WorldPopulation"), "Population")  # the name lookups, before counting
        before = self.flux.requests()
        first = [s.values("WorldPopulation", "Population", [p])[0] for p in paths]
        self.assertEqual(self.flux.requests() - before, len(paths), "a new sheet: cell by cell")
        before = self.flux.requests()
        self.assertEqual([s.values("WorldPopulation", "Population", [p])[0] for p in paths], first)
        self.assertEqual(self.flux.requests() - before, 0, "within one calculation: from the cache")
        let_time_pass(s)
        before = self.flux.requests()
        self.assertEqual([s.values("WorldPopulation", "Population", [p])[0] for p in paths], first)
        self.assertEqual(self.flux.requests() - before, 1, "the next calculation: everything in one batch")

    def test_a_bad_path_in_a_batch_spoils_only_its_own_cell(self):
        s = self.server()
        good, bad = ("World", "1950", "Both sexes", "All ages"), ("Atlantis", "1950", "Both sexes", "All ages")
        s.values("WorldPopulation", "Population", [good])
        with self.assertRaises(fc.FluxError):
            s.values("WorldPopulation", "Population", [bad])
        let_time_pass(s)
        # Both stale: one batch fails, the cells are read one by one.
        self.assertEqual(s.values("WorldPopulation", "Population", [good]), [WORLD_1950])

    def test_a_refused_path_doesnt_slow_down_the_others(self):
        s = self.server()
        good = [(loc, "1950", "Both sexes", "All ages") for loc in ("World", "Africa", "Asia", "Europe")]
        bad = ("Atlantis", "1950", "Both sexes", "All ages")
        for p in good:
            s.values("WorldPopulation", "Population", [p])
        with self.assertRaises(fc.FluxError):
            s.values("WorldPopulation", "Population", [bad])
        let_time_pass(s)
        before = self.flux.requests()
        self.assertEqual(s.values("WorldPopulation", "Population", [good[0]]), [WORLD_1950])
        self.assertEqual(self.flux.requests() - before, 1, "the known-refused path isn't in the batch")
        let_time_pass(s)
        before = self.flux.requests()
        with self.assertRaises(fc.FluxError):
            s.values("WorldPopulation", "Population", [bad])  # asked for itself: tried again
        self.assertEqual(self.flux.requests() - before, 2, "the batch fails at the refused path, the rest is read in one more")
        self.assertEqual(s.values("WorldPopulation", "Population", good[1:2])[0] > 0, True)

    def test_area_rows_and_columns(self):
        rows, cols, grid = self.server().area("WorldPopulation", "Population", [["World", "Africa"], ["1950", "2000", "2050"], ["Both sexes"], ["All ages"]])
        self.assertEqual((rows, cols), (["World", "Africa"], ["1950", "2000", "2050"]))
        self.assertEqual(grid[0][0], WORLD_1950)
        self.assertEqual(len(grid), 2)
        self.assertEqual(len(grid[1]), 3)
        _, _, single = self.server().area("WorldPopulation", "Population", [["World"], ["1950"], ["Both sexes"], ["All ages"]])
        self.assertEqual(single, [[WORLD_1950]])
        with self.assertRaises(fc.FluxError):
            self.server().area("WorldPopulation", "Population", [["World", "Asia"], ["1950", "2000"], ["Male", "Female"], ["All ages"]])

    def test_elements_in_hierarchy_order(self):
        s = self.server()
        self.assertEqual(s.elements("NobelPrizes", "Category"),
                         ["All prizes", "Natural sciences", "Physics", "Chemistry", "Physiology or Medicine", "Literature", "Peace", "Economic Sciences"])
        self.assertEqual(s.elements("NobelPrizes", "Category", "Natural sciences"), ["Physics", "Chemistry", "Physiology or Medicine"])

    def test_addresses(self):
        self.assertEqual(fc.split_url("localhost:7777"), ("http", "localhost", 7777))
        self.assertEqual(fc.split_url("https://olap.example.com/"), ("https", "olap.example.com", 443))
        self.assertEqual(fc.element_name(1950.0), "1950")
        self.assertEqual(fc.element_name(0.5), "0.5")
        self.assertEqual(fc.element_name(" World "), "World")


if __name__ == "__main__":
    unittest.main()
