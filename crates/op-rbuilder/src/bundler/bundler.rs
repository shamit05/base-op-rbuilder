//! Bundler orchestration - ties together pool client, gas tracking, and bundle building
//!
//! This is the main entry point for AA bundling during block building.

use std::sync::Arc;

use alloy_primitives::{Address, FixedBytes};

use super::{
    bundle::{BundleBuilder, BundleTransaction, PackedUserOperation, UserOperation},
    gas_tracker::GasTracker,
    pool_client::{PoolClient, PoolOperation, UserOperationVariant},
    ENTRYPOINT_BUFFER_GAS,
};
use crate::tx_signer::Signer;

/// Known EntryPoint addresses
pub mod entry_points {
    use alloy_primitives::{address, Address};

    /// EntryPoint v0.6 address
    pub const V06: Address = address!("5FF137D4b0FDCD49DcA30c7CF57E578a026d2789");
    /// EntryPoint v0.7 address
    pub const V07: Address = address!("0000000071727De22E5E9d8BAf0edAc6f37da032");
}

/// The main bundler that orchestrates AA bundle creation
pub struct Bundler<P: PoolClient> {
    /// Pool client for fetching operations
    pool_client: P,
    /// Bundle builder for creating transactions
    bundle_builder: BundleBuilder,
    /// Gas tracker for reservation logic
    gas_tracker: GasTracker,
    /// Whether bundling is enabled
    enabled: bool,
}

/// Result of bundle building
#[derive(Debug)]
pub struct BundleResult {
    /// Bundle transactions to include (one per entry point)
    pub bundles: Vec<BundleTransaction>,
    /// Total gas used by all bundles
    pub total_gas: u64,
    /// Number of operations included
    pub total_ops: usize,
}

impl<P: PoolClient> Bundler<P> {
    /// Create a new bundler
    ///
    /// # Arguments
    /// * `pool_client` - Client for fetching operations from mempool
    /// * `signer` - Signer for bundle transactions
    /// * `block_gas_limit` - Total gas limit for the block
    /// * `threshold_percentage` - Gas percentage at which to build bundles (e.g., 50)
    /// * `reserve_percentage` - Gas percentage reserved for bundles (e.g., 20)
    pub fn new(
        pool_client: P,
        signer: Signer,
        block_gas_limit: u64,
        threshold_percentage: u8,
        reserve_percentage: u8,
    ) -> Self {
        let beneficiary = signer.address;
        let bundle_builder = BundleBuilder::new(
            signer,
            beneficiary,
            vec![entry_points::V06, entry_points::V07],
        );
        let gas_tracker = GasTracker::new(block_gas_limit, threshold_percentage, reserve_percentage);

        Self {
            pool_client,
            bundle_builder,
            gas_tracker,
            enabled: true,
        }
    }

    /// Create a disabled bundler (no-op)
    pub fn disabled(pool_client: P) -> Self {
        Self {
            pool_client,
            bundle_builder: BundleBuilder::new(
                Signer::random(),
                Address::ZERO,
                vec![],
            ),
            gas_tracker: GasTracker::new(0, 0, 0),
            enabled: false,
        }
    }

    /// Check if bundling should happen based on current gas usage
    pub fn should_build_bundles(&self, cumulative_gas_used: u64) -> bool {
        if !self.enabled {
            return false;
        }
        let reservation = self.gas_tracker.check_reservation(cumulative_gas_used);
        reservation.threshold_reached
    }

