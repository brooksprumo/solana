use {
    solana_pubkey::Pubkey,
    std::hash::{self, BuildHasher as _},
    std::{fmt, num::NonZeroUsize},
};

/// Used to calculate which bin a pubkey maps to.
///
/// This struct may be cloned, and will retain the same pubkey -> bin results.
///
/// To instantiate, use `PubkeyBinCalculatorBuilder::with_bins(num_bins)`.
#[derive(Clone)]
pub struct PubkeyBinCalculator {
    mask: u64,
    hasher: hash::RandomState,
}

impl PubkeyBinCalculator {
    /// Calculates the bin that `pubkey` maps to.
    #[inline]
    pub fn bin_from_pubkey(&self, pubkey: &Pubkey) -> usize {
        let hash = self.hash_from_pubkey(pubkey);
        (hash & self.mask) as usize
    }

    /// Calculates the hash of `pubkey`.
    #[inline]
    fn hash_from_pubkey(&self, pubkey: &Pubkey) -> u64 {
        self.hasher.hash_one(pubkey)
    }
}

impl fmt::Debug for PubkeyBinCalculator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkeyBinCalculator")
            .field("num_bins", &(self.mask + 1))
            .finish_non_exhaustive()
    }
}

/// Used to build unique instances of `PubkeyBinCalculator'.
#[derive(Debug)]
pub struct PubkeyBinCalculatorBuilder;

impl PubkeyBinCalculatorBuilder {
    /// Builds a `PubkeyBinCalculator` with `num_bins`.
    ///
    /// The returned bin calculator will produce *unique* mappings
    /// compared to other bin calculators!
    ///
    /// # Panics
    ///
    /// This function will panic if the following conditions are not met:
    /// * `num_bins` must be a power of two
    pub fn with_bins(num_bins: NonZeroUsize) -> PubkeyBinCalculator {
        Self::build(num_bins, hash::RandomState::new())
    }

    /// Builds a `PubkeyBinCalculator` with `num_bins` and `offset`.
    ///
    /// The `offset` is used to instantiate a specific PubkeyHasher for the bin calculator.
    /// Prefer `with_bins()` whenever possible.
    ///
    /// The returned bin calculator will produce *identical* mappings
    /// compared to other bin calculators with the same num_bins and offset.
    ///
    /// # Panics
    ///
    /// This function will panic if the following conditions are not met:
    /// * `num_bins` must be a power of two
    pub fn with_bins_and_offset(num_bins: NonZeroUsize, offset: usize) -> PubkeyBinCalculator {
        unimplemented!()
    }

    /// Internal helper for building a `PubkeyBinCalculator`.
    ///
    /// Only intended to be called by the public build methods.
    fn build(num_bins: NonZeroUsize, hasher: hash::RandomState) -> PubkeyBinCalculator {
        assert!(num_bins.is_power_of_two());
        let num_bins_mask = num_bins.get() - 1;
        PubkeyBinCalculator {
            mask: num_bins_mask as u64,
            hasher,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensure that bin calculation is deterministic.
    #[test]
    fn test_bin_from_pubkey_is_deterministic() {
        for num_bins in [1 << 10, 1 << 14, 1 << 19] {
            let bin_calculator1 =
                PubkeyBinCalculatorBuilder::with_bins(NonZeroUsize::new(num_bins).unwrap());
            // second bin calculator that exercies Calculator::clone()
            let bin_calculator2 = bin_calculator1.clone();
            for i_pubkey in 0..1_000 {
                let pubkey = Pubkey::new_unique();
                let expected_bin = bin_calculator1.bin_from_pubkey(&pubkey);
                for i_calculation in 0..10 {
                    let actual_bin = bin_calculator1.bin_from_pubkey(&pubkey);
                    assert_eq!(
                        actual_bin, expected_bin,
                        "num_bins: {num_bins}, i_pubkey: {i_pubkey}, i_calculation: \
                         {i_calculation}, pubkey: {pubkey}",
                    );
                }
                assert_eq!(expected_bin, bin_calculator2.bin_from_pubkey(&pubkey));
            }
        }
    }

    /// Ensure that bin calculators from *different* builders produce different hashes.
    #[test]
    fn test_builders_produces_unique_instances() {
        let num_bins = NonZeroUsize::new(1).unwrap();
        let bin_calculator1 = PubkeyBinCalculatorBuilder::with_bins(num_bins);
        let bin_calculator2 = PubkeyBinCalculatorBuilder::with_bins(num_bins);
        let pubkey = Pubkey::new_unique();
        assert_ne!(
            bin_calculator1.hash_from_pubkey(&pubkey),
            bin_calculator2.hash_from_pubkey(&pubkey),
        );
    }

    /// Ensure that bin calculators from different builders, but with the
    /// same num_bins and offset, produce *identical* hashes.
    #[test]
    fn test_builders_with_same_offset_produce_identical_instances() {
        let num_bins = NonZeroUsize::new(1).unwrap();
        let offset = 0;
        let bin_calculator1 = PubkeyBinCalculatorBuilder::with_bins_and_offset(num_bins, offset);
        let bin_calculator2 = PubkeyBinCalculatorBuilder::with_bins_and_offset(num_bins, offset);
        let pubkey = Pubkey::new_unique();
        assert_eq!(
            bin_calculator1.hash_from_pubkey(&pubkey),
            bin_calculator2.hash_from_pubkey(&pubkey),
        );
    }

    /// Ensure non-power-of-two number of bins is not allowed.
    #[test]
    #[should_panic(expected = "num_bins.is_power_of_two()")]
    fn test_num_bins_not_power_of_two_should_panic() {
        let num_bins = NonZeroUsize::new(3).unwrap();
        PubkeyBinCalculatorBuilder::with_bins(num_bins);
    }
}
