//! Builds the shipped examples from `examples/sources`, imports the built
//! files the way the server does (`AppState::import_database_file`), and
//! checks values against the sources. The expected numbers come from the
//! committed source extracts: refreshing a dataset (`fetch.py`) can
//! change them.

use fluxolap_engine::{ops, AppState};
use fluxolap_storage::CellValue;
use std::sync::OnceLock;

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Built once, imported into one fresh server state, shared by every test.
fn state() -> &'static AppState {
    static STATE: OnceLock<AppState> = OnceLock::new();
    STATE.get_or_init(|| {
        let out = std::env::temp_dir().join(format!("fluxolap-examples-test-{}", std::process::id()));
        let files = fluxolap_examples::build_all(&fluxolap_examples::default_sources_dir(), &out).expect("examples build");
        assert_eq!(files.len(), 8, "{files:?}");
        let state = AppState::new();
        for f in &files {
            let size = std::fs::metadata(f).unwrap().len();
            assert!(size <= MAX_FILE_BYTES, "{} is {size} bytes", f.display());
            assert!(state.import_database_file(f).unwrap().is_some(), "{} imported", f.display());
        }
        assert!(state.import_database_file(&files[0]).unwrap().is_none(), "importing again skips an existing name");
        let _ = std::fs::remove_dir_all(&out);
        state
    })
}

/// Reads one cell of `database`.`cube`, the path given as element names.
fn cell(database: &str, cube: &str, path: &[&str]) -> Option<CellValue> {
    let state = state();
    let databases = state.databases.read().unwrap();
    let db = databases.find_by_name(database).unwrap_or_else(|| panic!("database {database}"));
    let loaded = state.resolve_loaded(&databases, db).unwrap();
    let dimensions = loaded.dimensions.read().unwrap();
    let cubes = loaded.cubes.read().unwrap();
    let cube_id = cubes.find_by_name(db, cube).unwrap_or_else(|| panic!("cube {cube}"));
    let cube = cubes.get(cube_id).unwrap();
    let ids: Vec<_> = path
        .iter()
        .zip(&cube.dimensions)
        .map(|(name, &d)| dimensions.get(d).unwrap().find_element_by_name(name).unwrap_or_else(|| panic!("element {name}")))
        .collect();
    ops::read_cell(&dimensions, &loaded.storage, cube_id, cube, &ids)
}

fn number(database: &str, cube: &str, path: &[&str]) -> f64 {
    match cell(database, cube, path) {
        Some(CellValue::Numeric(v)) => v,
        other => panic!("{path:?} is {other:?}"),
    }
}

fn assert_close(actual: f64, expected: f64, tolerance: f64) {
    assert!((actual - expected).abs() <= tolerance, "{actual} is not within {tolerance} of {expected}");
}

#[test]
fn online_retail_totals_and_product_descriptions() {
    // 525,461 + 541,910 source lines minus the 22,523 both sheets repeat.
    assert_close(number("OnlineRetail", "Sales", &["All months", "All countries", "All products", "Invoice lines"]), 1_044_848.0, 0.0);
    assert_close(number("OnlineRetail", "Sales", &["2010", "All countries", "All products", "Revenue"]), 9_111_106.09, 0.01);
    assert_eq!(cell("OnlineRetail", "#_Product_", &["Description", "85123A"]), Some(CellValue::String("WHITE HANGING HEART T-LIGHT HOLDER".into())));
}

#[test]
fn nobel_prize_shares_sum_to_one_prize_per_year() {
    assert_close(number("NobelPrizes", "Laureates", &["Physics", "All years", "World", "All laureates", "Laureates"]), 230.0, 0.0);
    // A category-year's shares add up to one prize: Physics was awarded in 119 years.
    assert_close(number("NobelPrizes", "Laureates", &["Physics", "All years", "World", "All laureates", "Prize share"]), 119.0, 1e-6);
    assert_close(number("NobelPrizes", "Laureates", &["All prizes", "1900s", "World", "Female", "Laureates"]), 3.0, 0.0);
}

#[test]
fn world_population_rolls_up_from_countries_and_age_groups() {
    assert_close(number("WorldPopulation", "Population", &["World", "2025", "Both sexes", "All ages"]), 8_231_613_565.0, 0.0);
    let by_group: f64 = ["Children (0-14)", "Working age (15-64)", "Elderly (65+)"]
        .iter()
        .map(|g| number("WorldPopulation", "Population", &["World", "2025", "Both sexes", g]))
        .sum();
    assert_close(by_group, 8_231_613_565.0, 0.5);
}

