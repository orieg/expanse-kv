//! Statistical gating primitives and deterministic PRNG (`AGENTS.md` §8.4, §8.6).
//!
//! Provides:
//! - [`SplitMix64`]: deterministic 64-bit PRNG for reproducible key access
//!   sequences and bootstrap resampling.
//! - [`percentile_sorted`]: exact nearest-rank percentile on sorted samples.
//! - [`bca_ci_95`]: Bias-Corrected and Accelerated (BCa) 95% bootstrap
//!   confidence interval for the sample mean (Efron 1987, *JASA* 82(397):171–185).

/// Deterministic SplitMix64 PRNG (Steele, Lea, Flood 2014, OOPSLA).
#[derive(Debug, Clone, Copy)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Creates a new generator from `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Returns the next pseudorandom `u64`.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Returns a pseudorandom index in `0..bound` (assumes `bound > 0`).
    #[inline]
    pub fn next_usize(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }
}

/// Summary statistics with a BCa 95% bootstrap confidence interval for the mean.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BcaInterval {
    /// Sample mean of the input observations.
    pub mean: f64,
    /// Lower bound of the 95% BCa confidence interval.
    pub ci_low: f64,
    /// Upper bound of the 95% BCa confidence interval.
    pub ci_high: f64,
    /// Number of input observations.
    pub n: usize,
    /// Number of bootstrap resamples used.
    pub resamples: usize,
}

/// Returns the nearest-rank percentile `p` (in `0.0..=100.0`) from a non-empty
/// sorted slice of `u64` latency samples.
#[must_use]
pub fn percentile_sorted(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let clamped = p.clamp(0.0, 100.0);
    let rank = ((clamped / 100.0) * (sorted.len() as f64)).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Standard normal CDF $\Phi(x)$ via Abramowitz & Stegun (1964) 7.1.26
/// (maximum absolute error $< 1.5 \times 10^{-7}$).
#[must_use]
pub fn normal_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.231_641_9 * x.abs());
    let poly = t
        * (0.319_381_530
            + t * (-0.356_563_782
                + t * (1.781_477_937 + t * (-1.821_255_978 + t * 1.330_274_429))));
    let pdf = (-0.5 * x * x).exp() / (2.0 * core::f64::consts::PI).sqrt();
    let tail = pdf * poly;
    if x >= 0.0 { 1.0 - tail } else { tail }
}

