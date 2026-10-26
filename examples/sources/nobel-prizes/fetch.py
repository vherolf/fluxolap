"""Science example: every Nobel Prize since 1901.

Source: Nobel Prize API v2.1, https://api.nobelprize.org/ -- CC0
(https://www.nobelprize.org/about/terms-of-use-for-api-nobelprize-org-and-data-nobelprize-org/).

One cell per laureate and prize: category x award year x birth country
(founding country for organizations) x gender. Measures: laureates,
their share of the prize (1, 1/2, 1/3, 1/4 -- a category-year's shares
sum to 1), and the prize money that share is worth in today's SEK
(the API's inflation-adjusted amount). Standard library only.
"""

import collections
import json
import os
import sys
from fractions import Fraction

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from package import Package, download  # noqa: E402

URL = "https://api.nobelprize.org/2.1/laureates?limit=5000"
SCIENCES = ["Physics", "Chemistry", "Physiology or Medicine"]
OTHER = ["Literature", "Peace", "Economic Sciences"]
CONTINENTS = ["Europe", "North America", "Asia", "Africa", "Oceania", "South America"]


def main():
    out = os.path.dirname(os.path.abspath(__file__))
    laureates = json.load(open(download(URL, "laureates.json"), encoding="utf-8"))["laureates"]

    cells = collections.defaultdict(float)
    countries = collections.defaultdict(set)
    years = set()
    for person in laureates:
        origin = (person.get("birth") or person.get("founded") or {}).get("place", {})
        country = origin.get("countryNow", {}).get("en") or "Unknown country"
        continent = origin.get("continent", {}).get("en") or "Unknown"
        countries[continent].add(country)
        gender = {"female": "Female", "male": "Male"}.get(person.get("gender"), "Organization")
        for prize in person.get("nobelPrizes", []):
            category, year = prize["category"]["en"], prize["awardYear"]
            if category not in SCIENCES + OTHER:
                sys.exit(f"unknown category {category!r}")
            years.add(int(year))
            share = float(Fraction(prize["portion"]))
            key = (category, year, country, gender)
            cells[key + ("Laureates",)] += 1
            cells[key + ("Prize share",)] += share
            cells[key + ("Prize money (SEK, today)",)] += round(share * prize["prizeAmountAdjusted"])

    pkg = Package(out, "NobelPrizes", 2, "Every Nobel Prize and laureate since 1901, by category, year, birth country and gender.", [
        {"name": "Nobel Prize API v2.1", "by": "Nobel Prize Outreach", "url": "https://api.nobelprize.org/", "license": "CC0"},
    ])

    pkg.dimension("Category", [("All prizes", "numeric", "", 1), ("Natural sciences", "numeric", "All prizes", 1)]
                  + [(c, "numeric", "Natural sciences", 1) for c in SCIENCES]
                  + [(c, "numeric", "All prizes", 1) for c in OTHER])

    first, last = min(years), max(years)
    rows = [("All years", "numeric", "", 1)]
    decades = range(first // 10 * 10, last + 1, 10)
    rows += [(f"{d}s", "numeric", "All years", 1) for d in decades]
    rows += [(str(y), "numeric", f"{y // 10 * 10}s", 1) for y in range(first, last + 1)]
    pkg.dimension("Year", rows)

    order = [c for c in CONTINENTS if c in countries] + sorted(c for c in countries if c not in CONTINENTS)
    rows = [("World", "numeric", "", 1)] + [(c, "numeric", "World", 1) for c in order]
    rows += [(country, "numeric", c, 1) for c in order for country in sorted(countries[c])]
    pkg.dimension("Birth country", rows)

    pkg.dimension("Gender", [("All laureates", "numeric", "", 1)] + [(g, "numeric", "All laureates", 1) for g in ("Female", "Male", "Organization")])
    pkg.dimension("Measure", [(m, "numeric", "", 1) for m in ("Laureates", "Prize share", "Prize money (SEK, today)")])

    pkg.cube("Laureates", ["Category", "Year", "Birth country", "Gender", "Measure"], ((*k, v) for k, v in sorted(cells.items())))
    pkg.write()


if __name__ == "__main__":
    main()
