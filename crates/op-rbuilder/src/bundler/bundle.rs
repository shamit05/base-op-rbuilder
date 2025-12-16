//! Bundle transaction creation for ERC-4337 UserOperations
//!
//! ## Gas Calculation (following Rundler)
//!
//! Bundle gas limit is calculated as:
//! ```text
//! bundle_gas = SHARED_GAS + sum(per_op_gas)
//!
//! where per_op_gas = total_verification_gas_limit
//!                  + required_pre_execution_buffer
//!                  + call_gas_limit
//! ```
//!
//! The `required_pre_execution_buffer` captures EntryPoint overhead:
//! - v0.6: verification_gas_limit + 5,000
//! - v0.7: 10,000 + paymaster_post_op_gas + 1/63 of (call_gas + paymaster_post_op + 10,000)
//!
//! Finally, a 5% safety buffer is added to account for estimation variance.
//!
//! ## Mempool Interface
//!
//! This module is designed to work with the `Mempool` trait:
//! ```ignore
//! pub trait Mempool {
//!     fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<PoolOperation>>;
//!     fn remove_operation(&mut self, hash: &UserOpHash) -> Result<Option<PoolOperation>>;
//! }
//! ```

use alloy_primitives::{Address, Bytes};
#[cfg(test)]
use alloy_primitives::U256;
use alloy_sol_types::{sol, SolCall};

use crate::tx_signer::Signer;

/// Maximum gas for a single bundle transaction (21M gas)
pub const MAX_BUNDLE_GAS: u64 = 21_000_000;

/// Transaction intrinsic gas (shared across all ops in bundle)
pub const BUNDLE_SHARED_GAS: u64 = 21_000;

/// EntryPoint inner gas overhead for v0.6 (per op)
/// See: rundler/crates/types/src/user_operation/v0_6.rs
pub const ENTRY_POINT_INNER_GAS_OVERHEAD_V06: u64 = 5_000;

/// EntryPoint inner gas overhead for v0.7 (per op)
/// See: rundler/crates/types/src/user_operation/v0_7.rs
pub const ENTRY_POINT_INNER_GAS_OVERHEAD_V07: u64 = 10_000;

/// Extra buffer percent to add on the bundle transaction gas estimate (5%)
/// See: rundler/crates/builder/src/bundle_proposer.rs
pub const BUNDLE_TRANSACTION_GAS_OVERHEAD_PERCENT: u64 = 5;

// EntryPoint handleOps function signature (v0.7 packed format)
sol! {
    /// Minimal interface for EntryPoint handleOps (v0.7)
    #[sol(rpc)]
    interface IEntryPointV07 {
        function handleOps(
            PackedUserOperation[] calldata ops,
            address payable beneficiary
        ) external;
    }

    /// Packed UserOperation for v0.7
    /// See: https://eips.ethereum.org/EIPS/eip-4337
    #[derive(Debug, PartialEq, Eq)]
    struct PackedUserOperation {
        address sender;
        uint256 nonce;
        bytes initCode;
        bytes callData;
        /// Packed: verificationGasLimit (16 bytes) | callGasLimit (16 bytes)
        bytes32 accountGasLimits;
        uint256 preVerificationGas;
        /// Packed: maxPriorityFeePerGas (16 bytes) | maxFeePerGas (16 bytes)
        bytes32 gasFees;
        bytes paymasterAndData;
        bytes signature;
    }
}

// EntryPoint handleOps function signature (v0.6 unpacked format)
sol! {
    /// Minimal interface for EntryPoint handleOps (v0.6)
    #[sol(rpc)]
    interface IEntryPointV06 {
        function handleOps(
            UserOperation[] calldata ops,
            address payable beneficiary
        ) external;
    }

    /// UserOperation for v0.6 (unpacked format)
    #[derive(Debug, PartialEq, Eq)]
    struct UserOperation {
        address sender;
        uint256 nonce;
        bytes initCode;
        bytes callData;
        uint256 callGasLimit;
        uint256 verificationGasLimit;
        uint256 preVerificationGas;
        uint256 maxFeePerGas;
        uint256 maxPriorityFeePerGas;
        bytes paymasterAndData;
        bytes signature;
    }
}