#[test]
fn climate_world_emissions_and_weighted_annual_means() {
    // Matches the Global Carbon Budget's World total (international transport included).
    assert_close(number("Climate", "Emissions", &["World", "2023", "All sources"]), 38_094.039, 0.01);
    // Twelve months at weight 1/12: NOAA's published 2023 annual mean at Mauna Loa.
    assert_close(number("Climate", "Atmosphere", &["2023", "Annual mean", "CO2 (ppm)"]), 421.08, 0.01);
    assert_close(number("Climate", "Atmosphere", &["2023", "Annual mean", "Temperature anomaly (C)"]), 1.17, 0.01);
}

#[test]
fn exoplanet_discoveries_and_catalog_strings() {
    assert_close(number("Exoplanets", "Discoveries", &["All years", "All methods", "All facilities", "All sizes"]), 6372.0, 0.0);
    assert_close(number("Exoplanets", "Discoveries", &["All years", "Transit", "All facilities", "All sizes"]), 4709.0, 0.0);
    assert_eq!(cell("Exoplanets", "Catalog", &["TRAPPIST-1 e", "Host star"]), Some(CellValue::String("TRAPPIST-1".into())));
    assert_close(number("Exoplanets", "Catalog", &["TRAPPIST-1 e", "Radius (Earth radii)"]), 0.92, 1e-9);
}

#[test]
fn particle_physics_peaks_through_the_known_particles_hierarchy() {
    let events = |mass: &str, charge: &str| number("ParticlePhysics", "Events", &[mass, charge, "All types", "Whole detector", "All runs"]);
    assert_close(events("All masses", "All pairs"), 100_000.0, 0.0);
    assert_close(events("All masses", "Opposite sign"), 63_946.0, 0.0);
    // "Known particles" groups the same bins under a second parent.
    assert_close(events("J/psi (3.097 GeV)", "All pairs"), 10_107.0, 0.0);
    assert_close(events("3.05-3.10 GeV", "Opposite sign"), 4_121.0, 0.0);
    assert_close(events("Upsilon(1S) (9.460 GeV)", "Opposite sign"), 2_080.0, 0.0);
    // The Z decays into a muon and an antimuon: almost no same-sign pairs.
    assert_close(events("Z boson (91.19 GeV)", "Opposite sign"), 293.0, 0.0);
    assert_close(events("Z boson (91.19 GeV)", "Same sign"), 4.0, 0.0);
    assert_close(number("ParticlePhysics", "Events", &["All masses", "All pairs", "All types", "Barrel + barrel", "All runs"]), 21_798.0, 0.0);
}

#[test]
fn genetic_variants_roll_up_by_chromosome_and_classification() {
    let variants = |gene: &str, class: &str| number("GeneticVariants", "Variants", &[gene, "All variant types", class, "All review levels", "All years", "Variants"]);
    assert_close(variants("All genes", "All classifications"), 4_578_407.0, 0.0);
    // The pathogenic side: Pathogenic + Pathogenic/Likely pathogenic + Likely pathogenic.
    assert_close(variants("All genes", "Pathogenic side"), 366_712.0, 0.0);
    assert_close(variants("All genes", "Uncertain significance"), 2_362_554.0, 0.0);
    assert_close(variants("BRCA1", "All classifications"), 15_559.0, 0.0);
    assert_close(variants("BRCA1", "Pathogenic"), 3_781.0, 0.0);
    assert!(variants("Chromosome 17", "All classifications") > variants("BRCA1", "All classifications"), "BRCA1 rolls up into chromosome 17");
    assert_close(number("GeneticVariants", "Variants", &["All genes", "All variant types", "All classifications", "All review levels", "All years", "Submitters"]), 6_865_225.0, 0.0);
}

#[test]
fn metabolomics_concentrations_add_up_and_count_their_samples() {
    let value = |acid: &str, who: &str, measure: &str| number("Metabolomics", "Bile acids", &[acid, who, measure]);
    // Total bile acids, summed over each group's 20 people.
    assert_close(value("All bile acids", "Parkinson's disease", "Concentration (ng/mL)"), 57_188.033, 0.001);
    assert_close(value("All bile acids", "Healthy controls", "Concentration (ng/mL)"), 40_297.728, 0.001);
    // A group's mean is Concentration / Samples measured: TCDCA, the study's
    // finding, is about twice as high in Parkinson's patients.
    let mean = |who: &str| value("Taurochenodeoxycholic acid", who, "Concentration (ng/mL)") / value("Taurochenodeoxycholic acid", who, "Samples measured");
    assert_close(mean("Parkinson's disease"), 83.967, 0.001);
    assert_close(mean("Healthy controls"), 42.528, 0.001);
    assert_close(value("All bile acids", "All participants", "Samples measured"), 1_670.0, 0.0);
}
