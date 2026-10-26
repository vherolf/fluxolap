"""Reading FLUX.DATA formulas back (fluxolap_formula), plain Python."""

import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "extension", "pythonpath"))
import fluxolap_formula as ff  # noqa: E402

DATA = "=ORG.FLUXOLAP.CALC.FLUXOLAPADDIN.DATA("


class FormulaTest(unittest.TestCase):
    def test_arguments_of_every_kind(self):
        f = DATA + '"localhost:7777";"Say ""hi""";$Sheet2.B$3;1950;\'My sheet\'.A1;C4;-2.5)'
        self.assertEqual([ff.classify(a) for a in ff.arguments(f)], [
            ("text", "localhost:7777"), ("text", 'Say "hi"'), ("ref", "Sheet2", "B3"), ("number", 1950.0),
            ("ref", "My sheet", "A1"), ("ref", None, "C4"), ("number", -2.5),
        ])

    def test_semicolons_and_parentheses_inside_strings(self):
        self.assertEqual(ff.arguments(DATA + '"a;b";"(c)")'), ['"a;b"', '"(c)"'])

    def test_what_cannot_be_written(self):
        for formula in ('=1+' + DATA[1:] + '"a";"b")', DATA + '"a";"b")*2', "=SUM(A1:A3)"):
            with self.assertRaises(ff.NotWritable, msg=formula):
                ff.arguments(formula)
        with self.assertRaises(ff.NotWritable):
            ff.classify('A1&"x"')
        with self.assertRaises(ff.NotWritable):
            ff.classify("SUM(A1:A2)")

    def test_recognizes_only_flux_data(self):
        self.assertTrue(ff.is_data(DATA + '"a")'))
        self.assertTrue(ff.is_data(DATA.lower() + '"a")'))
        self.assertFalse(ff.is_data("=ORG.FLUXOLAP.CALC.FLUXOLAPADDIN.AREA(\"a\")"))
        self.assertFalse(ff.is_data("5"))


if __name__ == "__main__":
    unittest.main()