/// Represents a bundle transaction to be included in a block
#[derive(Debug, Clone)]
pub struct BundleTransaction {
    /// The EntryPoint address this bundle targets
    pub entry_point: Address,
    /// The encoded calldata for handleOps
    pub calldata: Bytes,
    /// Gas limit for the bundle transaction
    pub gas_limit: u64,
    /// Max fee per gas
    pub max_fee_per_gas: u128,
    /// Max priority fee per gas
    pub max_priority_fee_per_gas: u128,
    /// Number of UserOperations in this bundle
    pub num_ops: usize,
    /// Hashes of the included operations (for removal on success/failure)
    pub op_hashes: Vec<[u8; 32]>,
}

/// Gas information extracted from a UserOperation
/// Follows Rundler's approach to gas calculation
#[derive(Debug, Clone, Copy)]
pub struct UserOpGasInfo {
    /// Verification gas limit
    pub verification_gas_limit: u64,
    /// Call gas limit
    pub call_gas_limit: u64,
    /// Pre-verification gas
    pub pre_verification_gas: u64,
    /// Paymaster verification gas limit (v0.7 only, 0 for v0.6)
    pub paymaster_verification_gas_limit: u64,
    /// Paymaster post-op gas limit (v0.7 only, 0 for v0.6)
    pub paymaster_post_op_gas_limit: u64,
    /// Max fee per gas
    pub max_fee_per_gas: u128,
    /// Max priority fee per gas
    pub max_priority_fee_per_gas: u128,
    /// Whether this is a v0.7 operation (affects gas calculation)
    pub is_v07: bool,
    /// Whether this operation has a paymaster (for v0.6 calculation)
    pub has_paymaster: bool,
}

impl UserOpGasInfo {
    /// Total verification gas limit
    /// v0.6: verification_gas_limit * (2 if paymaster else 1)
    /// v0.7: verification_gas_limit + paymaster_verification_gas_limit
    pub fn total_verification_gas_limit(&self) -> u64 {
        if self.is_v07 {
            self.verification_gas_limit
                .saturating_add(self.paymaster_verification_gas_limit)
        } else {
            // v0.6: if paymaster, double the verification gas
            let mul = if self.has_paymaster { 2 } else { 1 };
            self.verification_gas_limit.saturating_mul(mul)
        }
    }

    /// Required pre-execution buffer (per Rundler)
    ///
    /// v0.6: verification_gas_limit + ENTRY_POINT_INNER_GAS_OVERHEAD_V06
    /// v0.7: ENTRY_POINT_INNER_GAS_OVERHEAD_V07 + paymaster_post_op_gas_limit
    ///       + 1/63 of (call_gas_limit + paymaster_post_op_gas_limit + overhead)
    ///       (the 1/63 accounts for the 63/64ths rule in EVM call forwarding)
    pub fn required_pre_execution_buffer(&self) -> u64 {
        if self.is_v07 {
            let base = ENTRY_POINT_INNER_GAS_OVERHEAD_V07
                .saturating_add(self.paymaster_post_op_gas_limit);

            // 63/64ths rule buffer
            let inner_gas = self
                .call_gas_limit
                .saturating_add(self.paymaster_post_op_gas_limit)
                .saturating_add(ENTRY_POINT_INNER_GAS_OVERHEAD_V07);

            base.saturating_add(inner_gas / 63)
        } else {
            self.verification_gas_limit
                .saturating_add(ENTRY_POINT_INNER_GAS_OVERHEAD_V06)
        }
    }

    /// Gas limit contribution for this op in a bundle (excluding pre-verification gas)
    /// Per Rundler: total_verification_gas_limit + required_pre_execution_buffer + call_gas_limit
    pub fn bundle_gas_limit_without_pvg(&self) -> u64 {
        self.total_verification_gas_limit()
            .saturating_add(self.required_pre_execution_buffer())
            .saturating_add(self.call_gas_limit)
    }

    /// Total gas required for this operation (simple sum, for backwards compat)
    pub fn total_gas(&self) -> u64 {
        self.verification_gas_limit
            .saturating_add(self.call_gas_limit)
            .saturating_add(self.pre_verification_gas)
    }

