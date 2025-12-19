//! Bundler orchestration - ties together mempool, gas tracking, and bundle building
//!
//! This is the main entry point for AA bundling during block building.

use std::sync::Arc;

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use tokio::sync::RwLock;

use super::{
    bundle::{
        BundleBuilder, BundleTransaction, IEntryPointV06, IEntryPointV07, PackedUserOperation,
        UserOpGasInfo, UserOperation as SolUserOperation,
    },
    gas_tracker::GasTracker
};
use crate::tx_signer::Signer;
use account_abstraction_core::{PoolConfig, domain::WrappedUserOperation};
use account_abstraction_core::domain::Mempool;
use account_abstraction_core::InMemoryMempool;

/// Known EntryPoint addresses
pub mod entry_points {
    use alloy_primitives::{address, Address};

    /// EntryPoint v0.6 address
    pub const V06: Address = address!("5FF137D4b0FDCD49DcA30c7CF57E578a026d2789");
    /// EntryPoint v0.7 address
    pub const V07: Address = address!("0000000071727De22E5E9d8BAf0edAc6f37da032");
}

pub type SharedMempool = Arc<RwLock<dyn Mempool>>;

/// The main bundler that orchestrates AA bundle creation
pub struct Bundler {
    /// Shared mempool for fetching operations
    mempool: SharedMempool,
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

impl Bundler {
    /// Create a new bundler with a shared mempool
    ///
    /// # Arguments
    /// * `mempool` - Shared mempool for fetching operations
    /// * `signer` - Signer for bundle transactions
    /// * `block_gas_limit` - Total gas limit for the block
    /// * `threshold_percentage` - Gas percentage at which to build bundles (e.g., 50)
    /// * `reserve_percentage` - Gas percentage reserved for bundles (e.g., 20)
    pub fn new(
        mempool: Arc<RwLock<dyn Mempool>>,
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
        let gas_tracker =
            GasTracker::new(block_gas_limit, threshold_percentage, reserve_percentage);

        Self {
            mempool,
            bundle_builder,
            gas_tracker,
            enabled: true,
        }
    }

    /// Create a disabled bundler (for when AA bundling is off)
    pub fn disabled() -> Self {
        // Create an empty mempool
        let empty_mempool = Arc::new(RwLock::new(InMemoryMempool::new(PoolConfig::default())));
        let signer = Signer::random();
        
        Self {
            mempool: empty_mempool,
            bundle_builder: BundleBuilder::new(signer, signer.address, vec![]),
            gas_tracker: GasTracker::new(30_000_000, 50, 20),
            enabled: false,
        }
    }

    /// Check if bundler is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Check if we should build bundles at the current gas usage
    pub async fn should_build_bundles(&self, cumulative_gas_used: u64) -> bool {
        if !self.enabled {
            return false;
        }
        
        // Check if mempool has any operations
        let pool = self.mempool.read().await;
        if pool.get_top_operations(1).is_empty() {
            return false;
        }
        drop(pool);
        
        self.gas_tracker
            .check_reservation(cumulative_gas_used)
            .threshold_reached
    }

