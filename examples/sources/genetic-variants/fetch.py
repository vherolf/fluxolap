"""Science example: the human genetic variants in ClinVar and what they mean
for health.

Source: ClinVar, National Center for Biotechnology Information (NCBI), U.S.
National Library of Medicine -- variant_summary.txt.gz,
https://ftp.ncbi.nlm.nih.gov/pub/clinvar/tab_delimited/. NCBI places no
restrictions on the use or distribution of the data; ClinVar asks for
attribution (Landrum et al., Nucleic Acids Res. 2018, PMID 29165669).
Not for diagnostic use.

One count per variant (GRCh38 records only; the file lists most variants
once per genome assembly): by gene (grouped by chromosome; the 2,000
genes with the most variants by name, the others as "Other genes" per
chromosome), variant type, classification (pathogenic ... benign, grouped),
review status (ClinVar's 0-4 stars) and the year it was last evaluated.
Measures: variants, and submitters (records submitted about them).
Standard library only; reads the 450 MB file as a stream.
"""

import collections
import csv
import gzip
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from package import Package, download  # noqa: E402

URL = "https://ftp.ncbi.nlm.nih.gov/pub/clinvar/tab_delimited/variant_summary.txt.gz"
TOP_GENES = 2000
FIRST_YEAR = 2010

CHROMOSOMES = [str(c) for c in range(1, 23)] + ["X", "Y", "MT"]

TYPES = {
    "single nucleotide variant": "Single nucleotide variants",
    "Deletion": "Insertions and deletions",
    "Duplication": "Insertions and deletions",
    "Insertion": "Insertions and deletions",
    "Indel": "Insertions and deletions",
    "copy number gain": "Copy number changes",
    "copy number loss": "Copy number changes",
}
TYPE_GROUPS = ["Single nucleotide variants", "Insertions and deletions", "Copy number changes", "Other variant types"]

# Classification: group -> classifications, most severe first.
CLASSES = [
    ("Pathogenic side", ["Pathogenic", "Pathogenic/Likely pathogenic", "Likely pathogenic"]),
    ("Uncertain significance", []),
    ("Benign side", ["Likely benign", "Benign/Likely benign", "Benign"]),
    ("Conflicting classifications", []),
    ("Other or not provided", []),
]

STARS = [
    ("4 stars: practice guideline", {"practice guideline"}),
    ("3 stars: reviewed by expert panel", {"reviewed by expert panel"}),
    ("2 stars: multiple submitters, no conflicts", {"criteria provided, multiple submitters, no conflicts"}),
    ("1 star: single submitter or conflicting", {"criteria provided, single submitter", "criteria provided, conflicting classifications"}),
]
NO_STARS = "0 stars: no assertion criteria"


def classification(raw):
    """The classification group leaf of ClinVar's ClinicalSignificance text."""
    s = raw.split(";")[0].split(",")[0].strip()  # "Pathogenic, low penetrance; other" -> "Pathogenic"
    if s.startswith("Likely pathogenic/"):
        s = "Likely pathogenic"
    for _, leaves in CLASSES:
        if s in leaves:
            return s
    if s.startswith(("Uncertain significance", "VUS")):
        return "Uncertain significance"
    if s.startswith("Conflicting classifications"):
        return "Conflicting classifications"
    return "Other or not provided"


def stars(raw):
    for name, statuses in STARS:
        if raw in statuses:
            return name
    return NO_STARS


def evaluated(raw):
    if raw == "-":
        return "Not evaluated"
    year = int(raw[-4:])
    return str(year) if year >= FIRST_YEAR else f"Before {FIRST_YEAR}"


def chromosome_name(c):
    return "Mitochondrial DNA" if c == "MT" else f"Chromosome {c}"