    /// Build bundles from the mempool
    ///
    /// Called when the gas threshold is reached during block building.
    /// Returns bundle transactions ready for inclusion.
    ///
    /// # Arguments
    /// * `base_fee` - Current block base fee
    /// * `priority_fee` - Priority fee to use for bundle transactions
    pub fn build_bundles(&self, base_fee: u128, priority_fee: u128) -> BundleResult {
        if !self.enabled || !self.pool_client.is_ready() {
            return BundleResult {
                bundles: vec![],
                total_gas: 0,
                total_ops: 0,
            };
        }

        // Fetch top operations from pool (already sorted by priority)
        let reserved_gas = self.gas_tracker.calculate_reserved_gas();
        let max_ops = self.estimate_max_ops(reserved_gas);
        let operations = self.pool_client.get_top_operations(max_ops);

        if operations.is_empty() {
            return BundleResult {
                bundles: vec![],
                total_gas: 0,
                total_ops: 0,
            };
        }

        // Group operations by entry point
        let (v06_ops, v07_ops) = self.group_by_entry_point(&operations);

        let mut bundles = Vec::new();
        let mut total_gas = 0u64;
        let mut total_ops = 0usize;

        // Build v0.6 bundle if there are operations
        if !v06_ops.is_empty() {
            if let Some(bundle) = self.build_v06_bundle(&v06_ops, base_fee, priority_fee, reserved_gas) {
                total_gas = total_gas.saturating_add(bundle.gas_limit);
                total_ops += bundle.num_ops;
                bundles.push(bundle);
            }
        }

        // Build v0.7 bundle if there are operations (with remaining gas)
        let remaining_gas = reserved_gas.saturating_sub(total_gas);
        if !v07_ops.is_empty() && remaining_gas > ENTRYPOINT_BUFFER_GAS {
            if let Some(bundle) = self.build_v07_bundle(&v07_ops, base_fee, priority_fee, remaining_gas) {
                total_gas = total_gas.saturating_add(bundle.gas_limit);
                total_ops += bundle.num_ops;
                bundles.push(bundle);
            }
        }

        BundleResult {
            bundles,
            total_gas,
            total_ops,
        }
    }

    /// Remove operations that were included in bundles
    pub fn remove_included_operations(&self, bundles: &[BundleTransaction]) {
        for bundle in bundles {
            for hash in &bundle.op_hashes {
                let _ = self.pool_client.remove_operation(hash);
            }
        }
    }

    /// Estimate maximum number of operations that can fit in reserved gas
    fn estimate_max_ops(&self, reserved_gas: u64) -> usize {
        // Rough estimate: 150k gas per operation on average
        const AVG_GAS_PER_OP: u64 = 150_000;
        let usable_gas = reserved_gas.saturating_sub(ENTRYPOINT_BUFFER_GAS);
        (usable_gas / AVG_GAS_PER_OP) as usize
    }

    /// Group operations by entry point version
    fn group_by_entry_point(
        &self,
        operations: &[Arc<PoolOperation>],
    ) -> (Vec<Arc<PoolOperation>>, Vec<Arc<PoolOperation>>) {
        let mut v06_ops = Vec::new();
        let mut v07_ops = Vec::new();

        for op in operations {
            if op.entry_point == entry_points::V06 {
                v06_ops.push(op.clone());
            } else if op.entry_point == entry_points::V07 {
                v07_ops.push(op.clone());
            }
            // Unknown entry points are ignored
        }

        (v06_ops, v07_ops)
    }

    /// Build a v0.6 bundle from operations
    fn build_v06_bundle(
        &self,
        operations: &[Arc<PoolOperation>],
        base_fee: u128,
        priority_fee: u128,
        max_gas: u64,
    ) -> Option<BundleTransaction> {
        let mut ops = Vec::new();
        let mut hashes = Vec::new();
        let mut cumulative_gas = 21_000u64 + ENTRYPOINT_BUFFER_GAS;

        for pool_op in operations {
            let op_gas = pool_op.gas_info.total_gas();
            if cumulative_gas.saturating_add(op_gas) > max_gas {
                break;
            }

            if let UserOperationVariant::V06(v06) = &pool_op.operation {
                ops.push(convert_to_v06_sol(v06));
                hashes.push(pool_op.hash);
                cumulative_gas = cumulative_gas.saturating_add(op_gas);
            }
        }

        if ops.is_empty() {
            return None;
        }

        Some(self.bundle_builder.create_bundle_v06(
            entry_points::V06,
            ops,
            hashes,
            cumulative_gas,
            base_fee + priority_fee,
            priority_fee,
        ))
    }

