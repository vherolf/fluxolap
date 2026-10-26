"""Social example: the world's population by country, sex and age, 1950-2100.

Source: United Nations, Department of Economic and Social Affairs,
Population Division (2024). World Population Prospects 2024, medium
variant, https://population.un.org/wpp/ -- CC BY 3.0 IGO.

Estimates to 2023, projections from 2024; every 5th year (1950, 1955,
..., 2100) keeps the extract small. Countries roll up through the UN's
geographic subregions and regions to World; ages roll up into children
(0-14), working age (15-64) and elderly (65+). Values are persons (the
source is in thousands). Population is a stock, not a flow, so the Year
dimension has no totals. Standard library only.
"""

import collections
import csv
import gzip
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from package import Package, download  # noqa: E402

URL = "https://population.un.org/wpp/assets/Excel%20Files/1_Indicator%20(Standard)/CSV_FILES/WPP2024_PopulationByAge5GroupSex_Medium.csv.gz"
YEARS = range(1950, 2101, 5)
GEOGRAPHY = {"Country/Area", "Subregion", "Geographic region", "World"}
# ParentIDs the file points at but doesn't contain: 1840 groups the top
# geographic regions (-> World, 900); 918 is the Northern America
# subregion, whose countries go straight under the Northern America
# region (905) of the same name.
PARENT_FIXES = {"1840": "900", "918": "905"}
AGE_GROUPS = [("Children (0-14)", 0, 14), ("Working age (15-64)", 15, 64), ("Elderly (65+)", 65, 999)]


def main():
    out = os.path.dirname(os.path.abspath(__file__))
    path = download(URL, "wpp_age.csv.gz")

    locations = {}  # LocID -> (name, type, ParentID)
    cells = {}
    ages = {}
    with gzip.open(path, "rt", encoding="utf-8-sig") as f:
        for r in csv.DictReader(f):
            if r["LocTypeName"] not in GEOGRAPHY:
                continue
            locations.setdefault(r["LocID"], (r["Location"], r["LocTypeName"], r["ParentID"]))
            if r["LocTypeName"] != "Country/Area" or int(r["Time"]) not in YEARS:
                continue
            ages[r["AgeGrp"]] = int(r["AgeGrpStart"])
            for sex, column in (("Male", "PopMale"), ("Female", "PopFemale")):
                persons = round(float(r[column]) * 1000)
                if persons:
                    cells[(r["Location"], r["Time"], sex, r["AgeGrp"])] = persons

    # Location tree: every country's geographic chain up to World.
    children = collections.defaultdict(list)
    for loc_id, (name, kind, parent) in locations.items():
        if kind == "World":
            continue
        parent = PARENT_FIXES.get(parent, parent)
        if parent not in locations:
            sys.exit(f"{name} ({kind}) has no geographic parent")
        children[parent].append(loc_id)
    world = next(i for i, v in locations.items() if v[1] == "World")
    rows = [("World", "numeric", "", 1)]

    def walk(loc_id):
        for child in sorted(children[loc_id], key=lambda c: (locations[c][1] == "Country/Area", locations[c][0])):
            rows.append((locations[child][0], "numeric", locations[loc_id][0], 1))
            walk(child)

    walk(world)
    names = [r[0] for r in rows]
    if len(names) != len(set(names)):
        sys.exit(f"duplicate location names: {[n for n, c in collections.Counter(names).items() if c > 1]}")

    pkg = Package(out, "WorldPopulation", 3, "Population of every country by sex and 5-year age group, 1950-2100 (UN estimates and medium-variant projections).", [
        {"name": "World Population Prospects 2024", "by": "United Nations, DESA, Population Division", "url": "https://population.un.org/wpp/", "license": "CC BY 3.0 IGO"},
    ])
    pkg.dimension("Location", rows)
    pkg.dimension("Year", [(str(y), "numeric", "", 1) for y in YEARS])
    pkg.dimension("Sex", [("Both sexes", "numeric", "", 1), ("Male", "numeric", "Both sexes", 1), ("Female", "numeric", "Both sexes", 1)])
    rows = [("All ages", "numeric", "", 1)] + [(g, "numeric", "All ages", 1) for g, _, _ in AGE_GROUPS]
    for age, start in sorted(ages.items(), key=lambda a: a[1]):
        group = next(g for g, lo, hi in AGE_GROUPS if lo <= start <= hi)
        rows.append((age, "numeric", group, 1))
    pkg.dimension("Age", rows)
    pkg.cube("Population", ["Location", "Year", "Sex", "Age"], ((*k, v) for k, v in sorted(cells.items())))
    pkg.write()


if __name__ == "__main__":
    main()
