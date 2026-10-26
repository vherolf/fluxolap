"""Universe example: every confirmed exoplanet.

Source: NASA Exoplanet Archive, Planetary Systems Composite Parameters
table, https://exoplanetarchive.ipac.caltech.edu/ -- open data; the
archive asks users to include its acknowledgment (see manifest).

Two cubes:
- Discoveries: planets counted by discovery year, method, facility
  (grouped by ground/space) and size class (from the planet's radius:
  the Kepler mission's size bins).
- Catalog: one row per planet -- host star, method and facility as
  string cells, radius/mass/period/distance as numeric cells.
Standard library only.
"""

import collections
import csv
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from package import Package, download  # noqa: E402

COLUMNS = "pl_name,hostname,discoverymethod,disc_year,disc_facility,disc_locale,pl_rade,pl_bmasse,pl_orbper,sy_dist,sy_pnum"
URL = f"https://exoplanetarchive.ipac.caltech.edu/TAP/sync?query=select+{COLUMNS}+from+pscomppars&format=csv"
SIZES = [("Earth-size (< 1.25 R⊕)", 1.25), ("Super-Earth (1.25-2 R⊕)", 2), ("Neptune-size (2-6 R⊕)", 6), ("Jupiter-size (6-15 R⊕)", 15), ("Larger than Jupiter (> 15 R⊕)", float("inf"))]
UNKNOWN_SIZE = "Unknown size"
PROPERTIES = [  # (element, type, column)
    ("Host star", "string", "hostname"),
    ("Discovery method", "string", "discoverymethod"),
    ("Discovery facility", "string", "disc_facility"),
    ("Discovery year", "numeric", "disc_year"),
    ("Radius (Earth radii)", "numeric", "pl_rade"),
    ("Mass (Earth masses)", "numeric", "pl_bmasse"),
    ("Orbital period (days)", "numeric", "pl_orbper"),
    ("Distance (parsecs)", "numeric", "sy_dist"),
    ("Planets in system", "numeric", "sy_pnum"),
]


def size_class(radius):
    if not radius:
        return UNKNOWN_SIZE
    return next(name for name, upper in SIZES if float(radius) < upper)


def main():
    out = os.path.dirname(os.path.abspath(__file__))
    rows = list(csv.DictReader(open(download(URL, "exoplanets.csv"), encoding="utf-8")))
    for r in rows:
        r["disc_locale"] = r["disc_locale"].strip().capitalize() if r["disc_locale"].lower() in ("ground", "space") else r["disc_locale"]

    # A facility seen from more than one locale gets one element per locale,
    # so no planet is counted twice in the "All facilities" total.
    locales_of = collections.defaultdict(set)
    for r in rows:
        locales_of[r["disc_facility"]].add(r["disc_locale"])

    def facility(r):
        f = r["disc_facility"]
        return f if len(locales_of[f]) == 1 else f"{f} ({r['disc_locale']})"

    discoveries = collections.Counter((r["disc_year"], r["discoverymethod"], facility(r), size_class(r["pl_rade"])) for r in rows)
    catalog = {}
    for r in rows:
        for prop, kind, column in PROPERTIES:
            if r[column]:
                catalog[(r["pl_name"], prop)] = r[column] if kind == "string" else float(r[column])

    pkg = Package(out, "Exoplanets", 5, "Every confirmed exoplanet: discoveries by year, method, facility and size, plus a per-planet catalog.", [
        {"name": "Planetary Systems Composite Parameters", "by": "NASA Exoplanet Archive", "url": "https://exoplanetarchive.ipac.caltech.edu/", "license": "Open data, acknowledgment requested",
         "acknowledgment": "This research has made use of the NASA Exoplanet Archive, which is operated by the California Institute of Technology, under contract with the National Aeronautics and Space Administration under the Exoplanet Exploration Program."},
    ])

    years = sorted({int(r["disc_year"]) for r in rows})
    dim = [("All years", "numeric", "", 1)]
    dim += [(f"{d}s", "numeric", "All years", 1) for d in range(years[0] // 10 * 10, years[-1] + 1, 10)]
    dim += [(str(y), "numeric", f"{y // 10 * 10}s", 1) for y in range(years[0], years[-1] + 1)]
    pkg.dimension("Discovery year", dim)

    methods = collections.Counter(r["discoverymethod"] for r in rows)
    pkg.dimension("Method", [("All methods", "numeric", "", 1)] + [(m, "numeric", "All methods", 1) for m, _ in methods.most_common()])

    per_locale = collections.defaultdict(collections.Counter)
    for r in rows:
        per_locale[r["disc_locale"]][facility(r)] += 1
    locales = sorted(per_locale, key=lambda l: -sum(per_locale[l].values()))
    dim = [("All facilities", "numeric", "", 1)] + [(l, "numeric", "All facilities", 1) for l in locales]
    dim += [(f, "numeric", l, 1) for l in locales for f, _ in per_locale[l].most_common()]
    pkg.dimension("Facility", dim)

    pkg.dimension("Size", [("All sizes", "numeric", "", 1)] + [(s, "numeric", "All sizes", 1) for s, _ in SIZES] + [(UNKNOWN_SIZE, "numeric", "All sizes", 1)])
    pkg.dimension("Planet", [(r["pl_name"], "numeric", "", 1) for r in sorted(rows, key=lambda r: r["pl_name"])])
    pkg.dimension("Property", [(p, kind, "", 1) for p, kind, _ in PROPERTIES])

    pkg.cube("Discoveries", ["Discovery year", "Method", "Facility", "Size"], ((*k, v) for k, v in sorted(discoveries.items())))
    pkg.cube("Catalog", ["Planet", "Property"], ((*k, v) for k, v in sorted(catalog.items())))
    pkg.write()


if __name__ == "__main__":
    main()
