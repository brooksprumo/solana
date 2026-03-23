use {
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{
        fmt,
        hash::{BuildHasher as _, Hasher as _},
        num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    },
};

#[derive(Clone, Copy, Debug)]
pub struct PubkeyBinCalculator24 {
    // how many bits from the first 3 bytes to shift away to ignore when calculating bin
    shift_bits: u32,
}

impl PubkeyBinCalculator24 {
    const MAX_BITS: u32 = 24;
    const MAX_BINS: usize = 1_usize << Self::MAX_BITS;

    /// Creates a new PubkeyBinCalculator24
    ///
    /// # Panics
    ///
    /// This function will panic if the following conditions are not met:
    /// * `bins` must be greater than zero
    /// * `bins` must be a power of two
    /// * `bins` must be less than or equal to 2^24
    pub fn new(bins: usize) -> Self {
        // SAFETY: Caller must guarantee `bins` is non-zero.
        let bins = NonZeroUsize::new(bins).expect("bins is non-zero");
        assert!(bins.is_power_of_two());
        assert!(bins.get() <= Self::MAX_BINS);
        // SAFETY: `bins` was already non-zero, and we just asserted it fits in 24 bits.
        let bins = unsafe { NonZeroU32::new_unchecked(bins.get() as u32) };
        let bits = bins.ilog2();
        Self {
            shift_bits: Self::MAX_BITS - bits,
        }
    }

    pub fn bins(&self) -> usize {
        1 << (Self::MAX_BITS - self.shift_bits)
    }

    #[inline]
    pub fn bin_from_pubkey(&self, pubkey: &Pubkey) -> usize {
        let as_ref = pubkey.as_ref();
        (((as_ref[0] as usize) << 16) | ((as_ref[1] as usize) << 8) | (as_ref[2] as usize))
            >> self.shift_bits
    }
}

// brooks TODO: doc
#[derive(Clone)]
pub struct PubkeyBinCalculator {
    num_bins: NonZeroUsize,
    hasher_builder: PubkeyHasherBuilder,
}

impl PubkeyBinCalculator {
    pub fn bin_from_pubkey(&self, pubkey: &Pubkey) -> usize {
        let mut hasher = self.hasher_builder.build_hasher();
        hasher.write(pubkey.as_array());
        let hash = hasher.finish();
        // SAFETY: Value is guaranteed to be non-zero, since it comes from a NonZero type.
        let num_bins = unsafe { NonZeroU64::new_unchecked(self.num_bins.get() as u64) };
        (hash % num_bins) as usize
    }
}

impl fmt::Debug for PubkeyBinCalculator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkeyBinCalculator")
            .field("num_bins", &self.num_bins)
            .finish()
    }
}

// brooks TODO: doc
pub struct PubkeyBinCalculatorBuilder {
    hasher_builder: PubkeyHasherBuilder,
}

