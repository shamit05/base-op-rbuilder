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
//! - **Middle**: AA bundles are inserted here (configurable gas threshold ~50%)
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
//! - `aa_gas_threshold`: Percentage of block gas at which to start considering bundles (default: 50%)
//! - `aa_gas_reserve_percentage`: Percentage of block gas allocated for bundles (default: 20%)
//!
//! ## Pool Client
//!
//! The `PoolClient` trait defines the interface for fetching UserOperations from the mempool.
//! See `pool_client` module for details.

mod bundle;
mod bundler;
mod gas_tracker;
mod pool_client;

pub use bundle::{
    BundleBuilder, BundleConfig, BundleTransaction, PackedUserOperation, UserOpGasInfo,
    UserOperation, ENTRYPOINT_BUFFER_GAS, MAX_BUNDLE_GAS,
};
pub use bundler::{entry_points, BundleResult, Bundler};
pub use gas_tracker::{GasReservation, GasTracker};
pub use pool_client::{
    NoOpPoolClient, OperationGasInfo, PoolClient, PoolClientError, PoolOperation, UserOpHash,
    UserOperationV06, UserOperationV07, UserOperationVariant,
};

