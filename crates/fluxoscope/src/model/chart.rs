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

/// A rectangle in a chart's coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Lays `weights` out in `area` as a squarified treemap: one rectangle per
/// weight, in the same order, each with a share of the area matching its
/// share of the weights. Weights of 0 or below get an empty rectangle.
pub fn treemap(weights: &[f64], area: Rect) -> Vec<Rect> {
    let mut out = vec![Rect::default(); weights.len()];
    let mut order: Vec<usize> = (0..weights.len()).filter(|&i| weights[i] > 0.0).collect();
    order.sort_by(|&a, &b| weights[b].total_cmp(&weights[a]));
    let total: f64 = order.iter().map(|&i| weights[i]).sum();
    if total <= 0.0 {
        return out;
    }
    let unit = area.w * area.h / total;
    let mut free = area;
    let mut start = 0;
    while start < order.len() {
        // Grow a strip along the shorter side while its tiles get squarer.
        let side = free.w.min(free.h);
        if side <= 0.0 {
            break;
        }
        let largest = weights[order[start]] * unit;
        let (mut end, mut sum, mut best) = (start, 0.0, f64::INFINITY);
        while end < order.len() {
            let tile = weights[order[end]] * unit;
            let grown = sum + tile;
            let worst = (side * side * largest / (grown * grown)).max(grown * grown / (side * side * tile));
            if worst > best {
                break;
            }
            (best, sum, end) = (worst, grown, end + 1);
        }
        let thickness = sum / side;
        let along_height = free.w >= free.h;
        let mut offset = 0.0;
        for &i in &order[start..end] {
            let length = weights[i] * unit / thickness;
            out[i] = if along_height { Rect { x: free.x, y: free.y + offset, w: thickness, h: length } } else { Rect { x: free.x + offset, y: free.y, w: length, h: thickness } };
            offset += length;
        }
        free = if along_height { Rect { x: free.x + thickness, w: free.w - thickness, ..free } } else { Rect { y: free.y + thickness, h: free.h - thickness, ..free } };
        start = end;
    }
    out
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
    fn treemap_areas_match_the_weights_and_fill_the_space() {
        let area = Rect { x: 10.0, y: 20.0, w: 600.0, h: 400.0 };
        let weights = [6.0, 0.0, 6.0, 4.0, 3.0, -5.0, 2.0, 2.0, 1.0];
        let tiles = treemap(&weights, area);
        let total: f64 = weights.iter().filter(|w| **w > 0.0).sum();
        for (w, t) in weights.iter().zip(&tiles) {
            let expected = w.max(0.0) / total * area.w * area.h;
            assert!((t.w * t.h - expected).abs() < 1e-6, "{w}: {t:?}");
            if *w > 0.0 {
                assert!(t.x >= area.x - 1e-9 && t.y >= area.y - 1e-9 && t.x + t.w <= area.x + area.w + 1e-9 && t.y + t.h <= area.y + area.h + 1e-9, "{t:?}");
            }
        }
        for (i, a) in tiles.iter().enumerate() {
            for b in &tiles[i + 1..] {
                let overlap = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x) > 1e-6 && (a.y + a.h).min(b.y + b.h) - a.y.max(b.y) > 1e-6;
                assert!(!overlap, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn treemap_tiles_stay_fairly_square() {
        let tiles = treemap(&[1.0; 16], Rect { x: 0.0, y: 0.0, w: 400.0, h: 400.0 });
        assert!(tiles.iter().all(|t| t.w.max(t.h) / t.w.min(t.h) < 2.0), "{tiles:?}");
    }

    #[test]
    fn treemap_of_nothing_is_empty() {
        let area = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        assert_eq!(treemap(&[0.0, -1.0], area), vec![Rect::default(); 2]);
        assert!(treemap(&[], area).is_empty());
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
