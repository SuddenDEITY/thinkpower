//! Unit conversions between the GUI/config units (W, s) and sysfs units (µW, µs).

pub fn w_to_uw(w: f64) -> u64 {
    (w * 1e6).round().max(0.0) as u64
}

pub fn uw_to_w(uw: u64) -> f64 {
    uw as f64 / 1e6
}

pub fn s_to_us(s: f64) -> u64 {
    (s * 1e6).round().max(0.0) as u64
}

pub fn us_to_s(us: u64) -> f64 {
    us as f64 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watts() {
        assert_eq!(w_to_uw(5.0), 5_000_000);
        assert_eq!(w_to_uw(28.0), 28_000_000);
        assert_eq!(w_to_uw(0.125), 125_000);
        assert_eq!(uw_to_w(64_000_000), 64.0);
    }

    #[test]
    fn seconds() {
        assert_eq!(s_to_us(2.0), 2_000_000);
        assert_eq!(s_to_us(28.0), 28_000_000);
        assert!((us_to_s(4_997_120) - 4.99712).abs() < 1e-9);
    }

    #[test]
    fn negative_clamps_to_zero() {
        assert_eq!(w_to_uw(-1.0), 0);
    }
}