def main():
    out = os.path.dirname(os.path.abspath(__file__))
    path = download(URL, "clinvar-variant_summary.txt.gz")
    csv.field_size_limit(sys.maxsize)

    def records():
        with gzip.open(path, "rt", encoding="utf-8", newline="") as f:
            reader = csv.reader(f, delimiter="\t")
            ix = {h.lstrip("#"): i for i, h in enumerate(next(reader))}
            for row in reader:
                if row[ix["Assembly"]] == "GRCh38" and row[ix["Chromosome"]] in CHROMOSOMES:
                    yield (row[ix[k]] for k in ("GeneSymbol", "Chromosome", "Type", "ClinicalSignificance", "ReviewStatus", "LastEvaluated", "NumberSubmitters"))

    # Pass 1: each gene's variant count and its chromosome (where most of its variants are).
    per_gene = collections.Counter()
    gene_chrom = collections.defaultdict(collections.Counter)
    for gene, chrom, *_ in records():
        per_gene[gene] += 1
        gene_chrom[gene][chrom] += 1
    named = {g for g, _ in per_gene.most_common(TOP_GENES + 2) if g != "-" and ";" not in g}
    named = set(sorted(named, key=lambda g: -per_gene[g])[:TOP_GENES])
    home = {g: gene_chrom[g].most_common(1)[0][0] for g in named}

    # Pass 2: the cells.
    cells = collections.Counter()
    years = set()
    for gene, chrom, vtype, sig, review, last, submitters in records():
        g = gene if gene in named and home[gene] == chrom else f"Other genes, {chromosome_name(chrom).lower()}"
        year = evaluated(last)
        years.add(year)
        key = (g, TYPES.get(vtype, vtype), classification(sig), stars(review), year)
        cells[key + ("Variants",)] += 1
        cells[key + ("Submitters",)] += int(submitters) if submitters.isdigit() else 0

    pkg = Package(out, "GeneticVariants", 7,
                  "4.6 million human genetic variants from ClinVar (NCBI) by gene, type, clinical classification, review status and year evaluated.", [
                      {"name": "ClinVar variant_summary (GRCh38)", "by": "National Center for Biotechnology Information, U.S. National Library of Medicine",
                       "url": "https://www.ncbi.nlm.nih.gov/clinvar/", "license": "No restrictions on use or distribution (NCBI); attribution requested: Landrum et al. 2018, PMID 29165669"},
                  ])

    rows = [("All genes", "numeric", "", 1)]
    for c in CHROMOSOMES:
        rows.append((chromosome_name(c), "numeric", "All genes", 1))
        rows += [(g, "numeric", chromosome_name(c), 1) for g in sorted((g for g in named if home[g] == c), key=lambda g: -per_gene[g])]
        rows.append((f"Other genes, {chromosome_name(c).lower()}", "numeric", chromosome_name(c), 1))
    pkg.dimension("Gene", rows)

    other_types = sorted(t for t in {k[1] for k in cells} if t not in TYPE_GROUPS)
    rows = [("All variant types", "numeric", "", 1)] + [(t, "numeric", "All variant types", 1) for t in TYPE_GROUPS]
    rows += [(t, "numeric", "Other variant types", 1) for t in other_types]
    pkg.dimension("Variant type", rows)

    rows = [("All classifications", "numeric", "", 1)]
    for group, leaves in CLASSES:
        rows.append((group, "numeric", "All classifications", 1))
        rows += [(leaf, "numeric", group, 1) for leaf in leaves]
    pkg.dimension("Classification", rows)

    pkg.dimension("Review status", [("All review levels", "numeric", "", 1)] + [(name, "numeric", "All review levels", 1) for name, _ in STARS] + [(NO_STARS, "numeric", "All review levels", 1)])

    rows = [("All years", "numeric", "", 1), (f"Before {FIRST_YEAR}", "numeric", "All years", 1)]
    rows += [(y, "numeric", "All years", 1) for y in sorted(y for y in years if y.isdigit())]
    rows.append(("Not evaluated", "numeric", "All years", 1))
    pkg.dimension("Last evaluated", rows)

    pkg.dimension("Measure", [("Variants", "numeric", "", 1), ("Submitters", "numeric", "", 1)])

    pkg.cube("Variants", ["Gene", "Variant type", "Classification", "Review status", "Last evaluated", "Measure"], ((*k, v) for k, v in sorted(cells.items())))
    pkg.write()


if __name__ == "__main__":
    main()
