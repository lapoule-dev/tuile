// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Transcendental functions from `+ − × ÷` and bit manipulation only.
//!
//! `libm`'s `ln`, `exp`, `sin`, `atan2` may differ in their last bits from
//! one platform to the next; these do not, because IEEE 754 rounds each basic
//! operation exactly and Rust never fuses them. So the simulation's inputs,
//! and the decision digests that follow from them, are the same on macOS and
//! on the Linux CI. Accuracy is a few ulps over the ranges used here.

const LN_2: f64 = std::f64::consts::LN_2;
const TAU: f64 = std::f64::consts::TAU;

/// `log2(x)` for finite `x > 0`.
pub fn log2(x: f64) -> f64 {
    assert!(x.is_finite() && x > 0.0, "log2 of {x}");
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
    if e == -1023 {
        // Subnormal: renormalise.
        return log2(x * 4_503_599_627_370_496.0) - 52.0;
    }
    if m > std::f64::consts::SQRT_2 {
        m /= 2.0;
        e += 1;
    }
    // ln(m) = 2·atanh(z), z = (m − 1)/(m + 1), |z| < 0.172.
    let z = (m - 1.0) / (m + 1.0);
    let z2 = z * z;
    let mut term = z;
    let mut sum = 0.0;
    let mut k = 1.0;
    for _ in 0..14 {
        sum += term / k;
        term *= z2;
        k += 2.0;
    }
    e as f64 + 2.0 * sum / LN_2
}

/// `ln(x)` for finite `x > 0`.
pub fn ln(x: f64) -> f64 {
    log2(x) * LN_2
}

/// `2^x` for `|x| < 1000`.
pub fn exp2(x: f64) -> f64 {
    assert!(x.is_finite() && x.abs() < 1_000.0, "exp2 of {x}");
    let n = x.floor();
    let f = (x - n) * LN_2;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    for _ in 0..24 {
        term *= f / k;
        sum += term;
        k += 1.0;
    }
    let n = n as i64;
    let scale = f64::from_bits(((n.clamp(-1022, 1023) + 1023) as u64) << 52);
    sum * scale
}

/// `(sin x, cos x)`.
pub fn sin_cos(x: f64) -> (f64, f64) {
    let k = (x / TAU).round();
    let r = x - k * TAU;
    let r2 = r * r;
    let (mut s, mut c) = (0.0, 0.0);
    let (mut ts, mut tc) = (r, 1.0);
    let mut n = 1.0;
    for _ in 0..14 {
        s += ts;
        c += tc;
        ts *= -r2 / ((n + 1.0) * (n + 2.0));
        tc *= -r2 / (n * (n + 1.0));
        n += 2.0;
    }
    (s, c)
}

#[test]
fn the_deterministic_functions_agree_with_libm() {
    for &x in &[1e-300, 0.001, 0.5, 1.0, 1.5, 2.0, 3.7, 1e6, 1e300] {
        assert!(
            (log2(x) - x.log2()).abs() < 1e-12 * x.log2().abs().max(1.0),
            "log2 {x}"
        );
    }
    for &x in &[-20.5, -1.0, 0.0, 0.3, 1.0, 7.25, 40.0] {
        assert!((exp2(x) / x.exp2() - 1.0).abs() < 1e-13, "exp2 {x}");
    }
    for k in -40..40 {
        let x = f64::from(k) * 0.37;
        let (s, c) = sin_cos(x);
        assert!(
            (s - x.sin()).abs() < 1e-12 && (c - x.cos()).abs() < 1e-12,
            "sin_cos {x}"
        );
    }
}
