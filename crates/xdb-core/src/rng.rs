//! Small deterministic PRNG (SplitMix64). No external crates.

#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `0..n` (0 if n == 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
    /// Uniform in `lo..=hi`.
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        lo + self.below((hi - lo) as u64 + 1) as i64
    }
    /// Uniform in `[0, 1)`.
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }
    /// Exponential with the given mean.
    pub fn exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.f64()).ln()
    }
    /// Zipf-like skewed pick in `0..n` with skew `theta` (0 = uniform).
    /// Uses the simple power method: floor(n * u^(1/(1-theta)))... approximated by u^(1+theta*4).
    pub fn skewed(&mut self, n: u64, theta: f64) -> u64 {
        if theta <= 0.0 {
            return self.below(n);
        }
        let u = self.f64();
        let x = u.powf(1.0 + theta * 4.0);
        ((x * n as f64) as u64).min(n.saturating_sub(1))
    }
}