impl PubkeyBinCalculatorBuilder {
    // brooks TODO: doc
    pub fn new() -> Self {
        Self {
            hasher_builder: PubkeyHasherBuilder::default(),
        }
    }
    // brooks TODO: doc
    pub fn build(&self, num_bins: NonZeroUsize) -> PubkeyBinCalculator {
        PubkeyBinCalculator {
            num_bins,
            hasher_builder: self.hasher_builder.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl PubkeyBinCalculator24 {
        fn lowest_pubkey_from_bin(&self, mut bin: usize) -> Pubkey {
            assert!(bin < self.bins());
            bin <<= self.shift_bits;
            let mut pubkey = Pubkey::from([0; 32]);
            pubkey.as_mut()[0] = ((bin / 256 / 256) & 0xff) as u8;
            pubkey.as_mut()[1] = ((bin / 256) & 0xff) as u8;
            pubkey.as_mut()[2] = (bin & 0xff) as u8;
            pubkey
        }

        fn highest_pubkey_from_bin(&self, mut bin: usize) -> Pubkey {
            assert!(bin < self.bins());
            let mask = (1 << self.shift_bits) - 1;
            bin <<= self.shift_bits;
            bin |= mask;
            let mut pubkey = Pubkey::from([0xff; 32]);
            pubkey.as_mut()[0] = ((bin / 256 / 256) & 0xff) as u8;
            pubkey.as_mut()[1] = ((bin / 256) & 0xff) as u8;
            pubkey.as_mut()[2] = (bin & 0xff) as u8;
            pubkey
        }
    }

    #[test]
    fn test_pubkey_bins() {
        for i in 0..=24 {
            let bins = 2u32.pow(i);
            let calc = PubkeyBinCalculator24::new(bins as usize);
            assert_eq!(calc.shift_bits, 24 - i, "i: {i}");
            for bin in 0..bins {
                assert_eq!(
                    bin as usize,
                    calc.bin_from_pubkey(&calc.lowest_pubkey_from_bin(bin as usize))
                );

                assert_eq!(
                    bin as usize,
                    calc.bin_from_pubkey(&calc.highest_pubkey_from_bin(bin as usize))
                );

                assert_eq!(calc.bins(), bins as usize);
            }
        }
    }

    #[test]
    fn test_pubkey_bins_pubkeys() {
        let mut pk = Pubkey::from([0; 32]);
        for i in 0..=8 {
            let bins = 2usize.pow(i);
            let calc = PubkeyBinCalculator24::new(bins);

            let shift_bits = calc.shift_bits - 16; // we are only dealing with first byte

            pk.as_mut()[0] = 0;
            assert_eq!(0, calc.bin_from_pubkey(&pk));
            pk.as_mut()[0] = 0xff;
            assert_eq!(bins - 1, calc.bin_from_pubkey(&pk));

            for bin in 0..bins {
                pk.as_mut()[0] = (bin << shift_bits) as u8;
                assert_eq!(
                    bin,
                    calc.bin_from_pubkey(&pk),
                    "bin: {}/{}, shift_bits: {}, val: {}",
                    bin,
                    bins,
                    shift_bits,
                    pk.as_ref()[0]
                );
                if bin > 0 {
                    pk.as_mut()[0] = ((bin << shift_bits) - 1) as u8;
                    assert_eq!(bin - 1, calc.bin_from_pubkey(&pk));
                }
            }
        }

        for i in 9..=16 {
            let mut pk = Pubkey::from([0; 32]);
            let bins = 2usize.pow(i);
            let calc = PubkeyBinCalculator24::new(bins);

            let shift_bits = calc.shift_bits - 8;

            pk.as_mut()[1] = 0;
            assert_eq!(0, calc.bin_from_pubkey(&pk));
            pk.as_mut()[0] = 0xff;
            pk.as_mut()[1] = 0xff;
            assert_eq!(bins - 1, calc.bin_from_pubkey(&pk));

            let mut pk = Pubkey::from([0; 32]);
            for bin in 0..bins {
                let mut target = (bin << shift_bits) as u16;
                pk.as_mut()[0] = (target / 256) as u8;
                pk.as_mut()[1] = (target % 256) as u8;
                assert_eq!(
                    bin,
                    calc.bin_from_pubkey(&pk),
                    "bin: {}/{}, shift_bits: {}, val: {}",
                    bin,
                    bins,
                    shift_bits,
                    pk.as_ref()[0]
                );
                if bin > 0 {
                    target -= 1;
                    pk.as_mut()[0] = (target / 256) as u8;
                    pk.as_mut()[1] = (target % 256) as u8;
                    assert_eq!(bin - 1, calc.bin_from_pubkey(&pk));
                }
            }
        }

        for i in 17..=24 {
            let mut pk = Pubkey::from([0; 32]);
            let bins = 2usize.pow(i);
            let calc = PubkeyBinCalculator24::new(bins);

            let shift_bits = calc.shift_bits;

            pk.as_mut()[1] = 0;
            assert_eq!(0, calc.bin_from_pubkey(&pk));
            pk.as_mut()[0] = 0xff;
            pk.as_mut()[1] = 0xff;
            pk.as_mut()[2] = 0xff;
            assert_eq!(bins - 1, calc.bin_from_pubkey(&pk));

            let mut pk = Pubkey::from([0; 32]);
            for bin in 0..bins {
                let mut target = (bin << shift_bits) as u32;
                pk.as_mut()[0] = (target / 256 / 256) as u8;
                pk.as_mut()[1] = ((target / 256) % 256) as u8;
                pk.as_mut()[2] = (target % 256) as u8;
                assert_eq!(
                    bin,
                    calc.bin_from_pubkey(&pk),
                    "bin: {}/{}, shift_bits: {}, val: {:?}",
                    bin,
                    bins,
                    shift_bits,
                    &pk.as_ref()[0..3],
                );
                if bin > 0 {
                    target -= 1;
                    pk.as_mut()[0] = (target / 256 / 256) as u8;
                    pk.as_mut()[1] = ((target / 256) % 256) as u8;
                    pk.as_mut()[2] = (target % 256) as u8;
                    assert_eq!(bin - 1, calc.bin_from_pubkey(&pk));
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "bins.is_power_of_two()")]
    fn test_pubkey_bins_bad_non_pow2() {
        PubkeyBinCalculator24::new(3);
    }

    #[test]
    #[should_panic(expected = "bins.get() <= Self::MAX_BINS")]
    fn test_pubkey_bins_bad_too_large() {
        PubkeyBinCalculator24::new(1 << (PubkeyBinCalculator24::MAX_BITS + 1));
    }
    #[test]
    #[should_panic(expected = "bins is non-zero")]
    fn test_pubkey_bins_bad_is_zero() {
        PubkeyBinCalculator24::new(0);
    }

    #[test]
    fn test_bin_from_pubkey_is_deterministic() {
        for num_bins in [1 << 10, 1 << 14, 1 << 19] {
            let bin_calculator_builder = PubkeyBinCalculatorBuilder::new();
            let bin_calculator = bin_calculator_builder.build(NonZeroUsize::new(num_bins).unwrap());
            for i_pubkey in 0..1_000 {
                let pubkey = Pubkey::new_unique();
                let expected_bin = bin_calculator.bin_from_pubkey(&pubkey);
                for i_calculation in 0..100 {
                    let actual_bin = bin_calculator.bin_from_pubkey(&pubkey);
                    assert_eq!(
                        actual_bin, expected_bin,
                        "num_bins: {num_bins}, i_pubkey: {i_pubkey}, i_calculation: {i_calculation}, pubkey: {pubkey}",
                    );
                }
            }
        }
    }
}
