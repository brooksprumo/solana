#![cfg_attr(RUSTC_WITH_SPECIALIZATION, feature(min_specialization))]
#![allow(clippy::integer_arithmetic)]
#[deprecated(
    since = "1.8.0",
    note = "Please use `solana_sdk::stake::program::id` or `solana_program::stake::program::id` instead"
)]
pub use solana_sdk::stake::program::{check_id, id};
use solana_sdk::{
    feature_set::{self, FeatureSet},
    genesis_config::GenesisConfig,
    native_token::LAMPORTS_PER_SOL,
};

pub mod config;
pub mod stake_instruction;
pub mod stake_state;

pub fn add_genesis_accounts(genesis_config: &mut GenesisConfig) -> u64 {
    config::add_genesis_account(genesis_config)
}

/// The minimum amount, in lamports, that stake accounts should delegate
///
/// While it may be technically possible to delegate a smaller amount than what is returned, it is
/// not advised to do so, as you will not receive staking rewards.
pub fn get_minimum_delegation(feature_set: &FeatureSet) -> u64 {
    std::cmp::max(
        get_minimum_delegation_for_new_stakes(feature_set),
        get_minimum_delegation_for_rewards(feature_set),
    )
}

/// The minimum stake amount that can be delegated, in lamports.
/// NOTE: This is also used to calculate the minimum balance of a stake account, which is the
/// rent exempt reserve _plus_ the minimum stake delegation.
#[inline(always)]
pub fn get_minimum_delegation_for_new_stakes(feature_set: &FeatureSet) -> u64 {
    if feature_set.is_active(&feature_set::stake_raise_minimum_delegation_to_1_sol::id()) {
        LAMPORTS_PER_SOL
    } else {
        1
    }
}

/// The minimum amount, in lamports, that must be delegated in order to be considered for staking rewards
pub fn get_minimum_delegation_for_rewards(feature_set: &FeatureSet) -> u64 {
    if feature_set.is_active(&feature_set::stake_rewards_minimum_delegation_1_sol::id()) {
        LAMPORTS_PER_SOL
    } else {
        0
    }
}
