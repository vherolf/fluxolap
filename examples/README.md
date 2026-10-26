# fluxolap examples

Eight ready-made databases built from real open data, one for each of commerce, science, society, the environment, the universe, particle physics, human genetics and metabolomics. They're here so you can explore what an OLAP cube does before you model your own data: drill down a hierarchy, read a whole area in one request, see weighted consolidation turn months into an annual mean.

## Start fluxolap with the examples

From a release download (the tarball contains `fluxolap` and this `examples/` directory):

```bash
FLUXOLAP_EXAMPLES=./examples ./fluxolap
```

With Docker (the image imports them by default):

```bash
docker run -p 7777:7777 ghcr.io/vherolf/fluxolap
```

From a source checkout:

```bash
cargo run --release -p xtask -- examples          # builds target/examples/*.bin.zst
FLUXOLAP_EXAMPLES=target/examples cargo run --release -p fluxolap-server
```

Then open <http://localhost:7777/api/v1/docs>, or use `curl` as below.

On startup each file in `FLUXOLAP_EXAMPLES` becomes a database, unless one with the same name already exists, so restarting is harmless. The directory is only read. With `FLUXOLAP_DATA_DIR` set, the imported databases are saved there like any other: change them as you like, and delete one to get a fresh copy on the next start.

## Finding your way

The queries below use the ids a fresh server assigns: databases 0–7 in the order of this page. If your server already had databases, look ids up by name:

```bash
API=http://localhost:7777/api/v1
curl "$API/databases?name=Climate"
curl "$API/databases/3/cubes?type=normal"          # the example cubes (hides the automatic attribute cubes)
curl "$API/databases/3/dimensions/2/elements?limit=20"
```

Cell queries take elements by name. In `POST .../cells/query`, `"2025"` is an element named 2025, while `2025` is element id 2025. Results list each cell's `path` as element ids, one per cube dimension, in the order given.

```bash
alias q='curl -s -H "content-type: application/json" -X POST'
```

---

## 1. Commerce: `OnlineRetail`

Two years of real sales from a UK online gift shop, December 2009 to December 2011: about a million invoice lines, reduced to monthly sums.

| Cube | Dimensions |
|---|---|
| `Sales` (id 5) | **Month** (All months → year → quarter → `2010-03`) · **Country** (All countries → region → country) · **Product** (All products → Merchandise / Services & adjustments → stock code; the top 1,000 products by revenue, the rest as `Other merchandise`) · **Measure** (`Revenue` in GBP, `Units`, `Invoice lines`) |

Revenue and units are net of cancellations. Each product's description is a string attribute in the automatic `#_Product_` cube.

```bash
# Revenue and units per year
q $API/databases/0/cubes/5/cells/query -d '{"area": [["2010", "2011"], ["All countries"], ["All products"], ["Revenue", "Units"]]}'
# -> 2010: 9,111,106.09 GBP, 5,187,533 units; 2011: 8,998,808.91 GBP, 4,834,223 units

# Revenue by region, all months
q $API/databases/0/cubes/5/cells/query -d '{"area": [["All months"], ["British Isles", "Continental Europe", "Americas", "Asia-Pacific", "Middle East & Africa"], ["All products"], ["Revenue"]]}'
# -> British Isles 16.7 M, Continental Europe 1.9 M, Asia-Pacific 237 k, ...

# What are products 22423 and 85123A? (attribute cube #_Product_, id 3)
q $API/databases/0/cubes/3/cells/query -d '{"paths": [["Description", "22423"], ["Description", "85123A"]]}'
# -> "REGENCY CAKESTAND 3 TIER", "WHITE HANGING HEART T-LIGHT HOLDER"
```

