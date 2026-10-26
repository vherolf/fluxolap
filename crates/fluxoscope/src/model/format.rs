//! Number formatting (ported from fluxoterm): grouped from 10,000 up,
//! four significant digits, and per-column alignment on the decimal point.

/// A number for display: whole numbers grouped by thousands, others with
/// four significant digits (at most two decimals from 1,000 up), so
/// 8231613565 reads 8,231,613,565 and 316.908333 reads 316.9.
pub fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return group(n);
    }
    let abs = n.abs();
    let decimals = if abs >= 1000.0 {
        2
    } else if abs == 0.0 {
        0
    } else {
        (3 - abs.log10().floor() as i32).clamp(0, 6) as usize
    };
    let s = format!("{n:.decimals$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    match s.split_once('.') {
        Some((int, frac)) => format!("{}.{frac}", group_str(int)),
        None => group_str(&s),
    }
}

/// How many decimals `format_number` shows for `n`.
pub fn decimals_of(n: f64) -> usize {
    format_number(n).split_once('.').map_or(0, |(_, frac)| frac.len())
}

/// `format_number`, padded with zeros to `decimals` decimals, so a
/// column's numbers line up on the decimal point.
pub fn format_aligned(n: f64, decimals: usize) -> String {
    let s = format_number(n);
    if decimals == 0 {
        return s;
    }
    match s.split_once('.') {
        Some((int, frac)) => format!("{int}.{frac:0<decimals$}"),
        None => format!("{s}.{}", "0".repeat(decimals)),
    }
}

/// A whole number grouped by thousands.
pub fn group(n: f64) -> String {
    group_str(&format!("{:.0}", n))
}

/// Groups by thousands from five digits up (10,000), so years and other
/// four-digit numbers stay plain: 2007, 1026.
fn group_str(int: &str) -> String {
    let (sign, digits) = int.strip_prefix('-').map_or(("", int), |d| ("-", d));
    if digits.len() <= 4 {
        return int.to_string();
    }
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// A file size for people: `834 B`, `12.4 KB`, `3.1 MB` (powers of 1024).
pub fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_well() {
        assert_eq!(format_number(8231613565.0), "8,231,613,565");
        assert_eq!(format_number(316.9083333333333), "316.9");
        assert_eq!(format_number(2007.0), "2007");
        assert_eq!(format_number(-0.025), "-0.025");
        assert_eq!(format_aligned(16689936.0, 2), "16,689,936.00");
        assert_eq!(decimals_of(1929099.08), 2);
    }

    #[test]
    fn file_sizes() {
        assert_eq!(file_size(834), "834 B");
        assert_eq!(file_size(12_700), "12.4 KB");
        assert_eq!(file_size(3_250_000), "3.1 MB");
    }
}
