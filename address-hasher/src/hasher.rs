#![allow(clippy::arithmetic_side_effects)]
use {
    rand::Rng as _,
    std::{
        cell::Cell,
        hash::{BuildHasher, Hasher},
        thread_local,
    },
};

/// Number of bytes in an address.
///
/// Mirrors `solana_pubkey::PUBKEY_BYTES` so `solana_pubkey` stays a dev-only dependency.
/// (constant is kept in sync by asserts in tests)
const ADDRESS_BYTES: usize = 32;

/// A faster, but less collision resistant hasher for addresses.
///
/// Specialized hasher that uses a random 8 bytes subslice of the
/// address as the hash value. Should not be used when collisions
/// might be used to mount DOS attacks.
pub struct AddressHasher {
    random: u64,
    offset: usize,
    state: u64,
}

impl Hasher for AddressHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.state
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        debug_assert_eq!(
            bytes.len(),
            ADDRESS_BYTES,
            "This hasher is intended to be used with addresses and nothing else",
        );
        // This slice/unwrap can never panic since offset is < ADDRESS_BYTES - size_of::<u64>()
        let chunk: &[u8; size_of::<u64>()] = bytes[self.offset..self.offset + size_of::<u64>()]
            .try_into()
            .unwrap();
        self.state = u64::from_ne_bytes(*chunk) ^ self.random;
    }
}

/// A builder for faster, but less collision resistant hasher for addresses.
///
/// Initializes `AddressHasher` instances that use an 8-byte
/// slice of the address as the hash value. Should not be used when
/// collisions might be used to mount DOS attacks.
#[derive(Clone)]
pub struct AddressHasherBuilder {
    random: u64,
    offset: usize,
}

impl AddressHasherBuilder {
    /// Constructs a builder with a specific random and offset.
    ///
    /// Prefer `AddressHasherBuilder::default()` unless deterministic results are required.
    pub fn with(random: u64, offset: usize) -> Self {
        AddressHasherBuilder { random, offset }
    }

    /// Returns the random used to construct this builder.
    pub fn random(&self) -> u64 {
        self.random
    }

    /// Returns the offset used to construct this builder.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl Default for AddressHasherBuilder {
    /// Default construct the AddressHasherBuilder.
    ///
    /// The position of the slice is determined initially
    /// through random draw and then by incrementing a thread-local
    /// This way each hashmap can be expected to use a slightly different
    /// slice. This is essentially the same mechanism as what is used by
    /// `RandomState`
    fn default() -> Self {
        thread_local! {
            static OFFSET: Cell<usize>  = {
                Cell::new(rand::rng().random_range(0..ADDRESS_BYTES - size_of::<u64>()))
            };
        }

        let random = rand::rng().random();
        let offset = OFFSET.with(|offset| {
            let mut next_offset = offset.get() + 1;
            if next_offset > ADDRESS_BYTES - size_of::<u64>() {
                next_offset = 0;
            }
            offset.set(next_offset);
            next_offset
        });
        AddressHasherBuilder { random, offset }
    }
}

impl BuildHasher for AddressHasherBuilder {
    type Hasher = AddressHasher;
    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        AddressHasher {
            random: self.random,
            offset: self.offset,
            state: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_pubkey::{PUBKEY_BYTES, Pubkey},
    };

    /// Ensure our ADDRESS_BYTES constant stays in sync with the solana-sdk
    #[test]
    fn test_address_bytes() {
        assert_eq!(ADDRESS_BYTES, PUBKEY_BYTES);
        assert_eq!(ADDRESS_BYTES, Pubkey::default().as_array().len());
        assert_eq!(ADDRESS_BYTES, Pubkey::default().as_ref().len());
    }

    /// Ensure identical hashers with identical keys produce identical hashes
    #[test]
    fn test_identical_hashers_and_identical_keys() {
        let mut hasher1 = AddressHasher {
            random: 42,
            offset: 7,
            state: 0,
        };
        let mut hasher2 = AddressHasher {
            random: 42,
            offset: 7,
            state: 0,
        };

        let key = Pubkey::new_unique();
        hasher1.write(key.as_array());
        hasher2.write(key.as_array());
        assert_eq!(hasher1.finish(), hasher2.finish());
    }

    /// Ensure identical hashers with different keys produce different hashes
    #[test]
    fn test_identical_hashers_and_different_keys() {
        let mut hasher1 = AddressHasher {
            random: 11,
            offset: 2,
            state: 0,
        };
        let mut hasher2 = AddressHasher {
            random: 11,
            offset: 2,
            state: 0,
        };

        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        hasher1.write(key1.as_array());
        hasher2.write(key2.as_array());
        assert_ne!(hasher1.finish(), hasher2.finish());
    }

    /// Ensure different hashers with identical keys produce different hashes
    #[test]
    fn test_different_hashers_and_identical_keys() {
        let mut hasher1 = AddressHasher {
            random: 123,
            offset: 3,
            state: 0,
        };
        let mut hasher2 = AddressHasher {
            random: 456,
            offset: 4,
            state: 0,
        };

        let key = Pubkey::new_unique();
        hasher1.write(key.as_array());
        hasher2.write(key.as_array());
        assert_ne!(hasher1.finish(), hasher2.finish());
    }

    /// Ensure different hashers with different keys produce different hashes
    #[test]
    fn test_different_hashers_and_different_keys() {
        let mut hasher1 = AddressHasher {
            random: 321,
            offset: 20,
            state: 0,
        };
        let mut hasher2 = AddressHasher {
            random: 987,
            offset: 10,
            state: 0,
        };

        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        hasher1.write(key1.as_array());
        hasher2.write(key2.as_array());
        assert_ne!(hasher1.finish(), hasher2.finish());
    }

    /// Ensure different builders are different
    #[test]
    fn test_builder_default() {
        let builder1 = AddressHasherBuilder::default();
        let builder2 = AddressHasherBuilder::default();

        assert_ne!(builder1.random(), builder2.random());
        assert_ne!(builder1.offset(), builder2.offset());
    }

    /// Ensure AddressHasherBuilder::with() is deterministic
    #[test]
    fn test_builder_with() {
        let builder1 = AddressHasherBuilder::default();
        let random1 = builder1.random();
        let offset1 = builder1.offset();
        let builder2 = AddressHasherBuilder::with(random1, offset1);

        assert_eq!(random1, builder2.random());
        assert_eq!(offset1, builder2.offset());
    }

    /// Ensure build_hasher() builds stable hashers
    #[test]
    fn test_build_hasher() {
        let builder = AddressHasherBuilder::default();
        let hasher1 = builder.build_hasher();
        let hasher2 = builder.build_hasher();

        assert_eq!(hasher1.random, hasher2.random);
        assert_eq!(hasher1.offset, hasher2.offset);
    }
}
