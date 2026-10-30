//! Chart geometry: "nice" axis ticks and stacking. The SVG itself is drawn
//! by the UI; this is the arithmetic, tested on the host.

use super::pivot::Tuple;

/// Tick values covering `[lo, hi]`, about `target` of them, at steps of
/// 1, 2 or 5 times a power of ten.
pub fn nice_ticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    let (lo, hi) = if lo == hi { (lo - 1.0, hi + 1.0) } else { (lo.min(hi), lo.max(hi)) };
    let raw = (hi - lo) / target.max(1) as f64;
    let magnitude = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0].into_iter().map(|m| m * magnitude).find(|s| *s >= raw).unwrap_or(10.0 * magnitude);
    let start = (lo / step).floor() * step;
    let mut ticks = Vec::new();
    let mut t = start;
    while t <= hi + step * 1e-9 {
        // Round away float noise (0.30000000000000004).
        ticks.push((t / step).round() * step);
        t += step;
    }
    if ticks.last().is_some_and(|&last| last < hi) {
        ticks.push(ticks.last().unwrap() + step);
    }
    ticks
}

/// The value range a chart's y axis must cover (always including 0, so
/// bars start from the baseline).
pub fn value_range(values: impl IntoIterator<Item = f64>) -> (f64, f64) {
    values.into_iter().fold((0.0f64, 0.0f64), |(lo, hi), v| (lo.min(v), hi.max(v)))
}

/// Stacks series per x position: returns `(bottom, top)` for each series
/// and x. Positive values stack upwards from 0, negative ones downwards,
/// so mixed signs never overlap.
pub fn stack(series: &[Vec<Option<f64>>]) -> Vec<Vec<(f64, f64)>> {
    let n_x = series.iter().map(Vec::len).max().unwrap_or(0);
    let mut up = vec![0.0; n_x];
    let mut down = vec![0.0; n_x];
    series
        .iter()
        .map(|values| {
            (0..n_x)
                .map(|x| {
                    let v = values.get(x).copied().flatten().unwrap_or(0.0);
                    if v >= 0.0 {
                        let bottom = up[x];
                        up[x] += v;
                        (bottom, up[x])
                    } else {
                        let top = down[x];
                        down[x] += v;
                        (down[x], top)
                    }
                })
                .collect()
        })
        .collect()
}

/// Maps a value in `[lo, hi]` onto `[out_lo, out_hi]`.
pub fn scale(v: f64, (lo, hi): (f64, f64), (out_lo, out_hi): (f64, f64)) -> f64 {
    if hi == lo {
        return (out_lo + out_hi) / 2.0;
    }
    out_lo + (v - lo) / (hi - lo) * (out_hi - out_lo)
}

/// Rows grouped by their outermost element, in order: the panels of a
/// chart split into small multiples.
pub fn group_rows(rows: &[Tuple]) -> Vec<(u32, Vec<Tuple>)> {
    let mut groups: Vec<(u32, Vec<Tuple>)> = Vec::new();
    for row in rows {
        let Some(&(element, _)) = row.first() else { continue };
        match groups.last_mut() {
            Some((last, group)) if *last == element => group.push(row.clone()),
            _ => groups.push((element, vec![row.clone()])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_are_round_and_cover_the_range() {
        assert_eq!(nice_ticks(0.0, 8_231_613_565.0, 5), vec![0.0, 2e9, 4e9, 6e9, 8e9, 1e10]);
        assert_eq!(nice_ticks(0.0, 427.3, 5), vec![0.0, 100.0, 200.0, 300.0, 400.0, 500.0]);
        assert_eq!(nice_ticks(-0.3, 1.2, 5), vec![-0.5, 0.0, 0.5, 1.0, 1.5]);
    }

    #[test]
    fn rows_group_by_their_outermost_element_in_order() {
        let nested: Vec<Tuple> = vec![vec![(7, 0), (1, 0)], vec![(7, 0), (2, 0)], vec![(3, 0), (1, 0)]];
        let groups = group_rows(&nested);
        assert_eq!(groups.iter().map(|g| (g.0, g.1.len())).collect::<Vec<_>>(), vec![(7, 2), (3, 1)]);
        assert_eq!(groups[0].1[1], nested[1]);

        let single: Vec<Tuple> = vec![vec![(5, 0)], vec![(4, 1)]];
        assert_eq!(group_rows(&single).iter().map(|g| (g.0, g.1.len())).collect::<Vec<_>>(), vec![(5, 1), (4, 1)]);
        assert!(group_rows(&[]).is_empty());
    }

    #[test]
    fn stacks_positive_up_and_negative_down() {
        let s = stack(&[vec![Some(2.0), Some(-1.0)], vec![Some(3.0), Some(-2.0)], vec![None, Some(4.0)]]);
        assert_eq!(s[0], vec![(0.0, 2.0), (-1.0, 0.0)]);
        assert_eq!(s[1], vec![(2.0, 5.0), (-3.0, -1.0)]);
        assert_eq!(s[2], vec![(5.0, 5.0), (0.0, 4.0)]);
    }
}
