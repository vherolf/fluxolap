"""Environmental example: CO2 emissions since 1850 and the changing atmosphere.

Sources:
- CO2 emissions by country and fuel: Global Carbon Budget (Global Carbon
  Project), via Our World in Data, https://github.com/owid/co2-data --
  CC BY 4.0. Continents: Our World in Data -- CC BY 4.0.
- Monthly CO2 at Mauna Loa: NOAA Global Monitoring Laboratory,
  https://gml.noaa.gov/ccgg/trends/ -- free public use with citation.
- Monthly global surface temperature anomaly (vs 1951-1980): NASA GISS
  Surface Temperature Analysis v4, https://data.giss.nasa.gov/gistemp/ --
  free use with citation.

Emissions: a cell per country, year and source. International aviation
and shipping, which no country is assigned, sit directly under World, so
World totals match the Global Carbon Budget. A "Not attributed" source
catches the rare gap between a country's total and its six sources.

Atmosphere: the month dimension's "Annual mean" consolidates the twelve
months with weight 1/12 each -- a weighted consolidation computing an
average. Only complete years are included. Standard library only.
"""

import csv
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from package import Package, download  # noqa: E402

CO2_URL = "https://raw.githubusercontent.com/owid/co2-data/master/owid-co2-data.csv"
CONTINENTS_URL = "https://ourworldindata.org/grapher/continents-according-to-our-world-in-data.csv?v=1&csvType=full&useColumnShortNames=true"
MLO_URL = "https://gml.noaa.gov/webdata/ccgg/trends/co2/co2_mm_mlo.csv"
GISTEMP_URL = "https://data.giss.nasa.gov/gistemp/tabledata_v4/GLB.Ts+dSST.csv"
FIRST_YEAR = 1850
SOURCES = [("Coal", "coal_co2"), ("Oil", "oil_co2"), ("Gas", "gas_co2"), ("Cement", "cement_co2"), ("Flaring", "flaring_co2"), ("Other industry", "other_industry_co2")]
CONTINENTS = ["Asia", "North America", "Europe", "Africa", "South America", "Oceania"]
# Emitters without an ISO code -> their group under World.
EXTRA = {"International aviation": "International transport", "International shipping": "International transport",
         "Kosovo": "Europe", "Kuwaiti Oil Fires (GCP)": "Asia", "Ryukyu Islands (GCP)": "Asia"}
MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]


def num(v):
    return float(v) if v not in ("", None) else 0.0


def main():
    out = os.path.dirname(os.path.abspath(__file__))
    continent_of = {r["code"]: r["owid_region"] for r in csv.DictReader(open(download(CONTINENTS_URL, "owid_continents.csv"), encoding="utf-8"))}

    emissions = {}
    group_of = {}
    last_year = FIRST_YEAR
    for r in csv.DictReader(open(download(CO2_URL, "owid_co2.csv"), encoding="utf-8")):
        year = int(r["year"])
        if year < FIRST_YEAR or not r["co2"]:
            continue
        name, iso = r["country"], r["iso_code"]
        if iso and not iso.startswith("OWID"):
            group = continent_of.get(iso, "World")  # Antarctica has no continent of its own here
        elif name in EXTRA:
            group = EXTRA[name]
        else:
            continue  # aggregates (World, continents, income groups, ...) -- fluxolap consolidates those itself
        group_of[name] = group
        last_year = max(last_year, year)
        attributed = 0.0
        for source, column in SOURCES:
            v = num(r[column])
            if v:
                emissions[(name, str(year), source)] = v
                attributed += v
        rest = num(r["co2"]) - attributed
        if abs(rest) >= 0.005:
            emissions[(name, str(year), "Not attributed")] = round(rest, 3)

    atmosphere = {}
    mlo = [l for l in open(download(MLO_URL, "co2_mm_mlo.csv"), encoding="utf-8") if not l.startswith("#")]
    by_year = {}
    for r in csv.DictReader(mlo):
        v = float(r["average"])
        if v > 0:
            by_year.setdefault(int(r["year"]), {})[int(r["month"])] = v
    for year, months in by_year.items():
        if len(months) == 12:
            for m, v in months.items():
                atmosphere[(str(year), MONTHS[m - 1], "CO2 (ppm)")] = v
    lines = open(download(GISTEMP_URL, "gistemp.csv"), encoding="utf-8").read().splitlines()[1:]
    for r in csv.DictReader(lines):
        values = [r[m] for m in MONTHS]
        if "***" in values:
            continue
        for m, v in zip(MONTHS, values):
            atmosphere[(r["Year"], m, "Temperature anomaly (C)")] = float(v)

    years = range(FIRST_YEAR, max(last_year, max(int(k[0]) for k in atmosphere)) + 1)
    pkg = Package(out, "Climate", 4, "CO2 emissions by country and source since 1850, and monthly CO2 concentration and global temperature anomaly.", [
        {"name": "Global Carbon Budget (via Our World in Data CO2 dataset)", "by": "Global Carbon Project; Our World in Data", "url": "https://github.com/owid/co2-data", "license": "CC BY 4.0"},
        {"name": "Continents according to Our World in Data", "by": "Our World in Data", "url": "https://ourworldindata.org/grapher/continents-according-to-our-world-in-data", "license": "CC BY 4.0"},
        {"name": "Mauna Loa CO2 monthly mean", "by": "NOAA Global Monitoring Laboratory (Lan, Tans, Thoning)", "url": "https://gml.noaa.gov/ccgg/trends/", "license": "Free public use, citation requested"},
        {"name": "GISS Surface Temperature Analysis (GISTEMP) v4", "by": "GISTEMP Team, NASA Goddard Institute for Space Studies", "url": "https://data.giss.nasa.gov/gistemp/", "license": "Free use, citation requested"},
    ])

    groups = [c for c in CONTINENTS if c in group_of.values()] + ["International transport"]
    rows = [("World", "numeric", "", 1)] + [(g, "numeric", "World", 1) for g in groups]
    rows += [(name, "numeric", group, 1) for group in groups + ["World"] for name in sorted(n for n, g in group_of.items() if g == group)]
    pkg.dimension("Country", rows)
    pkg.dimension("Year", [(str(y), "numeric", "", 1) for y in years])
    pkg.dimension("Source", [("All sources", "numeric", "", 1)] + [(s, "numeric", "All sources", 1) for s, _ in SOURCES] + [("Not attributed", "numeric", "All sources", 1)])
    pkg.dimension("Month", [("Annual mean", "numeric", "", 1)] + [(m, "numeric", "Annual mean", 1 / 12) for m in MONTHS])
    pkg.dimension("Measure", [(m, "numeric", "", 1) for m in ("CO2 (ppm)", "Temperature anomaly (C)")])

    pkg.cube("Emissions", ["Country", "Year", "Source"], ((*k, v) for k, v in sorted(emissions.items())))
    pkg.cube("Atmosphere", ["Year", "Month", "Measure"], ((*k, v) for k, v in sorted(atmosphere.items())))
    pkg.write()


if __name__ == "__main__":
    main()