    /// Build bundle transactions from the mempool
    ///
    /// Returns bundle transactions ready for execution
    pub async fn build_bundles(&self, base_fee: u128, priority_fee: u128) -> BundleResult {
        if !self.enabled {
            return BundleResult {
                bundles: vec![],
                total_gas: 0,
                total_ops: 0,
            };
        }

        let reserved_gas = self.gas_tracker.calculate_reserved_gas();
        let max_ops = self.estimate_max_ops(reserved_gas);

        // Fetch operations from mempool
        let operations: Vec<Arc<WrappedUserOperation>> = {
            let pool = self.mempool.read().await;
            pool.get_top_operations(max_ops)
        };

        if operations.is_empty() {
            return BundleResult {
                bundles: vec![],
                total_gas: 0,
                total_ops: 0,
            };
        }

        // Group operations by EntryPoint version
        let (v06_ops, v07_ops) = self.group_by_entry_point(&operations);

        let mut bundles = Vec::new();
        let mut total_gas = 0u64;
        let mut total_ops = 0usize;

        // Build v0.6 bundle
        if !v06_ops.is_empty() {
            if let Some(bundle) = self.build_v06_bundle(&v06_ops, base_fee, priority_fee) {
                total_gas += bundle.gas_limit;
                total_ops += bundle.num_ops;
                bundles.push(bundle);
            }
        }

        // Build v0.7 bundle
        if !v07_ops.is_empty() {
            if let Some(bundle) = self.build_v07_bundle(&v07_ops, base_fee, priority_fee) {
                total_gas += bundle.gas_limit;
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

    /// Remove operations that were included in bundles from the mempool
    pub async fn remove_included_operations(&self, bundles: &[BundleTransaction]) {
        use alloy_primitives::B256;
        let mut pool = self.mempool.write().await;
        for bundle in bundles {
            for hash in &bundle.op_hashes {
                let _ = pool.remove_operation(&B256::from(*hash));
            }
        }
    }

    /// Estimate maximum number of operations we can include
    fn estimate_max_ops(&self, reserved_gas: u64) -> usize {
        // Assume average ~150k gas per operation
        const AVG_GAS_PER_OP: u64 = 150_000;
        ((reserved_gas / AVG_GAS_PER_OP) as usize).max(1)
    }

    /// Group operations by EntryPoint version
    fn group_by_entry_point(
        &self,
        operations: &[Arc<WrappedUserOperation>],
    ) -> (Vec<Arc<WrappedUserOperation>>, Vec<Arc<WrappedUserOperation>>) {
        let mut v06_ops = Vec::new();
        let mut v07_ops = Vec::new();

        for op in operations {
            // Check the operation type to determine version
            match &op.operation {
                account_abstraction_core::domain::VersionedUserOperation::UserOperation(_) => {
                    v06_ops.push(Arc::clone(op));
                }
                account_abstraction_core::domain::VersionedUserOperation::PackedUserOperation(_) => {
                    v07_ops.push(Arc::clone(op));
                }
            }
        }

        (v06_ops, v07_ops)
    }

    /// Build a v0.6 bundle transaction
    fn build_v06_bundle(
        &self,
        operations: &[Arc<WrappedUserOperation>],
        base_fee: u128,
        priority_fee: u128,
    ) -> Option<BundleTransaction> {
        if operations.is_empty() {
            return None;
        }

        // Extract UserOperations and gas info
        let mut sol_ops = Vec::new();
        let mut gas_infos = Vec::new();
        let mut op_hashes = Vec::new();

        for op in operations {
            if let account_abstraction_core::domain::VersionedUserOperation::UserOperation(
                ref user_op,
            ) = op.operation
            {
                // Convert from alloy_rpc_types_eth::UserOperation to our sol! type
                let sol_op = convert_v06_user_op(user_op);
                let gas_info = UserOpGasInfo::from_unpacked(&sol_op);
                gas_infos.push(gas_info);
                sol_ops.push(sol_op);
                op_hashes.push(op.hash.0);
            }
        }

        if sol_ops.is_empty() {
            return None;
        }

        // Calculate gas limit
        let gas_limit = BundleBuilder::calculate_bundle_gas_limit(&gas_infos);

        // Build calldata using handleOps
        let call = IEntryPointV06::handleOpsCall {
            ops: sol_ops.clone(),
            beneficiary: self.bundle_builder.beneficiary(),
        };
        let calldata = Bytes::from(call.abi_encode());

        Some(BundleTransaction {
            entry_point: entry_points::V06,
            calldata,
            gas_limit,
            max_fee_per_gas: base_fee + priority_fee,
            max_priority_fee_per_gas: priority_fee,
            num_ops: sol_ops.len(),
            op_hashes,
        })
    }

    /// Build a v0.7 bundle transaction
    fn build_v07_bundle(
        &self,
        operations: &[Arc<WrappedUserOperation>],
        base_fee: u128,
        priority_fee: u128,
    ) -> Option<BundleTransaction> {
        if operations.is_empty() {
            return None;
        }

        // Extract PackedUserOperations and gas info
        let mut sol_ops = Vec::new();
        let mut gas_infos = Vec::new();
        let mut op_hashes = Vec::new();

        for op in operations {
            if let account_abstraction_core::domain::VersionedUserOperation::PackedUserOperation(
                ref packed_op,
            ) = op.operation
            {
                // Convert from alloy_rpc_types_eth::PackedUserOperation to our sol! type
                let sol_op = convert_v07_packed_op(packed_op);
                let gas_info = UserOpGasInfo::from_packed(&sol_op);
                gas_infos.push(gas_info);
                sol_ops.push(sol_op);
                op_hashes.push(op.hash.0);
            }
        }

        if sol_ops.is_empty() {
            return None;
        }

        // Calculate gas limit
        let gas_limit = BundleBuilder::calculate_bundle_gas_limit(&gas_infos);

        // Build calldata using handleOps
        let call = IEntryPointV07::handleOpsCall {
            ops: sol_ops.clone(),
            beneficiary: self.bundle_builder.beneficiary(),
        };
        let calldata = Bytes::from(call.abi_encode());

        Some(BundleTransaction {
            entry_point: entry_points::V07,
            calldata,
            gas_limit,
            max_fee_per_gas: base_fee + priority_fee,
            max_priority_fee_per_gas: priority_fee,
            num_ops: sol_ops.len(),
            op_hashes,
        })
    }
}

/// Convert from alloy_rpc_types_eth::UserOperation to our sol! generated type
fn convert_v06_user_op(op: &alloy_rpc_types_eth::UserOperation) -> SolUserOperation {
    use alloy_primitives::U256;
    SolUserOperation {
        sender: op.sender,
        nonce: U256::from(op.nonce),
        initCode: op.init_code.clone(),
        callData: op.call_data.clone(),
        callGasLimit: U256::from(op.call_gas_limit),
        verificationGasLimit: U256::from(op.verification_gas_limit),
        preVerificationGas: U256::from(op.pre_verification_gas),
        maxFeePerGas: U256::from(op.max_fee_per_gas),
        maxPriorityFeePerGas: U256::from(op.max_priority_fee_per_gas),
        paymasterAndData: op.paymaster_and_data.clone(),
        signature: op.signature.clone(),
    }
}

/// Convert from alloy_rpc_types_eth::PackedUserOperation to our sol! generated type
fn convert_v07_packed_op(op: &alloy_rpc_types_eth::PackedUserOperation) -> PackedUserOperation {
    use alloy_primitives::{B256, U256};
    
    // v0.7 PackedUserOperation uses separate factory and paymaster fields
    // Combine factory + factory_data into initCode
    let init_code = if let Some(factory) = op.factory {
        let mut code = factory.to_vec();
        if let Some(ref factory_data) = op.factory_data {
            code.extend_from_slice(factory_data);
        }
        Bytes::from(code)
    } else {
        Bytes::new()
    };
    
    // Combine paymaster fields into paymasterAndData
    let paymaster_and_data = if let Some(paymaster) = op.paymaster {
        let mut data = paymaster.to_vec();
        let ver_gas: u128 = op
            .paymaster_verification_gas_limit
            .and_then(|v| v.try_into().ok())
            .unwrap_or(0);
        let post_op_gas: u128 = op
            .paymaster_post_op_gas_limit
            .and_then(|v| v.try_into().ok())
            .unwrap_or(0);
        data.extend_from_slice(&ver_gas.to_be_bytes());
        data.extend_from_slice(&post_op_gas.to_be_bytes());
        if let Some(ref pm_data) = op.paymaster_data {
            data.extend_from_slice(pm_data);
        }
        Bytes::from(data)
    } else {
        Bytes::new()
    };
    
    // Pack verification and call gas limits
    let mut account_gas_limits = [0u8; 32];
    let ver_gas: u128 = op.verification_gas_limit.try_into().unwrap_or(0);
    let call_gas: u128 = op.call_gas_limit.try_into().unwrap_or(0);
    account_gas_limits[0..16].copy_from_slice(&ver_gas.to_be_bytes());
    account_gas_limits[16..32].copy_from_slice(&call_gas.to_be_bytes());
    
    // Pack gas fees
    let mut gas_fees = [0u8; 32];
    let priority_fee: u128 = op.max_priority_fee_per_gas.try_into().unwrap_or(0);
    let max_fee: u128 = op.max_fee_per_gas.try_into().unwrap_or(0);
    gas_fees[0..16].copy_from_slice(&priority_fee.to_be_bytes());
    gas_fees[16..32].copy_from_slice(&max_fee.to_be_bytes());
    
    PackedUserOperation {
        sender: op.sender,
        nonce: U256::from(op.nonce),
        initCode: init_code,
        callData: op.call_data.clone(),
        accountGasLimits: B256::from(account_gas_limits),
        preVerificationGas: U256::from(op.pre_verification_gas),
        gasFees: B256::from(gas_fees),
        paymasterAndData: paymaster_and_data,
        signature: op.signature.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use tokio::sync::RwLock;
    use account_abstraction_core::domain::{VersionedUserOperation, WrappedUserOperation};

    fn create_test_mempool() -> SharedMempool {
        Arc::new(RwLock::new(InMemoryMempool::new(PoolConfig {
            minimum_max_fee_per_gas: 0,
        })))
    }

    /// Create a mock v0.6 UserOperation for testing
    fn create_mock_user_op_v06(sender: Address, nonce: u64, max_fee: u128) -> WrappedUserOperation {
        let user_op = alloy_rpc_types_eth::UserOperation {
            sender,
            nonce: U256::from(nonce),
            init_code: Bytes::new(),
            call_data: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]), // dummy calldata
            call_gas_limit: U256::from(100_000u64),
            verification_gas_limit: U256::from(100_000u64),
            pre_verification_gas: U256::from(21_000u64),
            max_fee_per_gas: U256::from(max_fee),
            max_priority_fee_per_gas: U256::from(max_fee / 10),
            paymaster_and_data: Bytes::new(),
            signature: Bytes::from(vec![0x00; 65]), // dummy signature
        };

        // Compute a simple hash (not proper ERC-4337 hash, but good enough for testing)
        let hash = B256::random();

        WrappedUserOperation {
            operation: VersionedUserOperation::UserOperation(user_op),
            hash,
        }
    }

    #[tokio::test]
    async fn test_bundler_disabled() {
        let bundler = Bundler::disabled();
        assert!(!bundler.is_enabled());
        assert!(!bundler.should_build_bundles(1_000_000).await);
    }

    #[tokio::test]
    async fn test_bundler_empty_mempool() {
        let mempool = create_test_mempool();
        let signer = Signer::random();
        let bundler = Bundler::new(mempool, signer, 30_000_000, 50, 20);

        // Should not build bundles when mempool is empty
        assert!(!bundler.should_build_bundles(20_000_000).await);
    }

    #[tokio::test]
    async fn test_bundler_threshold_check() {
        let mempool = create_test_mempool();
        let signer = Signer::random();
        let bundler = Bundler::new(mempool, signer, 30_000_000, 50, 20);

        // Without operations in mempool, should always be false
        assert!(!bundler.should_build_bundles(0).await);
        assert!(!bundler.should_build_bundles(15_000_000).await);
        assert!(!bundler.should_build_bundles(25_000_000).await);
    }

    #[test]
    fn test_estimate_max_ops() {
        let mempool = create_test_mempool();
        let signer = Signer::random();
        let bundler = Bundler::new(mempool, signer, 30_000_000, 50, 20);

        // 6M gas reserved / 150k per op = 40 ops
        assert_eq!(bundler.estimate_max_ops(6_000_000), 40);

        // Minimum of 1
        assert_eq!(bundler.estimate_max_ops(10_000), 1);
    }

    #[tokio::test]
    async fn test_build_bundles_empty() {
        let mempool = create_test_mempool();
        let signer = Signer::random();
        let bundler = Bundler::new(mempool, signer, 30_000_000, 50, 20);

        let result = bundler.build_bundles(1_000_000_000, 100_000_000).await;
        assert!(result.bundles.is_empty());
        assert_eq!(result.total_ops, 0);
    }

    // ========================================================================
    // End-to-End Tests: Mempool -> Bundle Creation
    // ========================================================================

    #[tokio::test]
    async fn test_e2e_single_user_op_creates_bundle() {
        // Create mempool and add a UserOperation
        let mempool = create_test_mempool();
        let sender = Address::random();
        let user_op = create_mock_user_op_v06(sender, 0, 10_000_000_000); // 10 gwei

        {
            let mut pool = mempool.write().await;
            pool.add_operation(&user_op).expect("Failed to add operation");
        }

        // Create bundler
        let signer = Signer::random();
        let bundler = Bundler::new(mempool.clone(), signer, 30_000_000, 50, 20);

        // Verify mempool is not empty
        {
            let pool = mempool.read().await;
            assert!(!pool.get_top_operations(1).is_empty(), "Mempool should have one operation");
        }

        // At 50% threshold with 20% reserve, should build bundles when > 15M gas used
        assert!(
            bundler.should_build_bundles(16_000_000).await,
            "Should build bundles after threshold"
        );

        // Build bundles
        let base_fee = 1_000_000_000u128; // 1 gwei
        let priority_fee = 100_000_000u128; // 0.1 gwei
        let result = bundler.build_bundles(base_fee, priority_fee).await;

        // Should have created one bundle for v0.6
        assert_eq!(result.bundles.len(), 1, "Should have one bundle");
        assert_eq!(result.total_ops, 1, "Should have one op in bundle");

        let bundle = &result.bundles[0];
        assert_eq!(bundle.entry_point, entry_points::V06, "Should target v0.6 EntryPoint");
        assert_eq!(bundle.num_ops, 1, "Bundle should contain one op");
        assert!(!bundle.calldata.is_empty(), "Bundle should have calldata");
        assert!(bundle.gas_limit > 0, "Bundle should have gas limit");

        // Verify op hash is in the bundle
        assert_eq!(bundle.op_hashes.len(), 1, "Should have one op hash");
        assert_eq!(bundle.op_hashes[0], user_op.hash.0, "Op hash should match");
    }

    #[tokio::test]
    async fn test_e2e_multiple_user_ops_creates_bundle() {
        // Create mempool and add multiple UserOperations
        let mempool = create_test_mempool();
        let mut op_hashes = Vec::new();

        for i in 0..5 {
            let sender = Address::random();
            let user_op = create_mock_user_op_v06(sender, 0, 10_000_000_000 + i as u128); // varying fees
            op_hashes.push(user_op.hash);

            let mut pool = mempool.write().await;
            pool.add_operation(&user_op).expect("Failed to add operation");
        }

        // Create bundler
        let signer = Signer::random();
        let bundler = Bundler::new(mempool.clone(), signer, 30_000_000, 50, 20);

        // Build bundles
        let result = bundler.build_bundles(1_000_000_000, 100_000_000).await;

        // Should have created one bundle with all 5 ops
        assert_eq!(result.bundles.len(), 1, "Should have one bundle");
        assert_eq!(result.total_ops, 5, "Should have 5 ops in bundle");

        let bundle = &result.bundles[0];
        assert_eq!(bundle.num_ops, 5, "Bundle should contain 5 ops");
        assert_eq!(bundle.op_hashes.len(), 5, "Should have 5 op hashes");
    }

    #[tokio::test]
    async fn test_e2e_remove_operations_after_bundle() {
        // Create mempool and add UserOperations
        let mempool = create_test_mempool();
        let sender = Address::random();
        let user_op = create_mock_user_op_v06(sender, 0, 10_000_000_000);
        let _op_hash = user_op.hash;

        {
            let mut pool = mempool.write().await;
            pool.add_operation(&user_op).expect("Failed to add operation");
            assert!(!pool.get_top_operations(1).is_empty(), "Mempool should have one operation");
        }

        // Create bundler and build bundles
        let signer = Signer::random();
        let bundler = Bundler::new(mempool.clone(), signer, 30_000_000, 50, 20);
        let result = bundler.build_bundles(1_000_000_000, 100_000_000).await;

        assert_eq!(result.bundles.len(), 1, "Should have one bundle");

        // Remove included operations (simulating successful on-chain inclusion)
        bundler.remove_included_operations(&result.bundles).await;

        // Mempool should now be empty
        {
            let pool = mempool.read().await;
            assert!(pool.get_top_operations(1).is_empty(), "Mempool should be empty after removal");
        }

        // Building again should produce no bundles
        let result2 = bundler.build_bundles(1_000_000_000, 100_000_000).await;
        assert!(result2.bundles.is_empty(), "Should have no bundles after removal");
    }

    #[tokio::test]
    async fn test_e2e_bundle_has_valid_calldata() {
        // Create mempool and add a UserOperation
        let mempool = create_test_mempool();
        let sender = Address::random();
        let user_op = create_mock_user_op_v06(sender, 0, 10_000_000_000);

        {
            let mut pool = mempool.write().await;
            pool.add_operation(&user_op).expect("Failed to add operation");
        }

        // Create bundler and build bundles
        let signer = Signer::random();
        let bundler = Bundler::new(mempool, signer, 30_000_000, 50, 20);
        let result = bundler.build_bundles(1_000_000_000, 100_000_000).await;

        let bundle = &result.bundles[0];

        // Calldata should start with handleOps selector (0x1fad948c for v0.6)
        // handleOps(UserOperation[] calldata ops, address payable beneficiary)
        let calldata = &bundle.calldata;
        assert!(calldata.len() > 4, "Calldata should have at least selector");

        // The first 4 bytes are the function selector
        let selector = &calldata[0..4];
        // handleOps selector for v0.6: 0x1fad948c
        assert_eq!(
            selector,
            &[0x1f, 0xad, 0x94, 0x8c],
            "Should have handleOps selector"
        );
    }

    #[tokio::test]
    async fn test_e2e_gas_fees_in_bundle() {
        // Create mempool and add a UserOperation
        let mempool = create_test_mempool();
        let sender = Address::random();
        let user_op = create_mock_user_op_v06(sender, 0, 10_000_000_000);

        {
            let mut pool = mempool.write().await;
            pool.add_operation(&user_op).expect("Failed to add operation");
        }

        // Create bundler and build bundles with specific fees
        let signer = Signer::random();
        let bundler = Bundler::new(mempool, signer, 30_000_000, 50, 20);

        let base_fee = 2_000_000_000u128; // 2 gwei
        let priority_fee = 500_000_000u128; // 0.5 gwei
        let result = bundler.build_bundles(base_fee, priority_fee).await;

        let bundle = &result.bundles[0];

        // Verify gas fees are set correctly
        assert_eq!(
            bundle.max_fee_per_gas,
            base_fee + priority_fee,
            "Max fee should be base + priority"
        );
        assert_eq!(
            bundle.max_priority_fee_per_gas,
            priority_fee,
            "Priority fee should match"
        );
    }

    #[tokio::test]
    async fn test_e2e_priority_ordering() {
        // Create mempool and add UserOperations with different fees
        let mempool = create_test_mempool();

        // Add ops with different priority fees (mempool should order by priority)
        let senders: Vec<Address> = (0..3).map(|_| Address::random()).collect();
        
        // Low fee
        let op1 = create_mock_user_op_v06(senders[0], 0, 1_000_000_000);
        // High fee
        let op2 = create_mock_user_op_v06(senders[1], 0, 100_000_000_000);
        // Medium fee
        let op3 = create_mock_user_op_v06(senders[2], 0, 10_000_000_000);

        {
            let mut pool = mempool.write().await;
            pool.add_operation(&op1).expect("Failed to add op1");
            pool.add_operation(&op2).expect("Failed to add op2");
            pool.add_operation(&op3).expect("Failed to add op3");
        }

        // Create bundler and build bundles
        let signer = Signer::random();
        let bundler = Bundler::new(mempool, signer, 30_000_000, 50, 20);
        let result = bundler.build_bundles(1_000_000_000, 100_000_000).await;

        // Should have all 3 ops
        assert_eq!(result.total_ops, 3, "Should have 3 ops");

        // Mempool orders by max_priority_fee_per_gas descending
        // So op2 (highest) should be first
        let bundle = &result.bundles[0];
        assert_eq!(bundle.op_hashes[0], op2.hash.0, "Highest fee op should be first");
    }
}