    /// Extract gas info from packed v0.7 format
    pub fn from_packed(op: &PackedUserOperation) -> Self {
        let limits = op.accountGasLimits.0;
        let verification_gas = u128::from_be_bytes(limits[0..16].try_into().unwrap()) as u64;
        let call_gas = u128::from_be_bytes(limits[16..32].try_into().unwrap()) as u64;

        let fees = op.gasFees.0;
        let max_priority_fee = u128::from_be_bytes(fees[0..16].try_into().unwrap());
        let max_fee = u128::from_be_bytes(fees[16..32].try_into().unwrap());

        // Parse paymaster data to extract gas limits (if present)
        // paymasterAndData format: [paymaster (20)] [paymasterVerificationGasLimit (16)] [paymasterPostOpGasLimit (16)] [data...]
        let (paymaster_verification_gas, paymaster_post_op_gas, has_paymaster) =
            if op.paymasterAndData.len() >= 52 {
                let pm_data = &op.paymasterAndData[..];
                let pm_verification =
                    u128::from_be_bytes(pm_data[20..36].try_into().unwrap_or([0u8; 16])) as u64;
                let pm_post_op =
                    u128::from_be_bytes(pm_data[36..52].try_into().unwrap_or([0u8; 16])) as u64;
                (pm_verification, pm_post_op, true)
            } else {
                (0, 0, false)
            };

        Self {
            verification_gas_limit: verification_gas,
            call_gas_limit: call_gas,
            pre_verification_gas: op.preVerificationGas.try_into().unwrap_or(u64::MAX),
            paymaster_verification_gas_limit: paymaster_verification_gas,
            paymaster_post_op_gas_limit: paymaster_post_op_gas,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: max_priority_fee,
            is_v07: true,
            has_paymaster,
        }
    }

    /// Extract gas info from unpacked v0.6 format
    pub fn from_unpacked(op: &UserOperation) -> Self {
        let has_paymaster = op.paymasterAndData.len() >= 20;

        Self {
            verification_gas_limit: op.verificationGasLimit.try_into().unwrap_or(u64::MAX),
            call_gas_limit: op.callGasLimit.try_into().unwrap_or(u64::MAX),
            pre_verification_gas: op.preVerificationGas.try_into().unwrap_or(u64::MAX),
            paymaster_verification_gas_limit: 0, // v0.6 doesn't have separate paymaster verification
            paymaster_post_op_gas_limit: if has_paymaster {
                // In v0.6, paymaster post-op uses verification_gas_limit * 2
                op.verificationGasLimit.try_into().unwrap_or(0) * 2
            } else {
                0
            },
            max_fee_per_gas: op.maxFeePerGas.try_into().unwrap_or(u128::MAX),
            max_priority_fee_per_gas: op.maxPriorityFeePerGas.try_into().unwrap_or(u128::MAX),
            is_v07: false,
            has_paymaster,
        }
    }
}

/// Builder for creating bundle transactions
#[derive(Debug)]
pub struct BundleBuilder {
    /// The bundler signer for signing bundle transactions
    signer: Signer,
    /// Beneficiary address to receive bundle fees
    beneficiary: Address,
    /// Supported EntryPoint addresses
    entry_points: Vec<Address>,
}

impl BundleBuilder {
    /// Create a new bundle builder
    pub fn new(signer: Signer, beneficiary: Address, entry_points: Vec<Address>) -> Self {
        Self {
            signer,
            beneficiary,
            entry_points,
        }
    }

    /// Get the bundler's address
    pub fn address(&self) -> Address {
        self.signer.address
    }

    /// Get the beneficiary address
    pub fn beneficiary(&self) -> Address {
        self.beneficiary
    }

    /// Get supported entry points
    pub fn entry_points(&self) -> &[Address] {
        &self.entry_points
    }

