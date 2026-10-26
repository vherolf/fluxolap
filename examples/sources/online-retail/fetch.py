"""Commerce example: a real UK online gift retailer's sales, Dec 2009 - Dec 2011.

Source: "Online Retail II", Daqing Chen, UCI Machine Learning Repository,
https://doi.org/10.24432/C5CG6D -- CC BY 4.0.

Reduces ~1M invoice lines to monthly sums per country and product: the
1000 best-selling products by revenue, the rest as "Other merchandise",
and postage/fees/adjustments as "Services & adjustments". Revenue and
units are net of cancellations (cancelled lines carry negative
quantities in the source). Standard library only.
"""

import collections
import datetime
import os
import sys
import xml.etree.ElementTree as ET
import zipfile

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from package import Package, download  # noqa: E402

URL = "https://archive.ics.uci.edu/static/public/502/online+retail+ii.zip"
TOP_PRODUCTS = 1000
NS = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"
EXCEL_EPOCH = datetime.datetime(1899, 12, 30)
# Sheet 2 repeats sheet 1's last days (1-9 Dec 2010) row for row.
SHEET2_START = datetime.datetime(2010, 12, 1)

REGIONS = {
    "British Isles": ["United Kingdom", "EIRE", "Channel Islands", "Isle of Man"],
    "Continental Europe": [
        "Germany", "France", "Netherlands", "Belgium", "Switzerland", "Spain", "Portugal", "Italy", "Norway", "Finland",
        "Sweden", "Denmark", "Austria", "Poland", "Cyprus", "Iceland", "Greece", "Malta", "Lithuania", "Czech Republic",
        "European Community",
    ],
    "Americas": ["USA", "Canada", "Brazil", "West Indies", "Bermuda"],
    "Asia-Pacific": ["Australia", "Japan", "Hong Kong", "Singapore", "Korea", "Thailand"],
    "Middle East & Africa": ["Israel", "United Arab Emirates", "Lebanon", "Bahrain", "Saudi Arabia", "RSA", "Nigeria"],
    "Unknown": ["Unspecified"],
}
REGION_OF = {c: r for r, cs in REGIONS.items() for c in cs}

# Stock codes that aren't merchandise: postage, fees, discounts, adjustments.
SERVICES = {
    "POST": "Postage", "DOT": "Dotcom postage", "M": "Manual", "m": "Manual", "C2": "Carriage", "D": "Discount", "S": "Samples",
    "BANK CHARGES": "Bank charges", "AMAZONFEE": "Amazon fee", "CRUK": "CRUK commission", "ADJUST": "Adjust bad debt",
    "ADJUST2": "Adjust bad debt", "B": "Adjust bad debt", "TEST001": "Test", "TEST002": "Test",
}


def shared_strings(z):
    out = []
    for _, el in ET.iterparse(z.open("xl/sharedStrings.xml")):
        if el.tag == NS + "si":
            out.append("".join(t.text or "" for t in el.iter(NS + "t")))
            el.clear()
    return out


def sheet_rows(z, sheet, strings):
    for _, el in ET.iterparse(z.open(f"xl/worksheets/{sheet}.xml")):
        if el.tag != NS + "row":
            continue
        row = {}
        for c in el.iter(NS + "c"):
            col = c.get("r").rstrip("0123456789")
            v, t = c.find(NS + "v"), c.get("t")
            if t == "s":
                row[col] = strings[int(v.text)]
            elif t == "inlineStr":
                row[col] = "".join(x.text or "" for x in c.iter(NS + "t"))
            elif v is not None:
                row[col] = v.text
        el.clear()
        yield row


def stock_code(raw):
    code = str(raw).strip()
    return code[:-2] if code.endswith(".0") and code[:-2].isdigit() else code


