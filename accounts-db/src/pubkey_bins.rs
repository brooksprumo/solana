use {
    rand::{Rng, rng},
    solana_pubkey::{PUBKEY_BYTES, Pubkey},
    std::{fmt, num::NonZeroUsize},
};

const BITS_PER_BYTE: usize = u8::BITS as usize;
const MAX_OFFSET_BYTES: usize = PUBKEY_BYTES - size_of::<u32>(); // ensure we have enough to read
const MAX_OFFSET_BITS: usize = MAX_OFFSET_BYTES * BITS_PER_BYTE;

/// The maximum number of bins we can support.
///
/// This is based on the number of bytes we read in `read_bytes()`.
///
/// Basically, if we read four bytes (32 bits) from the pubkey as it's "hash", and can have a maximum bit-offset of seven,
/// then the maximum number of bins, as pow2, is 32 - 7 == 25.
///
/// To get the real number, do `pow2(MAX_BINS_POW2)`.
const MAX_BINS_POW2: usize = (size_of::<u32>() - 1) * BITS_PER_BYTE + 1;

/// Used to calculate which bin a pubkey maps to.
///
/// This struct may be cloned, and will retain the same pubkey -> bin results.
///
/// To instantiate, use `PubkeyBinCalculatorBuilder::with_bins(num_bins)`.
#[derive(Clone)]
pub struct PubkeyBinCalculator {
    mask: u32,
    byte_offset: usize,
    bit_offset: usize,
}

impl PubkeyBinCalculator {
    /// Calculates the bin that `pubkey` maps to.
    #[inline]
    pub fn bin_from_pubkey(&self, pubkey: &Pubkey) -> usize {
        let bytes = self.read_bytes(pubkey);
        ((bytes >> self.bit_offset) & self.mask) as usize
    }

    /// Read the bytes from `pubkey` needed to calculate the bin.
    #[inline]
    fn read_bytes(&self, pubkey: &Pubkey) -> u32 {
        debug_assert!(self.byte_offset <= MAX_OFFSET_BYTES);
        let ptr = pubkey.as_array().as_ptr();
        // SAFETY:
        //
        // - `byte_offset` was checked at build time to be in range to read a u32.
        //
        // add() is safe:
        // - `byte_offset` can fit in an isize.
        // - `byte_offset` is in-range of `pubkey`.
        //
        // read_unaligned() is safe:
        // - the ptr being read is valid
        //   - the ptr came from `pubkey`.
        //   - the memory range being read is entirely contained within the
        //     bounds of the allocation (this was checked above by `add()`).
        // - the value of the type being read (u32) is valid
        //   - the memory of `pubkey` has been initialized
        unsafe { ptr.add(self.byte_offset).cast::<u32>().read_unaligned() }
    }

    fn offset(&self) -> usize {
        self.byte_offset * BITS_PER_BYTE + self.bit_offset
    }
}

impl fmt::Debug for PubkeyBinCalculator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkeyBinCalculator")
            .field("num_bins", &(self.mask + 1))
            .field("offset", &self.offset())
            .finish()
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
    /// * `num_bins` must be <= 2^25
    pub fn with_bins(num_bins: NonZeroUsize) -> PubkeyBinCalculator {
        // skip pathological collisions at the beginning and end of the pubkey range
        let offset = rng().random_range(16..=(MAX_OFFSET_BITS - 16));
        Self::with_bins_and_offset(num_bins, offset)
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
    /// * `num_bins` must be <= 2^25
    /// * `offset` must be <= 224
    pub fn with_bins_and_offset(num_bins: NonZeroUsize, offset: usize) -> PubkeyBinCalculator {
        assert!(
            offset <= MAX_OFFSET_BITS,
            "offset must be <= 224 (actual: {offset})",
        );
        assert!(
            num_bins.is_power_of_two(),
            "num_bins must be a power of two (actual: {num_bins})",
        );
        assert!(
            num_bins.get() <= (1 << MAX_BINS_POW2),
            "num_bins must be <= 2^25 (actual: {num_bins})",
        );
        let num_bins_mask = num_bins.get() - 1;
        let byte_offset = offset / BITS_PER_BYTE;
        let bit_offset = offset - (byte_offset * BITS_PER_BYTE);
        PubkeyBinCalculator {
            mask: num_bins_mask as u32,
            byte_offset,
            bit_offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensure that bin calculation is deterministic.
    #[test]
    fn test_bin_from_pubkey_is_deterministic() {
        for num_bins in [1 << 10, 1 << 14, 1 << 19, 1 << MAX_BINS_POW2] {
            let bin_calculator1 =
                PubkeyBinCalculatorBuilder::with_bins(NonZeroUsize::new(num_bins).unwrap());
            // second bin calculator that exercies Calculator::clone()
            let bin_calculator2 = bin_calculator1.clone();
            for i_pubkey in 0..1_000 {
                let pubkey = solana_pubkey::new_rand();
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
        let bin_calculator2 = loop {
            let bc2 = PubkeyBinCalculatorBuilder::with_bins(num_bins);
            if bc2.byte_offset != bin_calculator1.byte_offset {
                break bc2;
            }
        };
        let pubkey = solana_pubkey::new_rand();
        assert_ne!(
            bin_calculator1.read_bytes(&pubkey),
            bin_calculator2.read_bytes(&pubkey),
        );
    }

    /// Ensure that bin calculators from different builders, but with the
    /// same num_bins and offset, produce *identical* hashes.
    #[test]
    fn test_builders_with_same_offset_produce_identical_instances() {
        let num_bins = NonZeroUsize::new(1 << 20).unwrap();
        let offset = 123;
        let bin_calculator1 = PubkeyBinCalculatorBuilder::with_bins_and_offset(num_bins, offset);
        let bin_calculator2 = PubkeyBinCalculatorBuilder::with_bins_and_offset(num_bins, offset);
        assert_eq!(bin_calculator1.offset(), bin_calculator2.offset());
        let pubkey = solana_pubkey::new_rand();
        assert_eq!(
            bin_calculator1.read_bytes(&pubkey),
            bin_calculator2.read_bytes(&pubkey),
        );
        assert_eq!(
            bin_calculator1.bin_from_pubkey(&pubkey),
            bin_calculator2.bin_from_pubkey(&pubkey),
        );
    }

    /// Ensure non-power-of-two number of bins is not allowed.
    #[test]
    #[should_panic(expected = "num_bins must be a power of two")]
    fn test_num_bins_not_power_of_two_should_panic() {
        let num_bins = NonZeroUsize::new(3).unwrap();
        PubkeyBinCalculatorBuilder::with_bins(num_bins);
    }

    /// Ensure number of bins is in range.
    #[test]
    #[should_panic(expected = "num_bins must be <= 2^25")]
    fn test_num_bins_too_large_should_panic() {
        let num_bins = NonZeroUsize::new(1 << (MAX_BINS_POW2 + 1)).unwrap();
        PubkeyBinCalculatorBuilder::with_bins(num_bins);
    }

    /// Ensure offset is in range.
    #[test]
    #[should_panic(expected = "offset must be <= 224")]
    fn test_bad_offset_should_panic() {
        let num_bins = NonZeroUsize::new(1).unwrap();
        PubkeyBinCalculatorBuilder::with_bins_and_offset(num_bins, MAX_OFFSET_BITS + 1);
    }
}
