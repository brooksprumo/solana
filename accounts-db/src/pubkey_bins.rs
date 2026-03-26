use {
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{fmt, hash::BuildHasher as _, num::NonZeroUsize},
};

/// Used to calculate which bin a pubkey maps to.
///
/// This struct may be cloned, and will retain the same pubkey -> bin results.
///
/// To instantiate, use `PubkeyBinCalculatorBuilder::build(num_bins)`.
#[derive(Clone)]
pub struct PubkeyBinCalculator {
    mask: u64,
    hasher_builder: PubkeyHasherBuilder,
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
        self.hasher_builder.hash_one(pubkey)
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
    /// Builds a `PubkeyBinCalculator`.
    ///
    /// The returned bin calculator will produce *unique* mappings
    /// compared to other bin calculators!
    ///
    /// # Panics
    ///
    /// This function will panic if the following conditions are not met:
    /// * `num_bins` must be a power of two
    pub fn build(num_bins: NonZeroUsize) -> PubkeyBinCalculator {
        assert!(num_bins.is_power_of_two());
        let num_bins_mask = num_bins.get() - 1;
        PubkeyBinCalculator {
            mask: num_bins_mask as u64,
            hasher_builder: PubkeyHasherBuilder::default(),
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
                PubkeyBinCalculatorBuilder::build(NonZeroUsize::new(num_bins).unwrap());
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
    fn test_builder_produces_unique_instances() {
        let num_bins = NonZeroUsize::new(1).unwrap();
        let bin_calculator1 = PubkeyBinCalculatorBuilder::build(num_bins);
        let bin_calculator2 = PubkeyBinCalculatorBuilder::build(num_bins);
        let pubkey = Pubkey::new_unique();
        assert_ne!(
            bin_calculator1.hash_from_pubkey(&pubkey),
            bin_calculator2.hash_from_pubkey(&pubkey),
        );
    }

    /// Ensure non-power-of-two number of bins is not allowed.
    #[test]
    #[should_panic(expected = "num_bins.is_power_of_two()")]
    fn test_num_bins_not_power_of_two() {
        let num_bins = NonZeroUsize::new(3).unwrap();
        PubkeyBinCalculatorBuilder::build(num_bins);
    }
}