/// Inverse standard normal CDF $\Phi^{-1}(p)$ via Peter Acklam's rational
/// approximation (relative error $< 1.15 \times 10^{-9}$ across $(0, 1)$).
#[must_use]
pub fn normal_inv_cdf(p: f64) -> f64 {
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239e0,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838e0,
        -2.549_732_539_343_734e0,
        4.374_664_141_464_968e0,
        2.938_163_982_698_783e0,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996e0,
        3.754_408_661_907_416e0,
    ];
    const P_LOW: f64 = 0.02425;
    const P_HIGH: f64 = 1.0 - P_LOW;

    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= P_HIGH {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

/// Computes the BCa (Bias-Corrected and Accelerated) 95% bootstrap confidence
/// interval for the mean of `samples` using `resamples` deterministic bootstrap
/// draws (Efron 1987, *JASA* 82(397):171–185; `AGENTS.md` §8.4).
#[must_use]
pub fn bca_ci_95(samples: &[f64], resamples: usize, seed: u64) -> BcaInterval {
    let n = samples.len();
    assert!(n >= 2, "bca_ci_95 requires at least 2 samples");
    assert!(
        resamples >= 1000,
        "AGENTS.md §8.4 requires >= 1000 bootstrap resamples"
    );

    let mean = samples.iter().sum::<f64>() / (n as f64);

    // Constant-sample fast path: zero variance collapses CI to the point estimate.
    if samples.iter().all(|&x| (x - mean).abs() == 0.0) {
        return BcaInterval {
            mean,
            ci_low: mean,
            ci_high: mean,
            n,
            resamples,
        };
    }

    let mut rng = SplitMix64::new(seed);
    let mut boot = Vec::with_capacity(resamples);
    let mut less_count = 0usize;
    for _ in 0..resamples {
        let mut sum = 0.0;
        for _ in 0..n {
            sum += samples[rng.next_usize(n)];
        }
        let bm = sum / (n as f64);
        if bm < mean {
            less_count += 1;
        }
        boot.push(bm);
    }
    boot.sort_by(f64::total_cmp);

    // Bias-correction z0.
    let prop = (less_count as f64 / resamples as f64).clamp(
        1.0 / (resamples as f64 + 1.0),
        1.0 - 1.0 / (resamples as f64 + 1.0),
    );
    let z0 = normal_inv_cdf(prop);

    // Jackknife acceleration a.
    let total_sum: f64 = samples.iter().sum();
    let mut jack = Vec::with_capacity(n);
    for &x in samples {
        jack.push((total_sum - x) / ((n - 1) as f64));
    }
    let jack_mean = jack.iter().sum::<f64>() / (n as f64);
    let mut num = 0.0;
    let mut den = 0.0;
    for &jm in &jack {
        let d = jack_mean - jm;
        num += d * d * d;
        den += d * d;
    }
    let accel = if den > 0.0 {
        num / (6.0 * den.powf(1.5))
    } else {
        0.0
    };

    let z_lo = normal_inv_cdf(0.025);
    let z_hi = normal_inv_cdf(0.975);

    let adj = |z: f64| -> f64 {
        let denom = 1.0 - accel * (z0 + z);
        if denom.abs() < 1e-9 {
            return if z < 0.0 { 0.025 } else { 0.975 };
        }
        normal_cdf(z0 + (z0 + z) / denom)
    };

    let a1 = adj(z_lo).clamp(0.0, 1.0);
    let a2 = adj(z_hi).clamp(0.0, 1.0);
    let idx_lo = ((a1 * (resamples - 1) as f64).floor() as usize).min(resamples - 1);
    let idx_hi = ((a2 * (resamples - 1) as f64).ceil() as usize).min(resamples - 1);

    let ci_low = boot[idx_lo].min(mean);
    let ci_high = boot[idx_hi].max(mean);

    BcaInterval {
        mean,
        ci_low,
        ci_high,
        n,
        resamples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_cdf_and_inv_cdf_pinned_to_standard_quantiles() {
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((normal_cdf(1.959_963_984_540_054) - 0.975).abs() < 1e-6);
        assert!((normal_cdf(-1.959_963_984_540_054) - 0.025).abs() < 1e-6);
        assert!((normal_inv_cdf(0.5)).abs() < 1e-9);
        assert!((normal_inv_cdf(0.975) - 1.959_963_984_540_054).abs() < 1e-7);
        assert!((normal_inv_cdf(0.025) + 1.959_963_984_540_054).abs() < 1e-7);
    }

    #[test]
    fn bca_ci_95_brackets_mean_and_collapses_on_constants() {
        let constant = [42.0; 10];
        let c = bca_ci_95(&constant, 2_000, 1);
        assert_eq!(c.mean, 42.0);
        assert_eq!(c.ci_low, 42.0);
        assert_eq!(c.ci_high, 42.0);

        let data = [10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0, 18.0, 19.0];
        let ci = bca_ci_95(&data, 2_000, 42);
        assert!((ci.mean - 14.5).abs() < 1e-12);
        assert!(ci.ci_low < ci.mean);
        assert!(ci.ci_high > ci.mean);
        assert!(ci.ci_low > 12.0 && ci.ci_high < 17.0);
    }

    #[test]
    fn percentile_sorted_matches_nearest_rank() {
        let s: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_sorted(&s, 50.0), 50);
        assert_eq!(percentile_sorted(&s, 99.0), 99);
        assert_eq!(percentile_sorted(&s, 100.0), 100);
    }
}