def main():
    out = os.path.dirname(os.path.abspath(__file__))
    zip_path = download(URL, "retail.zip")
    with zipfile.ZipFile(zip_path) as outer:
        xlsx_path = os.path.join(os.path.dirname(zip_path), "online_retail_II.xlsx")
        if not os.path.exists(xlsx_path):
            outer.extract("online_retail_II.xlsx", os.path.dirname(zip_path))
    xlsx = zipfile.ZipFile(xlsx_path)
    strings = shared_strings(xlsx)

    # (month, country, code) -> [revenue, units, lines]
    sums = collections.defaultdict(lambda: [0.0, 0.0, 0])
    revenue_by_code = collections.Counter()
    descriptions = collections.defaultdict(collections.Counter)
    unknown = collections.Counter()
    for sheet in ("sheet1", "sheet2"):
        for i, r in enumerate(sheet_rows(xlsx, sheet, strings)):
            if i == 0:
                continue
            when = EXCEL_EPOCH + datetime.timedelta(days=float(r["E"]))
            if sheet == "sheet1" and when >= SHEET2_START:
                continue
            country = r["H"]
            if country not in REGION_OF:
                unknown[country] += 1
                continue
            code = stock_code(r["B"])
            qty, price = float(r["D"]), float(r.get("F", 0))
            s = sums[(when.strftime("%Y-%m"), country, code)]
            s[0] += qty * price
            s[1] += qty
            s[2] += 1
            revenue_by_code[code] += qty * price
            if r.get("C"):
                descriptions[code][r["C"].strip()] += 1
    if unknown:
        sys.exit(f"unmapped countries, add them to REGIONS: {dict(unknown)}")

    merchandise = [c for c, _ in revenue_by_code.most_common() if c not in SERVICES]
    top = merchandise[:TOP_PRODUCTS]
    top_set = set(top)

    def product(code):
        if code in SERVICES:
            return code
        return code if code in top_set else "Other merchandise"

    cells = collections.defaultdict(float)
    for (month, country, code), (revenue, units, lines) in sums.items():
        p = product(code)
        for measure, v in (("Revenue", revenue), ("Units", units), ("Invoice lines", lines)):
            cells[(month, country, p, measure)] += v

    pkg = Package(out, "OnlineRetail", 1, "Monthly sales of a UK online gift retailer, Dec 2009 - Dec 2011, by country and product.", [
        {"name": "Online Retail II", "by": "Daqing Chen", "url": "https://doi.org/10.24432/C5CG6D", "license": "CC BY 4.0"},
    ])

    months = sorted({k[0] for k in cells})
    rows = [("All months", "numeric", "", 1)]
    for year in sorted({m[:4] for m in months}):
        rows.append((year, "numeric", "All months", 1))
    for q in sorted({(m[:4], (int(m[5:]) - 1) // 3 + 1) for m in months}):
        rows.append((f"{q[0]} Q{q[1]}", "numeric", q[0], 1))
    for m in months:
        rows.append((m, "numeric", f"{m[:4]} Q{(int(m[5:]) - 1) // 3 + 1}", 1))
    pkg.dimension("Month", rows)

    rows = [("All countries", "numeric", "", 1)]
    rows += [(r, "numeric", "All countries", 1) for r in REGIONS]
    used = {k[1] for k in cells}
    rows += [(c, "numeric", r, 1) for r, cs in REGIONS.items() for c in cs if c in used]
    pkg.dimension("Country", rows)

    used_services = sorted({k[2] for k in cells if k[2] in SERVICES})
    rows = [("All products", "numeric", "", 1), ("Merchandise", "numeric", "All products", 1), ("Services & adjustments", "numeric", "All products", 1)]
    rows += [(c, "numeric", "Merchandise", 1) for c in top]
    rows.append(("Other merchandise", "numeric", "Merchandise", 1))
    rows += [(c, "numeric", "Services & adjustments", 1) for c in used_services]
    pkg.dimension("Product", rows)

    pkg.dimension("Measure", [(m, "numeric", "", 1) for m in ("Revenue", "Units", "Invoice lines")])

    pkg.cube("Sales", ["Month", "Country", "Product", "Measure"], ((*k, round(v, 2)) for k, v in sorted(cells.items()) if round(v, 2) != 0))

    desc = {c: descriptions[c].most_common(1)[0][0] for c in top if descriptions[c]}
    desc.update({c: SERVICES[c] for c in used_services})
    desc["Other merchandise"] = f"All products outside the top {TOP_PRODUCTS} by revenue"
    pkg.attribute("Product", "Description", sorted(desc.items()))
    pkg.write()


if __name__ == "__main__":
    main()