    /// Build a v0.7 bundle from operations
    fn build_v07_bundle(
        &self,
        operations: &[Arc<PoolOperation>],
        base_fee: u128,
        priority_fee: u128,
        max_gas: u64,
    ) -> Option<BundleTransaction> {
        let mut ops = Vec::new();
        let mut hashes = Vec::new();
        let mut cumulative_gas = 21_000u64 + ENTRYPOINT_BUFFER_GAS;

        for pool_op in operations {
            let op_gas = pool_op.gas_info.total_gas();
            if cumulative_gas.saturating_add(op_gas) > max_gas {
                break;
            }

            if let UserOperationVariant::V07(v07) = &pool_op.operation {
                ops.push(convert_to_v07_sol(v07));
                hashes.push(pool_op.hash);
                cumulative_gas = cumulative_gas.saturating_add(op_gas);
            }
        }

        if ops.is_empty() {
            return None;
        }

        Some(self.bundle_builder.create_bundle_v07(
            entry_points::V07,
            ops,
            hashes,
            cumulative_gas,
            base_fee + priority_fee,
            priority_fee,
        ))
    }

    /// Get the gas tracker
    pub fn gas_tracker(&self) -> &GasTracker {
        &self.gas_tracker
    }

    /// Check if bundling is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Convert pool client V06 type to sol type for handleOps
fn convert_to_v06_sol(op: &super::pool_client::UserOperationV06) -> UserOperation {
    UserOperation {
        sender: op.sender,
        nonce: op.nonce,
        initCode: op.init_code.clone(),
        callData: op.call_data.clone(),
        callGasLimit: op.call_gas_limit,
        verificationGasLimit: op.verification_gas_limit,
        preVerificationGas: op.pre_verification_gas,
        maxFeePerGas: op.max_fee_per_gas,
        maxPriorityFeePerGas: op.max_priority_fee_per_gas,
        paymasterAndData: op.paymaster_and_data.clone(),
        signature: op.signature.clone(),
    }
}

/// Convert pool client V07 type to sol type for handleOps
fn convert_to_v07_sol(op: &super::pool_client::UserOperationV07) -> PackedUserOperation {
    PackedUserOperation {
        sender: op.sender,
        nonce: op.nonce,
        initCode: op.init_code.clone(),
        callData: op.call_data.clone(),
        accountGasLimits: FixedBytes::from(op.account_gas_limits),
        preVerificationGas: op.pre_verification_gas,
        gasFees: FixedBytes::from(op.gas_fees),
        paymasterAndData: op.paymaster_and_data.clone(),
        signature: op.signature.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundler::pool_client::NoOpPoolClient;

    #[test]
    fn test_bundler_disabled() {
        let bundler = Bundler::disabled(NoOpPoolClient);
        
        assert!(!bundler.is_enabled());
        assert!(!bundler.should_build_bundles(1_000_000));
        
        let result = bundler.build_bundles(1_000_000_000, 100_000_000);
        assert!(result.bundles.is_empty());
        assert_eq!(result.total_gas, 0);
        assert_eq!(result.total_ops, 0);
    }

    #[test]
    fn test_bundler_threshold_check() {
        let bundler = Bundler::new(
            NoOpPoolClient,
            Signer::random(),
            30_000_000, // 30M gas limit
            50,         // 50% threshold
            20,         // 20% reserve
        );

        // Below threshold (50% of 30M = 15M)
        assert!(!bundler.should_build_bundles(10_000_000));
        
        // At threshold
        assert!(bundler.should_build_bundles(15_000_000));
        
        // Above threshold
        assert!(bundler.should_build_bundles(20_000_000));
    }

    #[test]
    fn test_estimate_max_ops() {
        let bundler = Bundler::new(
            NoOpPoolClient,
            Signer::random(),
            30_000_000,
            50,
            20,
        );

        // 20% of 30M = 6M reserved
        // 6M - 100k buffer = 5.9M usable
        // 5.9M / 150k per op ≈ 39 ops
        let max_ops = bundler.estimate_max_ops(6_000_000);
        assert_eq!(max_ops, 39);
    }
}