    /// Create a bundle transaction from v0.7 packed UserOperations
    pub fn create_bundle_v07(
        &self,
        entry_point: Address,
        ops: Vec<PackedUserOperation>,
        op_hashes: Vec<[u8; 32]>,
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> BundleTransaction {
        let num_ops = ops.len();

        let call = IEntryPointV07::handleOpsCall {
            ops,
            beneficiary: self.beneficiary,
        };
        let calldata = Bytes::from(call.abi_encode());

        BundleTransaction {
            entry_point,
            calldata,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            num_ops,
            op_hashes,
        }
    }

    /// Create a bundle transaction from v0.6 unpacked UserOperations
    pub fn create_bundle_v06(
        &self,
        entry_point: Address,
        ops: Vec<UserOperation>,
        op_hashes: Vec<[u8; 32]>,
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> BundleTransaction {
        let num_ops = ops.len();

        let call = IEntryPointV06::handleOpsCall {
            ops,
            beneficiary: self.beneficiary,
        };
        let calldata = Bytes::from(call.abi_encode());

        BundleTransaction {
            entry_point,
            calldata,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            num_ops,
            op_hashes,
        }
    }

    /// Calculate total gas for a bundle (following Rundler's approach)
    ///
    /// Formula: SHARED_GAS + sum(op.bundle_gas_limit_without_pvg())
    /// Then apply 5% overhead buffer.
    ///
    /// # Arguments
    /// * `gas_infos` - Gas info for each operation
    ///
    /// # Returns
    /// Total gas limit with safety buffer, capped at MAX_BUNDLE_GAS
    pub fn calculate_bundle_gas_limit(gas_infos: &[UserOpGasInfo]) -> u64 {
        // Start with shared gas (transaction intrinsic)
        let mut gas_limit = BUNDLE_SHARED_GAS as u128;

        // Add per-op gas contribution
        for info in gas_infos {
            gas_limit = gas_limit.saturating_add(info.bundle_gas_limit_without_pvg() as u128);
        }

        // Apply 5% safety buffer (per Rundler)
        gas_limit = increase_by_percent(gas_limit, BUNDLE_TRANSACTION_GAS_OVERHEAD_PERCENT as u128);

        // Cap at MAX_BUNDLE_GAS
        gas_limit.min(MAX_BUNDLE_GAS as u128) as u64
    }

    /// Calculate gas limit for adding a single operation to a bundle
    ///
    /// Returns the additional gas needed if this op is added.
    pub fn gas_for_op(gas_info: &UserOpGasInfo) -> u64 {
        gas_info.bundle_gas_limit_without_pvg()
    }

    /// Check if adding an operation would exceed the gas limit
    pub fn can_add_operation(
        current_gas: u64,
        op_gas: &UserOpGasInfo,
        max_bundle_gas: u64,
    ) -> bool {
        let op_contribution = Self::gas_for_op(op_gas);
        let new_total = current_gas.saturating_add(op_contribution);
        // Account for the overhead buffer when checking
        let buffered = increase_by_percent(new_total as u128, BUNDLE_TRANSACTION_GAS_OVERHEAD_PERCENT as u128);
        buffered <= max_bundle_gas as u128
    }

    /// Estimate gas for a bundle (simple estimation)
    ///
    /// # Arguments
    /// * `num_ops` - Number of UserOperations in the bundle
    /// * `avg_gas_per_op` - Average gas per operation (default: 150,000)
    ///
    /// # Returns
    /// Estimated gas limit for the bundle
    pub fn estimate_bundle_gas(num_ops: usize, avg_gas_per_op: Option<u64>) -> u64 {
        let avg = avg_gas_per_op.unwrap_or(150_000);
        let gas = BUNDLE_SHARED_GAS.saturating_add(num_ops as u64 * avg);
        let buffered = increase_by_percent(gas as u128, BUNDLE_TRANSACTION_GAS_OVERHEAD_PERCENT as u128);
        buffered.min(MAX_BUNDLE_GAS as u128) as u64
    }

    // Keep old method for backwards compatibility
    #[deprecated(note = "Use calculate_bundle_gas_limit instead")]
    pub fn calculate_total_gas(gas_infos: &[UserOpGasInfo]) -> u64 {
        Self::calculate_bundle_gas_limit(gas_infos)
    }
}

/// Increase a value by a percentage
/// (val * (100 + percent)) / 100
pub fn increase_by_percent(val: u128, percent: u128) -> u128 {
    val.saturating_mul(100 + percent) / 100
}

/// Configuration for the bundle builder
#[derive(Debug, Clone)]
pub struct BundleConfig {
    /// Whether native bundling is enabled
    pub enabled: bool,
    /// The bundler signer
    pub signer: Option<Signer>,
    /// Gas threshold percentage (0-100)
    pub gas_threshold: u8,
    /// Gas reserve percentage (0-100)
    pub gas_reserve: u8,
    /// Pool URL for fetching UserOperations
    pub pool_url: Option<String>,
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            signer: None,
            gas_threshold: 50, // Updated to 50% for middle-of-block
            gas_reserve: 20,
            pool_url: None,
        }
    }
}

