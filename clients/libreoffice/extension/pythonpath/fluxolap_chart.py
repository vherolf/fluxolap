"""A pasted view's chart: a real Calc chart beside the view, kept in step
with it -- every time the view is written (a total opened or closed, a
filter, a pivot), the chart is pointed at the view's new cells. Like
fluxoscope's charts: the rows are the series, the columns the x axis;
line, bar or stacked bar; bar charts leave opened totals out.

The values are linked to the view's own cells, so the chart follows every
recalculation (write-back, Refresh values) by itself. The names are not:
a chart shows what a cell displays, and the view's headers display "▾ "
markers and hide repeated labels. So the series and category names
("Africa · Male") are written to a hidden sheet, two columns per chart
with the view's name on top.

The chart is placed one column right of the view and moves along as the
view grows or shrinks -- unless you moved it yourself: then it stays
where you put it. The view's definition remembers where the chart was
placed relative to the view (`chart.offset`).
"""

import uno

import fluxolap_layout as fl

HELPER = "fluxolap charts"
TEMPLATES = {"line": "Line", "bar": "Column", "stacked": "StackedColumn"}
SIZE = (16000, 9000)  # 1/100 mm
TOLERANCE = 10  # 1/100 mm: a chart this close to where it was placed hasn't been moved


def _shape(sheet, name):
    """The chart's shape on the sheet's draw page (it carries the position)."""
    page = sheet.getDrawPage()
    for i in range(page.getCount()):
        s = page.getByIndex(i)
        if s.supportsService("com.sun.star.drawing.OLE2Shape") and s.PersistName == name:
            return s
    return None


def _point(sheet, col, row):
    p = sheet.getCellByPosition(col, row).Position
    return p.X, p.Y


def moved_by_user(sheet, name, chart, top, left):
    """Has the chart been moved away from where it was placed? Asked
    before the view changes (a growing view moves the chart along)."""
    if not chart or "offset" not in chart or not sheet.getCharts().hasByName(name):
        return False
    shape = _shape(sheet, name)
    if shape is None:
        return False
    bx, by = _point(sheet, left, top)
    p = shape.Position
    dx, dy = chart["offset"]
    return abs(p.X - bx - dx) > TOLERANCE or abs(p.Y - by - dy) > TOLERANCE


# -- the hidden sheet with the names ---------------------------------------------------

def _helper(doc, create=True):
    sheets = doc.Sheets
    if not sheets.hasByName(HELPER):
        if not create:
            return None
        sheets.insertNewByName(HELPER, sheets.getCount())
        sheets.getByName(HELPER).IsVisible = False
    return sheets.getByName(HELPER)


def _block(helper, name, create=True):
    """The helper sheet's first column of the chart's two (row 0: the view's name)."""
    names = helper.getCellRangeByPosition(0, 0, 1999, 0).getDataArray()[0][::2]
    if name in names:
        return 2 * names.index(name)
    if not create:
        return None
    free = 2 * names.index("")
    helper.getCellByPosition(free, 0).setString(name)
    return free


def _clear(helper, col):
    end = helper.getRows().getCount() - 1
    helper.getCellRangeByPosition(col, 1, col + 1, end).clearContents(1 | 2 | 4 | 16)


def _range(sheet, c1, r1, c2, r2):
    return sheet.getCellRangeByPosition(c1, r1, c2, r2).AbsoluteName


# -- the chart ----------------------------------------------------------------------

