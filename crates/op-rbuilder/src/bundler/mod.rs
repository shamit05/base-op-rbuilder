//! Account Abstraction Native Bundler
//!
//! This module implements gas reservation and bundle creation for ERC-4337
//! UserOperations within the block building flow.
//!
//! ## Overview
//!
//! The native bundler integrates with the payload builder to:
//! 1. Process EOA transactions first (top of block reserved for EOA bidders)
//! 2. At a configurable gas threshold, create AA bundles in the "middle" of the block
//! 3. Continue with remaining EOA transactions after bundles
//!
//! ## Bundle Placement Strategy
//!
//! Bundles are placed in the **middle** of the block, not at the top or end:
//! - **Top**: Reserved for EOA bidders (until we get tx independence in AA)
//! - **Middle**: AA bundles are inserted here (configurable gas threshold)
//! - **End**: Avoided because large bundles might not fit
//!
//! ## V0 Bundle Building Strategy
//!
//! Optimistically build the biggest bundle where:
//! `Sum(validationGasLimit) + Sum(executionGasLimit) + entrypointBuffer < MAX_BUNDLE_GAS (21M)`
//!
//! If any ops fail during execution, prune them out.
//!
//! ## Gas Allocation
//!
//! - `aa_gas_threshold`: Percentage of block gas at which to start considering bundles (e.g., 50%)
//! - `aa_gas_reserve_percentage`: Percentage of block gas allocated for bundles (e.g., 20%)

mod gas_tracker;
mod bundle;

pub use gas_tracker::{GasReservation, GasTracker};
pub use bundle::{
    BundleBuilder, BundleConfig, BundleTransaction, 
    PackedUserOperation, UserOperation, UserOpGasInfo,
    MAX_BUNDLE_GAS, ENTRYPOINT_BUFFER_GAS,
};

