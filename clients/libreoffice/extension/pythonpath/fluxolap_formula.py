"""Reading a FLUX.DATA formula back (for writing values): which cell of
which cube it shows. Pure text work, tested without LibreOffice.

Calc's API spells formulas in English with `;` between arguments and
names our function by its programmatic name, e.g.
`=ORG.FLUXOLAP.CALC.FLUXOLAPADDIN.DATA("localhost:7777";"Sales";A1;1950)`.
"""

import re

DATA = "ORG.FLUXOLAP.CALC.FLUXOLAPADDIN.DATA("

# A cell reference: optional sheet (`$Sheet2.`, `'My sheet'.`), then `$B$12`.
REF = re.compile(r"^(?:(?P<sheet>\$?(?:'(?:[^']|'')+'|[^.'\s;()]+))\.)?(?P<cell>\$?[A-Za-z]{1,3}\$?[0-9]+)$")
NUMBER = re.compile(r"^[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?$")


class NotWritable(Exception):
    """The formula isn't a plain FLUX.DATA whose arguments can be read back."""


def is_data(formula):
    return formula.lstrip().upper().startswith("=" + DATA)


def arguments(formula):
    """The raw arguments of a `=FLUX.DATA(...)` formula, split on top-level `;`."""
    f = formula.strip()
    if not is_data(f) or not f.endswith(")"):
        raise NotWritable("not a plain FLUX.DATA formula (it must be the whole formula)")
    body = f[1 + len(DATA):-1]
    args, depth, quoted, start, i = [], 0, False, 0, 0
    while i < len(body):
        ch = body[i]
        if quoted:
            if ch == '"':
                if i + 1 < len(body) and body[i + 1] == '"':
                    i += 1  # "" is a quote inside a string
                else:
                    quoted = False
        elif ch == '"':
            quoted = True
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth < 0:
                raise NotWritable("FLUX.DATA must be the whole formula")
        elif ch == ";" and depth == 0:
            args.append(body[start:i].strip())
            start = i + 1
        i += 1
    if quoted or depth != 0:
        raise NotWritable("unbalanced quotes or parentheses")
    args.append(body[start:].strip())
    return args


def classify(arg):
    """("text", s) | ("number", x) | ("ref", sheet or None, cell) for one argument."""
    if len(arg) >= 2 and arg[0] == '"' and arg[-1] == '"' and '"' not in arg[1:-1].replace('""', ""):
        return ("text", arg[1:-1].replace('""', '"'))
    if NUMBER.match(arg):
        return ("number", float(arg))
    m = REF.match(arg)
    if m:
        sheet = m.group("sheet")
        if sheet:
            sheet = sheet.lstrip("$")
            if sheet.startswith("'"):
                sheet = sheet[1:-1].replace("''", "'")
        return ("ref", sheet, m.group("cell").replace("$", ""))
    raise NotWritable(f"the argument {arg} is a calculation -- use text, a number or a cell reference")