def update(doc, sheet, name, chart, rows, cols, top, left, first, title, moved=False):
    """Points the view's chart at its cells (creates it if needed) and
    returns the chart's definition to save with the view. `rows`, `cols`:
    the view's entries; its values start at row `top + first`, column
    `left + len(rows[0])`."""
    kind = chart["type"]
    series, points, more = fl.chart_plan(rows, cols, kind)
    helper = _helper(doc)
    block = _block(helper, name)
    _clear(helper, block)
    for i, r in enumerate(series):
        helper.getCellByPosition(block, 1 + i).setString(fl.tuple_name(rows[r]))
    for i, c in enumerate(points):
        helper.getCellByPosition(block + 1, 1 + i).setString(fl.tuple_name(cols[c]) if cols else "value")

    first_row, first_col = top + first, left + len(rows[0])
    charts = sheet.getCharts()
    if not charts.hasByName(name):
        rect = uno.createUnoStruct("com.sun.star.awt.Rectangle")
        rect.Width, rect.Height = SIZE
        whole = sheet.getCellRangeByPosition(first_col, first_row, first_col, first_row).getRangeAddress()
        charts.addNewByName(name, rect, (whole,), False, False)
    doc_chart = charts.getByName(name).getEmbeddedObject()
    doc_chart.lockControllers()
    try:
        _fill(doc_chart, sheet, helper, block, kind, series, points, first_row, first_col)
        doc_chart.HasLegend = True
        doc_chart.HasMainTitle = True
        doc_chart.getTitle().String = title + (f" (the first {fl.MAX_SERIES} rows; {more} more)" if more else "")
    finally:
        doc_chart.unlockControllers()

    out = dict(chart)
    if not moved:
        shape = _shape(sheet, name)
        right = left + len(rows[0]) + (len(cols) if cols else 1) - 1
        x, y = _point(sheet, right + 2, top)
        position = uno.createUnoStruct("com.sun.star.awt.Point")
        position.X, position.Y = x, y
        shape.Anchor = sheet.getCellByPosition(right + 2, top)  # moves with the cells
        shape.Position = position
        bx, by = _point(sheet, left, top)
        out["offset"] = [x - bx, y - by]
    return out


def _fill(doc_chart, sheet, helper, block, kind, series, points, first_row, first_col):
    smgr = uno.getComponentContext().ServiceManager
    ctx = uno.getComponentContext()
    diagram = doc_chart.getFirstDiagram()
    provider = doc_chart.getDataProvider()

    def sequence(text, role):
        s = provider.createDataSequenceByRangeRepresentation(text)
        s.Role = role
        return s

    def labeled(values, label=None):
        lds = smgr.createInstanceWithContext("com.sun.star.chart2.data.LabeledDataSequence", ctx)
        lds.setValues(values)
        if label is not None:
            lds.setLabel(label)
        return lds

    column_runs = fl.runs(points)
    out = []
    for i, r in enumerate(series):
        row = first_row + r
        values = ",".join(_range(sheet, first_col + a, row, first_col + b, row) for a, b in column_runs)
        data = smgr.createInstanceWithContext("com.sun.star.chart2.DataSeries", ctx)
        data.setData((labeled(sequence(values, "values-y"), sequence(_range(helper, block, 1 + i, block, 1 + i), "label")),))
        out.append(data)
    system = diagram.getCoordinateSystems()[0]
    system.getChartTypes()[0].setDataSeries(tuple(out))
    axis = system.getAxisByDimension(0, 0)
    scale = axis.getScaleData()
    scale.Categories = labeled(sequence(_range(helper, block + 1, 1, block + 1, len(points)), "categories"))
    axis.setScaleData(scale)
    # The kind of chart last: a template also sets up the series (stacking, lines).
    template = doc_chart.getChartTypeManager().createInstance(f"com.sun.star.chart2.template.{TEMPLATES[kind]}")
    if not template.matchesTemplate(diagram, True):
        template.changeDiagram(diagram)


def remove(doc, sheet, name):
    """The view has no chart (any more): its chart and its names go."""
    if sheet.getCharts().hasByName(name):
        sheet.getCharts().removeByName(name)
    helper = _helper(doc, create=False)
    if helper is not None:
        block = _block(helper, name, create=False)
        if block is not None:
            helper.getCellRangeByPosition(block, 0, block + 1, helper.getRows().getCount() - 1).clearContents(1 | 2 | 4 | 16)
