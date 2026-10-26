"""Science example: 100,000 collisions at CERN's LHC that produced two muons.

Source: CMS Collaboration via the CERN Open Data Portal, "Dimuon event
information derived from the Run2010B public Mu dataset" (Thomas McCauley,
2014), https://opendata.cern.ch/record/700, doi:10.7483/OPENDATA.CMS.CB8H.MFFA
-- CC0 (https://opendata.cern.ch/docs/terms-of-use).

Each event is a pair of muons with their invariant mass M (2-110 GeV).
Counting events by mass makes the particles they came from stand out as
peaks: J/psi, psi(2S), the Upsilons and the Z boson. The Mass dimension
bins M finely where those peaks are; a second root, "Known particles",
groups the bins around each one (the same bins, a second parent). Also:
the pair's charge, muon types (global/tracker), where in the detector
the muons went (from their pseudorapidity), and the run. Standard
library only.
"""

import collections
import csv
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from package import Package, download  # noqa: E402

URL = "https://opendata.cern.ch/record/700/files/MuRun2010B.csv"

# Mass ranges in units of 0.01 GeV: (name, from, to, bin width). Fine bins
# where the resonances are, coarse ones in between.
RANGES = [
    ("2-5 GeV", 200, 500, 5),
    ("5-15 GeV", 500, 1500, 10),
    ("15-60 GeV", 1500, 6000, 100),
    ("60-110 GeV", 6000, 11000, 100),
]

# Windows around known particles, on bin edges: (name, from, to), 0.01 GeV.
PARTICLES = [
    ("J/psi (3.097 GeV)", 300, 320),
    ("psi(2S) (3.686 GeV)", 360, 375),
    ("Upsilon(1S) (9.460 GeV)", 930, 960),
    ("Upsilon(2S) (10.023 GeV)", 990, 1020),
    ("Upsilon(3S) (10.355 GeV)", 1020, 1050),
    ("Z boson (91.19 GeV)", 8600, 9600),
]

# CMS's barrel covers |eta| < 1.2, the endcaps beyond.
BARREL = 1.2


def bin_name(lo, hi, width):
    if width < 100:
        return f"{lo / 100:.2f}-{hi / 100:.2f} GeV"
    return f"{lo // 100}-{hi // 100} GeV"


def mass_bin(m):
    """The (range, bin) of mass `m` in GeV."""
    c = int(m * 100)
    for name, lo, hi, width in RANGES:
        if lo <= c < hi:
            start = lo + (c - lo) // width * width
            return name, bin_name(start, start + width, width)
    sys.exit(f"mass {m} outside the ranges")


def main():
    out = os.path.dirname(os.path.abspath(__file__))
    path = download(URL, "cms-dimuon-run2010b.csv")

    cells = collections.Counter()
    runs = set()
    with open(path, newline="") as f:
        for r in csv.DictReader(f):
            r = {k.strip(): v for k, v in r.items()}  # the header has "px1 "
            _, mbin = mass_bin(float(r["M"]))
            charge = "Opposite sign" if r["Q1"] != r["Q2"] else ("Same sign ++" if r["Q1"] == "1" else "Same sign --")
            types = "".join(sorted(r["Type1"] + r["Type2"]))
            muons = {"GG": "Global + global", "GT": "Global + tracker"}.get(types)
            if muons is None:
                sys.exit(f"unexpected muon types {types}")
            barrel = [abs(float(r[f"eta{i}"])) < BARREL for i in (1, 2)]
            region = {2: "Barrel + barrel", 1: "Barrel + endcap", 0: "Endcap + endcap"}[sum(barrel)]
            runs.add(int(r["Run"]))
            cells[(mbin, charge, muons, region, r["Run"])] += 1

    pkg = Package(out, "ParticlePhysics", 6,
                  "100,000 LHC collisions with two muons, recorded by CMS at CERN in 2010: count them by mass and the J/psi, Upsilon and Z particles appear.", [
                      {"name": "Dimuon event information derived from the Run2010B public Mu dataset", "by": "CMS Collaboration (Thomas McCauley), CERN Open Data Portal",
                       "url": "https://opendata.cern.ch/record/700", "doi": "10.7483/OPENDATA.CMS.CB8H.MFFA", "license": "CC0"},
                  ])

    rows = [("All masses", "numeric", "", 1)]
    rows += [(name, "numeric", "All masses", 1) for name, *_ in RANGES]
    bins = []
    for name, lo, hi, width in RANGES:
        for start in range(lo, hi, width):
            bins.append((start, start + width, width))
            rows.append((bin_name(start, start + width, width), "numeric", name, 1))
    # The same bins again under the particle they belong to: a second parent.
    rows.append(("Known particles", "numeric", "", 1))
    for name, lo, hi in PARTICLES:
        rows.append((name, "numeric", "Known particles", 1))
        rows += [(bin_name(s, e, w), "numeric", name, 1) for s, e, w in bins if lo <= s and e <= hi]
    pkg.dimension("Mass", rows)

    pkg.dimension("Charge", [("All pairs", "numeric", "", 1), ("Opposite sign", "numeric", "All pairs", 1), ("Same sign", "numeric", "All pairs", 1),
                             ("Same sign ++", "numeric", "Same sign", 1), ("Same sign --", "numeric", "Same sign", 1)])
    pkg.dimension("Muon types", [("All types", "numeric", "", 1), ("Global + global", "numeric", "All types", 1), ("Global + tracker", "numeric", "All types", 1)])
    pkg.dimension("Detector region", [("Whole detector", "numeric", "", 1)]
                  + [(r, "numeric", "Whole detector", 1) for r in ("Barrel + barrel", "Barrel + endcap", "Endcap + endcap")])
    pkg.dimension("Run", [("All runs", "numeric", "", 1)] + [(str(r), "numeric", "All runs", 1) for r in sorted(runs)])

    pkg.cube("Events", ["Mass", "Charge", "Muon types", "Detector region", "Run"], ((*k, v) for k, v in sorted(cells.items())))
    pkg.write()


if __name__ == "__main__":
    main()
