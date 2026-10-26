"""Science example: bile acids in the blood of people with Parkinson's
disease and of healthy people (metabolomics).

Source: MetaboLights study MTBLS15429, "Targeted Bile Acid Metabolomic
Profiling of Plasma Samples in Patients with Parkinson's Disease" (Zhe
Zhao et al., Peking University Third Hospital, 2026),
https://www.ebi.ac.uk/metabolights/MTBLS15429 -- CC0 1.0.

82 bile acids measured in the plasma of 20 patients and 20 healthy
controls, as concentrations in ng/mL; a bile acid that wasn't detected in
a sample has no value. Concentrations add up (a group of bile acids, all
of them: "total bile acids"), so the cube sums them like any measure. A
second measure counts the samples a value was measured in, so a client
gets a group's mean as Concentration / Samples measured -- the way to do
averages until fluxolap has rules.

Bile acids are grouped by how they are conjugated, read from their names:
sulfated or glucuronidated, taurine-conjugated (tauro-), glycine-conjugated
(glyco-), or free. Standard library only.
"""

import collections
import csv
import io
import os
import re
import sys
import urllib.request

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from package import Package, download  # noqa: E402

STUDY = "MTBLS15429"
FTP = f"https://ftp.ebi.ac.uk/pub/databases/metabolights/studies/public/{STUDY}"
MAF = f"m_{STUDY}_LC-MS_negative_reverse-phase_v2_maf.tsv"
SAMPLES = f"s_{STUDY}.txt"

GROUPS = [
    ("Sulfated or glucuronidated", lambda n: re.search(r"sulfate|glucuronide", n, re.I)),
    ("Taurine-conjugated", lambda n: re.match(r"tauro", n, re.I)),
    ("Glycine-conjugated", lambda n: re.match(r"glyco", n, re.I)),
    ("Free (unconjugated)", lambda n: True),
]
PARTICIPANTS = {"Parkinson's disease": "Parkinson's disease", "healthy control": "Healthy controls"}


def clean(name):
    """A bile acid's name without the salt it was measured as: "Taurocholic
    acid (sodium salt)" -> "Taurocholic acid"; first letter capital."""
    name = re.sub(r"\s*\(?\b(di|tri)?sodium( salt)?\)?\s*$", "", name.strip(), flags=re.I)
    name = re.sub(r"\s*\bsalt\b\s*$", "", name, flags=re.I).strip()
    return name[:1].upper() + name[1:]


def table(path):
    with open(path, encoding="utf-8", newline="") as f:
        return list(csv.reader(f, delimiter="\t"))


def main():
    out = os.path.dirname(os.path.abspath(__file__))
    maf = table(download(f"{FTP}/{MAF}", f"{STUDY}-maf.tsv"))
    samples = table(download(f"{FTP}/{SAMPLES}", f"{STUDY}-samples.txt"))

    # Which group each sample belongs to (the study's factor "Disease status").
    sh = samples[0]
    name_col = sh.index("Sample Name")
    group_col = sh.index("Factor Value[Disease status]")
    group_of = {r[name_col]: PARTICIPANTS[r[group_col]] for r in samples[1:] if r and r[name_col]}

    head = maf[0]
    first_sample = head.index("smallmolecule_abundance_std_error_sub") + 1
    sample_cols = [(i, head[i]) for i in range(first_sample, len(head)) if head[i] in group_of]
    if len(sample_cols) != len(group_of):
        sys.exit(f"samples without values or groups: {sorted(set(group_of) ^ {h for _, h in sample_cols})}")

    cells = {}
    acids = collections.OrderedDict()  # name -> group
    for row in maf[1:]:
        name = clean(row[head.index("metabolite_identification")])
        if not name:
            continue
        if name in acids:
            sys.exit(f"two rows for {name}")
        acids[name] = next(g for g, test in GROUPS if test(name))
        for i, sample in sample_cols:
            value = row[i].strip() if i < len(row) else ""
            if value:
                cells[(name, sample, "Concentration (ng/mL)")] = float(value)
                cells[(name, sample, "Samples measured")] = 1

    pkg = Package(out, "Metabolomics", 8,
                  "Bile acids in the blood of 20 people with Parkinson's disease and 20 healthy people (MetaboLights MTBLS15429), in ng/mL.", [
                      {"name": "Targeted Bile Acid Metabolomic Profiling of Plasma Samples in Patients with Parkinson's Disease",
                       "by": "Zhe Zhao et al., Peking University Third Hospital; MetaboLights", "url": "https://www.ebi.ac.uk/metabolights/MTBLS15429",
                       "license": "CC0 1.0"},
                  ])

    rows = [("All bile acids", "numeric", "", 1)] + [(g, "numeric", "All bile acids", 1) for g, _ in GROUPS]
    rows += [(name, "numeric", group, 1) for group, _ in GROUPS for name in sorted(n for n, g in acids.items() if g == group)]
    pkg.dimension("Bile acid", rows)

    def sample_key(s):
        m = re.match(r"([A-Za-z]+)(\d+)$", s)
        return (m.group(1), int(m.group(2))) if m else (s, 0)

    rows = [("All participants", "numeric", "", 1)]
    for group in PARTICIPANTS.values():
        rows.append((group, "numeric", "All participants", 1))
        rows += [(s, "numeric", group, 1) for s in sorted((s for s, g in group_of.items() if g == group), key=sample_key)]
    pkg.dimension("Participant", rows)
    pkg.dimension("Measure", [("Concentration (ng/mL)", "numeric", "", 1), ("Samples measured", "numeric", "", 1)])

    pkg.cube("Bile acids", ["Bile acid", "Participant", "Measure"], ((*k, v) for k, v in sorted(cells.items())))
    pkg.write()


if __name__ == "__main__":
    main()
