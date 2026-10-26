# fluxolap for LibreOffice Calc

A LibreOffice extension (`fluxolap.oxt`) that reads fluxolap cubes into spreadsheets with formulas. It talks to the server over the REST API, like fluxoterm and fluxoscope. It's written in Python with the standard library only, so the same `.oxt` works on Linux, Windows and macOS.

## Install

- **From a release:** `fluxolap.oxt` is in the release tarball and is also its own download.
- **From the repo:** `python3 clients/libreoffice/build.py` writes `target/fluxolap.oxt`.

Then, in LibreOffice: **Tools → Extension Manager → Add…**, pick the file, and restart LibreOffice. From a terminal: `unopkg add fluxolap.oxt`.

On Linux, LibreOffice's Python support is a separate package: `libreoffice-script-provider-python` on Debian/Ubuntu. Windows and macOS include it.

> **A Python virtualenv breaks it.** If you start LibreOffice or `unopkg` from a terminal where a virtualenv is active (`VIRTUAL_ENV` is set), LibreOffice's built-in Python picks up the virtualenv. Installing then fails with `std::bad_alloc`, and the functions don't load. Start it from the desktop, or `deactivate` first.

## The fluxolap menu

Calc gets a **fluxolap** menu:

| Item | What it does |
|---|---|
| **Paste view…** | Builds a report at the selected cell. Pick a database and a cube, the dimensions for the rows and for the columns (one or more each, like a pivot table), and what each shows: an element's children (with or without the total), or all elements. Pick an element for every other dimension (the filters). |
| **Refresh values** | Reads every value from the server again, right away (Ctrl+Shift+F9 does too, but only for values older than 10 seconds, see [Speed](#speed-why-a-sheet-with-thousands-of-formulas-stays-fast)). |
| **Connect…** | The server for this document (saved in it; the default is `localhost:7777`), with a connection test. |
| **Load database…** | Loads one of the server's example databases, all of them, or a database file (`.bin.zst`) from your computer. |

The same commands are in the **sidebar**: open the fluxolap deck (the bar-chart icon at the right edge of Calc). It shows:
- the document's server, and whether it answers;
- the four commands as buttons;
- the document's views: pick one to go to it, or **Change view…** to edit it.

A pasted view looks like this. Everything in it is ordinary cells and formulas, so you can format it, add your own calculations, or move it:

```
Population
Server      localhost:7777           <- the formulas refer to these cells:
Database    WorldPopulation             change "Both sexes" to "Female"
Cube        Population                  and the whole table shows women
Sex         Both sexes
Age         All ages

Location / Year   1950            1955            …
World             2,493,093,673   2,740,214,554
  Africa            227,776,582     253,538,021
```

**Several dimensions on an axis (pivot).** Each axis in the dialog has a list of dimensions: **Add** puts one there, **Remove** takes it back to the filters, **Move up** changes the nesting. The first dimension is the outer one: with Location and Sex on the rows, every location gets a row for each sex. Every row dimension has its own header column (every column dimension its own header row); a label that repeats the one above it is hidden. Double-click opens and closes a total in any of them: opening Africa in the outer dimension adds its regions, each again with every sex.

```
Location   Sex / Year    1950            1955
▾ World    ▾ Both sexes  2,493,093,673   2,740,214,554
             Male        …               …
             Female      …               …
▸ Africa   ▾ Both sexes    227,776,582     253,538,021
```

Choose **Formulas** (`FLUX.DATA` per cell, the default) to type values into the view and write them back, or **One block** (`FLUX.AREA`) for a big read-only table in a single request (one dimension on the rows and at most one on the columns). **Hide empty rows and columns** leaves out the ones without any value. The pasted view's values are read in one request, so it appears at once.

### Navigating a view: double-click

| Double-click on … | What happens |
|---|---|
| a **total** in the row or column headers (`▸ Africa`) | It opens: its children appear below (or to the right of) it, and it shows `▾`. Double-click again to close it, together with everything opened under it. |
| a **filter** (the blue cells) | An element picker, with the hierarchy and a search field. The view shows the element you pick. You can also just type another element into the cell. |
| the **title** | *Paste view* opens, filled in with this view, to change it in place: other dimensions, filters or options. An axis set to "(as it is)" keeps what's open. |

The `▸`/`▾` markers are only number formatting: the cells hold the plain element names, which the formulas use.

A view describes itself in the document, so saved views can still be navigated after reopening:
- a named range `fluxolap_view_<n>` covers it, and moves along when you insert rows above it;
- a document property of the same name holds its definition.

There can be several views on a sheet, anywhere. A view never writes over other cells: if opening a total would make it grow into cells that aren't empty, it refuses and says which cells are in the way. The look comes from cell styles (`fluxolap Title`, `fluxolap Filter`, `fluxolap Element Total`, `fluxolap Value`, …), so change a style to restyle every view.

### A view's chart

Pick **Chart** in Paste view (Line, Bars or Stacked bars; double-click the title to add one to an existing view). You get a real Calc chart right of the view, which stays in step with it:
- **Like fluxoscope's charts.** The rows are the series and the columns the x axis. A series is named by all its row dimensions ("Africa · Male").
- **Totals.** Bars add up, so in bar charts an opened total is left out, since its children are already there. Lines keep it.
- **Values.** They are linked to the view's cells, so the chart follows write-back, **Refresh values** and a new filter by itself.
- **Navigating.** Opening or closing a total, or changing the view, points the chart at the new cells.
- **Place.** The chart moves along when the view grows or shrinks. If you move it yourself, it stays where you put it.
- **Limits.** At most 12 series are shown (the title says how many more there are).

A chart shows what its label cells display, and the view's headers display markers and hide repeated labels. So the series and category names are written to a hidden sheet, `fluxolap charts`. Leave it alone; choose **No chart** to remove a chart along with its names. Otherwise it's an ordinary chart: style it, copy it into Writer, export it.

## Functions

Every function's first argument is the server, e.g. `"localhost:7777"` or `"https://olap.example.com"`. Databases, cubes, dimensions and elements are given **by name**. A number such as `1950` becomes the element name `"1950"`.

| Function | Result |
|---|---|
| `=FLUX.DATA(server; database; cube; element; element; …)` | One cell, with one element per cube dimension in the cube's order. A consolidated element gives the total. An empty cell gives 0. |
| `=FLUX.AREA(server; database; cube; elements; elements; …)` | A block, as an array formula (Ctrl+Shift+Enter, or select the range first). Give each dimension one element or a **range** of element names. The first dimension with several elements becomes the rows, the second the columns. One request for the whole block. |
| `=FLUX.ELEMENTS(server; database; dimension; [parent])` | A dimension's element names as a column (array formula): all of them in hierarchy order, or the children of `parent`. |
| `=FLUX.SERVER(server)` | `fluxolap 22.0.0`, or why the server can't be reached. |

Problems show in the cell as text, e.g. `#FLUX: element 'Atlantis' not found in dimension 'Location'`.

With the example databases (`./start.sh`):

```
A1  =FLUX.DATA("localhost:7777"; "WorldPopulation"; "Population"; "World"; 1950; "Both sexes"; "All ages")
    -> 2493093673

B2:B4  World, Africa, Asia          C1:E1  1950, 2000, 2050
C2:E4  {=FLUX.AREA("localhost:7777"; "WorldPopulation"; "Population"; B2:B4; C1:E1; "Both sexes"; "All ages")}

G1:G8  {=FLUX.ELEMENTS("localhost:7777"; "NobelPrizes"; "Category")}
```

## Writing values back

Type a value over a `FLUX.DATA` cell, and it goes to the server:
- **The formula stays** and shows the new value. The whole sheet recalculates, so totals and other formulas show the change too.
- **A total splashes:** a value typed over a consolidated cell is spread over its base cells, as in fluxoterm and the API.
- **Numbers and text:** numbers are written as numbers, text as text (for string cells).
- **Delete** only brings the formula back. Nothing is cleared on the server, so to clear a cell, type 0.
- **Paste** works the same way: of the pasted cells, only the `FLUX.DATA` ones are written. The others keep what was pasted.
- **Arguments:** for write-back, the formula's arguments must be text, numbers or cell references (also on other sheets, like `$Sheet2.B3`). If an argument is a calculation (e.g. `A1&"x"`), the value isn't written, the formula comes back, and a message explains why.

This only works for `FLUX.DATA`, not for the cells of a `FLUX.AREA` block: Calc doesn't let you type into part of an array formula.

How it works: right after you type, the extension undoes your input for a moment to see what was in the cell. If it was a `FLUX.DATA` formula, it writes your value and keeps the formula. If not, it redoes your input, as if nothing happened.

## Speed: why a sheet with thousands of formulas stays fast

Calc evaluates one formula at a time. The extension keeps that from turning into one request per cell:
- Values are cached for 10 seconds (`FRESH_FOR` in `fluxolap_client.py`), which covers one calculation, even a slow one that reads many consolidated values. The cache is the extension's own, inside LibreOffice; the server keeps nothing for it.
- Each cube remembers the cells a sheet asked for. The next calculation reads all of them in **one** request: 930 `FLUX.DATA` formulas recalculate in 0.08 s.

The first calculation of a new sheet is batched too. The first few formulas are answered right away, so a single edit stays instant. When many come in quick succession, they briefly show `…`, and right after the calculation they're all read in one request and the sheet calculates again: 186 new formulas take 6 requests instead of 186. `FLUX.AREA` is always one request.

The values don't refresh on their own. **Refresh values** (fluxolap menu or sidebar) reads the server's current values right away. **Ctrl+Shift+F9** (Data → Calculate → Recalculate Hard) does the same for values read more than 10 seconds ago; within those 10 seconds it answers from the cache, and a change on the server shows only with a later recalculation. Your own write-back is never stale: writing a cell makes every cached value of its cube stale.

## Development

- `extension/fluxolap_addin.py` is the Calc add-in (the four functions) and the job that starts write-back in every spreadsheet (`Jobs.xcu`, on "OnViewCreated").
- `extension/Addons.xcu` is the fluxolap menu. Its items call the Commands service in `fluxolap_addin.py`.
- `extension/Sidebar.xcu` and `Factories.xcu` are the sidebar deck and its panel (`pythonpath/fluxolap_sidebar.py`).
- `icons/deck.png` is drawn by `make_icon.py`.
- `extension/pythonpath/`:
  - `fluxolap_commands.py` holds the menu's commands and dialogs.
  - `fluxolap_views.py` handles pasted views: rendering them in place, finding them, and double-click navigation.
  - `fluxolap_layout.py` decides where a pasted view's labels and formulas go, and what its chart shows, with no LibreOffice code.
  - `fluxolap_chart.py` keeps a view's chart in step with it.
  - `fluxolap_client.py` holds the API access, caching and batching, with no LibreOffice code.
  - `fluxolap_writeback.py` handles typing over `FLUX.DATA` cells.
  - `fluxolap_formula.py` reads a `FLUX.DATA` formula's arguments back.
- `extension/CalcAddIns.xcu` gives the functions' names and help texts in the function wizard. `idl/fluxolap.idl` is their interface.
- `fluxolap.rdb` is the compiled interface and is committed. Only after changing `fluxolap.idl` run `python3 build.py --idl`, which needs the LibreOffice SDK (`libreoffice-dev`).
- Tests start their own fluxolap with the examples. They need `cargo build --release -p fluxolap-server` and `cargo run --release -p xtask -- examples` first. The Calc tests drive headless LibreOffice and need the Python that has `uno`:

  ```bash
  /usr/bin/python3 -m unittest discover clients/libreoffice/tests
  ```
