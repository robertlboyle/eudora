//! A small deterministic random generator (SplitMix64): the same seed gives the same corpus
//! on every computer and every version of the toolchain.

#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// A generator for one item (e.g. one message slot) of a seeded run.
    pub fn for_item(seed: u64, item: u64, stream: u64) -> Rng {
        let mut r = Rng::new(seed);
        r.0 ^= item.wrapping_mul(0xA24B_AED4_963E_E407) ^ stream.wrapping_mul(0x9FB2_1C65_1E98_DF25);
        r.next();
        r
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// 0..n
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    pub fn range(&mut self, lo: u64, hi_inclusive: u64) -> u64 {
        lo + self.below(hi_inclusive - lo + 1)
    }

    /// True with probability p.
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    /// Skewed toward 0 (a few items used a lot, many rarely): like real senders and words.
    pub fn skewed(&mut self, n: u64) -> u64 {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        ((u * u * u) * n as f64) as u64 % n.max(1)
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

/// A stable 64-bit mix of a few numbers (for decisions that must be recomputable).
pub fn mix(a: u64, b: u64, c: u64) -> u64 {
    Rng::for_item(a, b, c).next()
}