*Source: "Online Retail II", Daqing Chen, UCI Machine Learning Repository, [doi:10.24432/C5CG6D](https://doi.org/10.24432/C5CG6D). License: CC BY 4.0.*

## 2. Science: `NobelPrizes`

Every Nobel Prize since 1901: 1,018 laureates.

| Cube | Dimensions |
|---|---|
| `Laureates` (id 6) | **Category** (All prizes → Natural sciences → Physics / Chemistry / Physiology or Medicine; Literature; Peace; Economic Sciences) · **Year** (All years → decade → year) · **Birth country** (World → continent → country; founding country for organizations) · **Gender** (All laureates → Female / Male / Organization) · **Measure** (`Laureates`, `Prize share`, `Prize money (SEK, today)`) |

`Prize share` is each laureate's portion (1, ½, ⅓, ¼). A category's shares add up to one prize per year, so `Prize share` over All years counts the years the prize was awarded.

```bash
# Laureates per category, all years
q $API/databases/1/cubes/6/cells/query -d '{"area": [["Physics", "Chemistry", "Physiology or Medicine", "Literature", "Peace", "Economic Sciences"], ["All years"], ["World"], ["All laureates"], ["Laureates"]]}'
# -> 230, 200, 232, 122, 143, 99

# Women laureates by decade
q $API/databases/1/cubes/6/cells/query -d '{"area": [["All prizes"], ["1900s", "1950s", "2000s", "2010s", "2020s"], ["World"], ["Female"], ["Laureates"]]}'
# -> 1900s: 3, 1950s: none, 2000s: 11, 2010s: 13, 2020s: 14
```

*Source: [Nobel Prize API](https://api.nobelprize.org/), Nobel Prize Outreach. License: CC0.*

## 3. Social: `WorldPopulation`

The population of every country by sex and 5-year age group, 1950–2100: UN estimates up to 2023, medium-variant projections after that, every fifth year.

| Cube | Dimensions |
|---|---|
| `Population` (id 5) | **Location** (World → region → subregion → country, the UN's geographic hierarchy) · **Year** (1950, 1955, …, 2100) · **Sex** (Both sexes → Male / Female) · **Age** (All ages → Children 0–14 / Working age 15–64 / Elderly 65+ → `0-4` … `100+`) |

Values are persons. Population is a stock, so the Year dimension has no totals: adding up years would be meaningless.

```bash
# The world in 2025
q $API/databases/2/cubes/5/cells/query -d '{"paths": [["World", "2025", "Both sexes", "All ages"]]}'
# -> 8,231,613,565

# The world grows older: age groups in 1950, 2025, 2100
q $API/databases/2/cubes/5/cells/query -d '{"area": [["World"], ["1950", "2025", "2100"], ["Both sexes"], ["Children (0-14)", "Working age (15-64)", "Elderly (65+)"]]}'
# -> 1950: 0.87 bn / 1.50 bn / 0.13 bn;  2025: 2.01 / 5.37 / 0.86;  2100: 1.68 / 6.06 / 2.44
```

*Source: United Nations, Department of Economic and Social Affairs, Population Division (2024). [World Population Prospects 2024](https://population.un.org/wpp/). License: CC BY 3.0 IGO.*

## 4. Environmental: `Climate`

Where CO₂ comes from, and what it does to the atmosphere.

| Cube | Dimensions |
|---|---|
| `Emissions` (id 6) | **Country** (World → continent → country, plus international aviation and shipping) · **Year** (1850–2025; emissions data ends 2024) · **Source** (All sources → Coal / Oil / Gas / Cement / Flaring / Other industry / Not attributed). Values are Mt CO₂. |
| `Atmosphere` (id 7) | **Year** · **Month** (`Annual mean` → Jan … Dec, **each weighted 1/12**) · **Measure** (`CO2 (ppm)` at Mauna Loa, `Temperature anomaly (C)`: global surface temperature vs 1951–1980) |

`Annual mean` shows weighted consolidation: it sums the twelve months at weight 1/12 each, which is their average. Only complete years are included. A monthly anomaly of exactly 0.00 °C reads as empty, because fluxolap stores zero as "no value". The annual mean is unaffected.

```bash
# World emissions in 2024 by source
q $API/databases/3/cubes/6/cells/query -d '{"area": [["World"], ["2024"], ["Coal", "Oil", "Gas", "Cement", "Flaring", "Other industry", "All sources"]]}'
# -> coal 15,805, oil 12,471, gas 8,010, cement 1,473, flaring 416, other 424; all sources 38,599 Mt

# CO2 and temperature, annual means
q $API/databases/3/cubes/7/cells/query -d '{"area": [["1960", "1990", "2025"], ["Annual mean"], ["CO2 (ppm)", "Temperature anomaly (C)"]]}'
# -> 1960: 316.9 ppm, -0.03 C;  1990: 354.5 ppm, +0.45 C;  2025: 427.3 ppm, +1.19 C
```

*Sources: Global Carbon Budget (Global Carbon Project) via [Our World in Data](https://github.com/owid/co2-data), and OWID's continent classification. License: CC BY 4.0. Mauna Loa CO₂: [NOAA Global Monitoring Laboratory](https://gml.noaa.gov/ccgg/trends/), free public use with citation. Temperature: [GISTEMP v4](https://data.giss.nasa.gov/gistemp/), GISTEMP Team, NASA Goddard Institute for Space Studies, free use with citation.*

## 5. Universe: `Exoplanets`

Every confirmed planet outside our solar system: 6,372 of them.

| Cube | Dimensions |
|---|---|
| `Discoveries` (id 7) | **Discovery year** (All years → decade → year) · **Method** (Transit, Radial Velocity, Microlensing, Imaging, …) · **Facility** (All facilities → Space / Ground / Multiple Locales → Kepler, TESS, …) · **Size** (Earth-size, Super-Earth, Neptune-size, Jupiter-size, Larger than Jupiter, Unknown). Values are planets. |
| `Catalog` (id 8) | **Planet** (6,372) · **Property** (`Host star`, `Discovery method`, `Discovery facility` as string cells; year, radius, mass, orbital period, distance, planets in system as numbers) |

```bash
# How planets are found
q $API/databases/4/cubes/7/cells/query -d '{"area": [["All years"], ["Transit", "Radial Velocity", "Microlensing", "Imaging"], ["All facilities"], ["All sizes"]]}'
# -> 4,709 by transit, 1,202 by radial velocity, 292 by microlensing, 97 imaged

# Space telescopes took over
q $API/databases/4/cubes/7/cells/query -d '{"area": [["1990s", "2000s", "2010s", "2020s"], ["All methods"], ["Space", "Ground"], ["All sizes"]]}'
# -> 1990s: 0 / 30;  2000s: 19 / 356;  2010s: 2,790 / 893;  2020s: 1,535 / 713

# One planet's catalog entry
q $API/databases/4/cubes/8/cells/query -d '{"area": [["TRAPPIST-1 e"], ["Host star", "Discovery year", "Radius (Earth radii)", "Mass (Earth masses)", "Orbital period (days)", "Distance (parsecs)"]]}'
# -> "TRAPPIST-1", 2017, 0.92, 0.692, 6.10 days, 12.43 pc
```

*Source: [NASA Exoplanet Archive](https://exoplanetarchive.ipac.caltech.edu/), Planetary Systems Composite Parameters. "This research has made use of the NASA Exoplanet Archive, which is operated by the California Institute of Technology, under contract with the National Aeronautics and Space Administration under the Exoplanet Exploration Program."*

---

## 6. Physics: `ParticlePhysics`

100,000 proton–proton collisions at CERN's Large Hadron Collider that produced exactly two muons, recorded by the CMS detector in 2010. For each pair the data gives the *invariant mass*: the mass of whatever particle decayed into the two muons, if one did. Count the events by mass and the particles stand out as peaks above the background: the J/ψ at 3.1 GeV, the ψ(2S), the three ϒ states around 10 GeV and the Z boson at 91 GeV.

| Cube | Dimensions |
|---|---|
| `Events` (id 6) | **Mass** (All masses → `2-5 GeV` / `5-15 GeV` / `15-60 GeV` / `60-110 GeV` → bins of 0.05, 0.1 and 1 GeV, e.g. `3.05-3.10 GeV`; and a second root, **Known particles** → `J/psi (3.097 GeV)`, `psi(2S)`, `Upsilon(1S)`–`(3S)`, `Z boson (91.19 GeV)` → the same bins) · **Charge** (Opposite sign, Same sign → `++` / `--`) · **Muon types** (Global + global, Global + tracker) · **Detector region** (Barrel + barrel, Barrel + endcap, Endcap + endcap; the barrel is \|η\| < 1.2) · **Run** (37 runs). Values are events. |

The Known particles bins have two parents: a bin under `J/psi (3.097 GeV)` is the same element as under `2-5 GeV`, so both totals include it: a hierarchy with two parents does a physicist's "mass window".

```bash
# The particles, as opposite-sign (a muon and an antimuon) and same-sign pairs
q $API/databases/5/cubes/6/cells/query -d '{"area": [["J/psi (3.097 GeV)", "Upsilon(1S) (9.460 GeV)", "Z boson (91.19 GeV)"], ["Opposite sign", "Same sign"], ["All types"], ["Whole detector"], ["All runs"]]}'
# -> J/psi 9,223 / 884;  Upsilon(1S) 2,080 / 635;  Z 293 / 4 -- a Z always decays into a muon and an antimuon

# The J/psi peak, bin by bin
q $API/databases/5/cubes/6/cells/query -d '{"area": [["2.95-3.00 GeV", "3.00-3.05 GeV", "3.05-3.10 GeV", "3.10-3.15 GeV", "3.15-3.20 GeV", "3.20-3.25 GeV"], ["Opposite sign"], ["All types"], ["Whole detector"], ["All runs"]]}'
```

In fluxoscope, put Mass on the columns and Charge on the rows. Open `All masses`, `2-5 GeV` and `All pairs`, then pick *Stacked*: the J/ψ peak rises at 3.05–3.15 GeV, muon + antimuon pairs far above the same-sign background.

*Source: CMS Collaboration, [Dimuon event information derived from the Run2010B public Mu dataset](https://opendata.cern.ch/record/700) (Thomas McCauley, 2014), CERN Open Data Portal, [doi:10.7483/OPENDATA.CMS.CB8H.MFFA](https://doi.org/10.7483/OPENDATA.CMS.CB8H.MFFA). CC0. Selected for education; not suitable for a full physics analysis.*

---

## 7. Genetics: `GeneticVariants`

The human genetic variants in ClinVar, the public archive where clinical laboratories report what a variant means for health: 4.6 million variants in about 30,000 genes. Each is classified from *pathogenic* (causes disease) to *benign*, with a review status that says how much evidence stands behind it.

| Cube | Dimensions |
|---|---|
| `Variants` (id 7) | **Gene** (All genes → `Chromosome 1` … `Chromosome X`, `Chromosome Y`, `Mitochondrial DNA` → the 2,000 genes with the most variants, e.g. `BRCA1`, and `Other genes, chromosome 17` for the rest) · **Variant type** (Single nucleotide variants, Insertions and deletions → Deletion, Duplication, …, Copy number changes, Other variant types) · **Classification** (Pathogenic side → Pathogenic, Pathogenic/Likely pathogenic, Likely pathogenic · Uncertain significance · Benign side → Likely benign, Benign/Likely benign, Benign · Conflicting classifications · Other or not provided) · **Review status** (ClinVar's 0–4 stars, from `4 stars: practice guideline` to `0 stars: no assertion criteria`) · **Last evaluated** (Before 2010, 2010 … 2026, Not evaluated) · **Measure** (`Variants`, `Submitters`: the records laboratories submitted about them) |

Only variants on the current genome assembly (GRCh38) are counted.

```bash
# How much is known: most variants are still of uncertain significance
q $API/databases/6/cubes/7/cells/query -d '{"area": [["All genes"], ["All variant types"], ["Pathogenic side", "Uncertain significance", "Benign side", "Conflicting classifications"], ["All review levels"], ["All years"], ["Variants"]]}'
# -> 366,712 pathogenic side, 2,362,554 uncertain, 1,413,852 benign side, 178,232 conflicting

# The breast cancer gene BRCA1, by classification
q $API/databases/6/cubes/7/cells/query -d '{"area": [["BRCA1"], ["All variant types"], ["Pathogenic", "Likely pathogenic", "Uncertain significance", "Likely benign", "Benign"], ["All review levels"], ["All years"], ["Variants"]]}'
```

In fluxoscope, put Gene on the rows and Classification on the columns. Open a chromosome, turn on the heatmap, and the genes with many pathogenic variants stand out.

*Source: [ClinVar](https://www.ncbi.nlm.nih.gov/clinvar/), National Center for Biotechnology Information, U.S. National Library of Medicine; Landrum MJ et al., ClinVar: improving access to variant interpretations and supporting evidence, Nucleic Acids Res. 2018 ([PMID 29165669](https://pubmed.ncbi.nlm.nih.gov/29165669/)). NCBI places no restrictions on the use or distribution of the data. Not for diagnostic use.*

---

## 8. Metabolomics: `Metabolomics`

Bile acids in the blood plasma of 20 people with Parkinson's disease and 20 healthy people, measured as concentrations (ng/mL) by targeted mass spectrometry: 82 bile acids, 1,670 values (a bile acid not detected in a sample has none).

| Cube | Dimensions |
|---|---|
| `Bile acids` (id 4) | **Bile acid** (All bile acids → Sulfated or glucuronidated / Taurine-conjugated / Glycine-conjugated / Free (unconjugated) → 82 bile acids, e.g. `Taurochenodeoxycholic acid`) · **Participant** (All participants → `Parkinson's disease` / `Healthy controls` → `PD1` … `PD20`, `HC1` … `HC20`) · **Measure** (`Concentration (ng/mL)`, `Samples measured`) |

Concentrations add up, so the totals are meaningful, e.g. "total bile acids". Averages don't add up. That's why the second measure counts the samples a value was measured in, and a group's **mean** is `Concentration / Samples measured`, worked out by the client until fluxolap has rules.

```bash
# TCDCA, the bile acid the study links to Parkinson's severity: sum and number of samples per group
q $API/databases/7/cubes/4/cells/query -d '{"area": [["Taurochenodeoxycholic acid"], ["Parkinson'"'"'s disease", "Healthy controls"], ["Concentration (ng/mL)", "Samples measured"]]}'
# -> Parkinson's 1,679.3 / 20 = 84.0 ng/mL on average; healthy 850.6 / 20 = 42.5 -- about twice as much

# How the bile acids are conjugated, for each group
q $API/databases/7/cubes/4/cells/query -d '{"area": [["Sulfated or glucuronidated", "Taurine-conjugated", "Glycine-conjugated", "Free (unconjugated)"], ["Parkinson'"'"'s disease", "Healthy controls"], ["Concentration (ng/mL)"]]}'
```

*Source: Zhe Zhao et al., Peking University Third Hospital, [Targeted Bile Acid Metabolomic Profiling of Plasma Samples in Patients with Parkinson's Disease](https://www.ebi.ac.uk/metabolights/MTBLS15429), MetaboLights MTBLS15429 (2026). CC0 1.0. The grouping by conjugation is read from the bile acids' names.*

---

## How the examples are made

`examples/sources/<name>/` holds each example as a small *source package*: a `manifest.json` and gzipped CSV files for dimensions (element, type, parent, weight), cube cells and attributes. `crates/fluxolap-examples` turns packages into databases using the same engine calls the REST API uses, and `cargo run -p xtask -- examples` writes them as fluxolap database files. Releases and the Docker image build them fresh, so they always match the version's file format.

`examples/databases/` holds examples that exist only as a database, with no source data behind them, as fluxolap database files (`.bin.zst`). The build imports each one and saves it again with the others. So these ship in the release's file format too, and if a format change can no longer read one, the build fails instead of a user's server.

**Adding an example.** Open data you can fetch again: write a source package (see `examples/sources/package.py`). A database you already have: export it from a server that has it loaded, and put the file into `examples/databases/`. Use fluxoterm's `e` / `:w`, or `curl -o examples/databases/<Name>.bin.zst http://localhost:7777/api/v1/databases/<id>/export`. The file's name doesn't matter; the database keeps the name it has inside. Then `cargo test -p fluxolap-examples` (add a test with a few known totals).

To refresh a dataset from its source, run its `fetch.py` (Python 3, standard library only). It downloads the raw data into `~/.cache/fluxolap-examples`, reduces it and rewrites the package. `cargo test -p fluxolap-examples` checks the built databases against known totals.