impl BundleConfig {
    /// Check if bundling is ready (enabled and has signer)
    pub fn is_ready(&self) -> bool {
        self.enabled && self.signer.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_signer() -> Signer {
        Signer::random()
    }

    #[test]
    fn test_bundle_builder_creation() {
        let signer = test_signer();
        let beneficiary = Address::repeat_byte(0x01);
        let entry_points = vec![Address::repeat_byte(0x02)];

        let builder = BundleBuilder::new(signer.clone(), beneficiary, entry_points.clone());

        assert_eq!(builder.address(), signer.address);
        assert_eq!(builder.beneficiary(), beneficiary);
        assert_eq!(builder.entry_points(), &entry_points);
    }

    #[test]
    fn test_create_bundle_v07() {
        let signer = test_signer();
        let beneficiary = Address::repeat_byte(0x01);
        let entry_point = Address::repeat_byte(0x02);

        let builder = BundleBuilder::new(signer, beneficiary, vec![entry_point]);

        let ops = vec![PackedUserOperation {
            sender: Address::repeat_byte(0x03),
            nonce: U256::ZERO,
            initCode: Bytes::new(),
            callData: Bytes::new(),
            accountGasLimits: [0u8; 32].into(),
            preVerificationGas: U256::from(21000),
            gasFees: [0u8; 32].into(),
            paymasterAndData: Bytes::new(),
            signature: Bytes::new(),
        }];
        let op_hashes = vec![[0u8; 32]];

        let bundle = builder.create_bundle_v07(
            entry_point,
            ops,
            op_hashes.clone(),
            200_000,
            1_000_000_000,
            100_000_000,
        );

        assert_eq!(bundle.entry_point, entry_point);
        assert_eq!(bundle.num_ops, 1);
        assert_eq!(bundle.gas_limit, 200_000);
        assert!(!bundle.calldata.is_empty());
        assert_eq!(bundle.op_hashes, op_hashes);
    }

    #[test]
    fn test_user_op_gas_info_v06() {
        let gas_info = UserOpGasInfo {
            verification_gas_limit: 100_000,
            call_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            paymaster_verification_gas_limit: 0,
            paymaster_post_op_gas_limit: 0,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 100_000_000,
            is_v07: false,
            has_paymaster: false,
        };

        // total_verification = 100k (no paymaster)
        assert_eq!(gas_info.total_verification_gas_limit(), 100_000);

        // required_pre_execution_buffer = verification (100k) + overhead (5k)
        assert_eq!(gas_info.required_pre_execution_buffer(), 105_000);

        // bundle_gas_limit_without_pvg = total_verification (100k) + buffer (105k) + call (50k)
        assert_eq!(gas_info.bundle_gas_limit_without_pvg(), 255_000);
    }

    #[test]
    fn test_user_op_gas_info_v06_with_paymaster() {
        let gas_info = UserOpGasInfo {
            verification_gas_limit: 100_000,
            call_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            paymaster_verification_gas_limit: 0,
            paymaster_post_op_gas_limit: 200_000, // verification * 2
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 100_000_000,
            is_v07: false,
            has_paymaster: true,
        };

        // total_verification = 100k * 2 = 200k (with paymaster)
        assert_eq!(gas_info.total_verification_gas_limit(), 200_000);

        // required_pre_execution_buffer = verification (100k) + overhead (5k)
        assert_eq!(gas_info.required_pre_execution_buffer(), 105_000);

        // bundle_gas_limit_without_pvg = total_verification (200k) + buffer (105k) + call (50k)
        assert_eq!(gas_info.bundle_gas_limit_without_pvg(), 355_000);
    }

    #[test]
    fn test_user_op_gas_info_v07() {
        let gas_info = UserOpGasInfo {
            verification_gas_limit: 100_000,
            call_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            paymaster_verification_gas_limit: 0,
            paymaster_post_op_gas_limit: 0,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 100_000_000,
            is_v07: true,
            has_paymaster: false,
        };

        // total_verification = 100k + 0 = 100k
        assert_eq!(gas_info.total_verification_gas_limit(), 100_000);

        // required_pre_execution_buffer = 10k + 0 + (50k + 0 + 10k) / 63
        // = 10,000 + 952 = 10,952
        assert_eq!(gas_info.required_pre_execution_buffer(), 10_952);

        // bundle_gas_limit_without_pvg = 100k + 10,952 + 50k = 160,952
        assert_eq!(gas_info.bundle_gas_limit_without_pvg(), 160_952);
    }

    #[test]
    fn test_user_op_gas_info_v07_with_paymaster() {
        let gas_info = UserOpGasInfo {
            verification_gas_limit: 100_000,
            call_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            paymaster_verification_gas_limit: 50_000,
            paymaster_post_op_gas_limit: 30_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 100_000_000,
            is_v07: true,
            has_paymaster: true,
        };

        // total_verification = 100k + 50k = 150k
        assert_eq!(gas_info.total_verification_gas_limit(), 150_000);

        // required_pre_execution_buffer = 10k + 30k + (50k + 30k + 10k) / 63
        // = 40,000 + 1,428 = 41,428
        assert_eq!(gas_info.required_pre_execution_buffer(), 41_428);

        // bundle_gas_limit_without_pvg = 150k + 41,428 + 50k = 241,428
        assert_eq!(gas_info.bundle_gas_limit_without_pvg(), 241_428);
    }

    #[test]
    fn test_calculate_bundle_gas_limit() {
        // Two v0.6 ops without paymaster
        let gas_infos = vec![
            UserOpGasInfo {
                verification_gas_limit: 100_000,
                call_gas_limit: 50_000,
                pre_verification_gas: 21_000,
                paymaster_verification_gas_limit: 0,
                paymaster_post_op_gas_limit: 0,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 100_000_000,
                is_v07: false,
                has_paymaster: false,
            },
            UserOpGasInfo {
                verification_gas_limit: 80_000,
                call_gas_limit: 40_000,
                pre_verification_gas: 21_000,
                paymaster_verification_gas_limit: 0,
                paymaster_post_op_gas_limit: 0,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 100_000_000,
                is_v07: false,
                has_paymaster: false,
            },
        ];

        // Op1: bundle_gas = 100k + 105k + 50k = 255k
        // Op2: bundle_gas = 80k + 85k + 40k = 205k
        // Total = 21k (shared) + 255k + 205k = 481k
        // With 5% buffer = 481k * 1.05 = 505,050
        let total = BundleBuilder::calculate_bundle_gas_limit(&gas_infos);
        assert_eq!(total, 505_050);
    }

    #[test]
    fn test_can_add_operation() {
        let op_gas = UserOpGasInfo {
            verification_gas_limit: 100_000,
            call_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            paymaster_verification_gas_limit: 0,
            paymaster_post_op_gas_limit: 0,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 100_000_000,
            is_v07: false,
            has_paymaster: false,
        };

        // op contributes 255k gas
        // Can fit in 300k (255k * 1.05 = 267,750)
        assert!(BundleBuilder::can_add_operation(0, &op_gas, 300_000));

        // Cannot fit if we already have 100k used (355k * 1.05 = 372,750 > 300k)
        assert!(!BundleBuilder::can_add_operation(100_000, &op_gas, 300_000));
    }

    #[test]
    fn test_estimate_bundle_gas() {
        // Single op: 21k + 150k = 171k, with 5% = 179,550
        let gas = BundleBuilder::estimate_bundle_gas(1, None);
        assert_eq!(gas, 179_550);

        // Multiple ops: 21k + 5 * 150k = 771k, with 5% = 809,550
        let gas = BundleBuilder::estimate_bundle_gas(5, None);
        assert_eq!(gas, 809_550);

        // Custom gas per op: 21k + 3 * 200k = 621k, with 5% = 652,050
        let gas = BundleBuilder::estimate_bundle_gas(3, Some(200_000));
        assert_eq!(gas, 652_050);
    }

    #[test]
    fn test_bundle_config_defaults() {
        let config = BundleConfig::default();

        assert!(!config.enabled);
        assert!(config.signer.is_none());
        assert_eq!(config.gas_threshold, 50); // Updated default
        assert_eq!(config.gas_reserve, 20);
        assert!(config.pool_url.is_none());
    }

    #[test]
    fn test_bundle_config_is_ready() {
        let mut config = BundleConfig::default();
        assert!(!config.is_ready());

        config.enabled = true;
        assert!(!config.is_ready()); // Still no signer

        config.signer = Some(test_signer());
        assert!(config.is_ready());
    }

    #[test]
    fn test_increase_by_percent() {
        assert_eq!(increase_by_percent(100, 5), 105);
        assert_eq!(increase_by_percent(1000, 10), 1100);
        assert_eq!(increase_by_percent(100, 0), 100);
    }
}
