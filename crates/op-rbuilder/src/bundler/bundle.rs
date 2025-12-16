//! Bundle transaction creation for ERC-4337 UserOperations
//!
//! ## V0 Bundle Building Strategy
//!
//! For v0, we optimistically build the biggest bundle where:
//! ```text
//! Sum(validationGasLimit) + Sum(executionGasLimit) + entrypointBuffer < MAX_BUNDLE_GAS
//! ```
//!
//! The MAX_BUNDLE_GAS is typically 21M gas. If any ops fail during execution,
//! we prune them out.
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
//!
//! The `get_top_operations` already returns ops sorted by priority (highest gas price first),
//! so bundling just needs to greedily consume from the iterator until gas limit is reached.

use alloy_primitives::{Address, Bytes};
#[cfg(test)]
use alloy_primitives::U256;
use alloy_sol_types::{sol, SolCall};

use crate::tx_signer::Signer;

/// Maximum gas for a single bundle transaction (21M gas)
pub const MAX_BUNDLE_GAS: u64 = 21_000_000;

/// Buffer gas for EntryPoint overhead per bundle
pub const ENTRYPOINT_BUFFER_GAS: u64 = 100_000;

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
#[derive(Debug, Clone, Copy)]
pub struct UserOpGasInfo {
    /// Verification gas limit
    pub verification_gas_limit: u64,
    /// Call gas limit  
    pub call_gas_limit: u64,
    /// Pre-verification gas
    pub pre_verification_gas: u64,
    /// Max fee per gas
    pub max_fee_per_gas: u128,
    /// Max priority fee per gas
    pub max_priority_fee_per_gas: u128,
}

impl UserOpGasInfo {
    /// Total gas required for this operation
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

        Self {
            verification_gas_limit: verification_gas,
            call_gas_limit: call_gas,
            pre_verification_gas: op.preVerificationGas.try_into().unwrap_or(u64::MAX),
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: max_priority_fee,
        }
    }

    /// Extract gas info from unpacked v0.6 format
    pub fn from_unpacked(op: &UserOperation) -> Self {
        Self {
            verification_gas_limit: op.verificationGasLimit.try_into().unwrap_or(u64::MAX),
            call_gas_limit: op.callGasLimit.try_into().unwrap_or(u64::MAX),
            pre_verification_gas: op.preVerificationGas.try_into().unwrap_or(u64::MAX),
            max_fee_per_gas: op.maxFeePerGas.try_into().unwrap_or(u128::MAX),
            max_priority_fee_per_gas: op.maxPriorityFeePerGas.try_into().unwrap_or(u128::MAX),
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
    ///
    /// # Arguments
    /// * `signer` - The signer for bundle transactions
    /// * `beneficiary` - Address to receive bundle execution fees
    /// * `entry_points` - List of supported EntryPoint addresses
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
    ///
    /// # Arguments
    /// * `entry_point` - The EntryPoint address for this bundle
    /// * `ops` - The packed UserOperations to bundle
    /// * `op_hashes` - Hashes of the operations (for tracking/removal)
    /// * `gas_limit` - Gas limit for the bundle transaction
    /// * `max_fee_per_gas` - Max fee per gas
    /// * `max_priority_fee_per_gas` - Max priority fee per gas
    ///
    /// # Returns
    /// A `BundleTransaction` ready for inclusion in a block
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

        // Encode the handleOps call for v0.7
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
    ///
    /// # Arguments
    /// * `entry_point` - The EntryPoint address for this bundle
    /// * `ops` - The unpacked UserOperations to bundle
    /// * `op_hashes` - Hashes of the operations (for tracking/removal)
    /// * `gas_limit` - Gas limit for the bundle transaction
    /// * `max_fee_per_gas` - Max fee per gas
    /// * `max_priority_fee_per_gas` - Max priority fee per gas
    ///
    /// # Returns
    /// A `BundleTransaction` ready for inclusion in a block
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

        // Encode the handleOps call for v0.6
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

    /// Calculate total gas for a set of operations from their gas info
    ///
    /// V0 Strategy: Sum(validationGasLimit) + Sum(callGasLimit) + Sum(preVerificationGas) + buffer
    ///
    /// # Arguments
    /// * `gas_infos` - Gas info for each operation
    ///
    /// # Returns
    /// Total gas limit, capped at MAX_BUNDLE_GAS
    pub fn calculate_total_gas(gas_infos: &[UserOpGasInfo]) -> u64 {
        let base_gas = 21_000_u64;

        let total_op_gas: u64 = gas_infos
            .iter()
            .map(|info| info.total_gas())
            .fold(0u64, |acc, gas| acc.saturating_add(gas));

        let total = base_gas
            .saturating_add(ENTRYPOINT_BUFFER_GAS)
            .saturating_add(total_op_gas);

        total.min(MAX_BUNDLE_GAS)
    }

    /// Check if adding an operation would exceed the gas limit
    ///
    /// # Arguments
    /// * `current_gas` - Current cumulative gas of operations in bundle
    /// * `op_gas` - Gas info for the operation to add
    /// * `max_bundle_gas` - Maximum gas allowed for the bundle
    ///
    /// # Returns
    /// `true` if the operation can fit, `false` if it would exceed the limit
    pub fn can_add_operation(current_gas: u64, op_gas: &UserOpGasInfo, max_bundle_gas: u64) -> bool {
        let op_total = op_gas.total_gas();
        current_gas.saturating_add(op_total) <= max_bundle_gas
    }

    /// Estimate gas for a bundle (simple estimation)
    ///
    /// This provides a rough estimate based on the number of operations.
    /// Actual gas usage depends on the specific UserOperations.
    ///
    /// # Arguments
    /// * `num_ops` - Number of UserOperations in the bundle
    /// * `avg_gas_per_op` - Average gas per operation (default: 100,000)
    ///
    /// # Returns
    /// Estimated gas limit for the bundle
    pub fn estimate_bundle_gas(num_ops: usize, avg_gas_per_op: Option<u64>) -> u64 {
        let avg = avg_gas_per_op.unwrap_or(100_000);
        let base_gas = 21_000_u64;

        base_gas
            .saturating_add(ENTRYPOINT_BUFFER_GAS)
            .saturating_add(num_ops as u64 * avg)
            .min(MAX_BUNDLE_GAS)
    }
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
            gas_threshold: 80,
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
    fn test_user_op_gas_info() {
        let gas_info = UserOpGasInfo {
            verification_gas_limit: 100_000,
            call_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 100_000_000,
        };

        assert_eq!(gas_info.total_gas(), 171_000);
    }

    #[test]
    fn test_calculate_total_gas() {
        let gas_infos = vec![
            UserOpGasInfo {
                verification_gas_limit: 100_000,
                call_gas_limit: 50_000,
                pre_verification_gas: 21_000,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 100_000_000,
            },
            UserOpGasInfo {
                verification_gas_limit: 80_000,
                call_gas_limit: 40_000,
                pre_verification_gas: 21_000,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 100_000_000,
            },
        ];

        // base (21k) + buffer (100k) + op1 (171k) + op2 (141k) = 433k
        let total = BundleBuilder::calculate_total_gas(&gas_infos);
        assert_eq!(total, 21_000 + ENTRYPOINT_BUFFER_GAS + 171_000 + 141_000);
    }

    #[test]
    fn test_can_add_operation() {
        let op_gas = UserOpGasInfo {
            verification_gas_limit: 100_000,
            call_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 100_000_000,
        };

        // Can fit when there's room
        assert!(BundleBuilder::can_add_operation(0, &op_gas, 200_000));
        assert!(BundleBuilder::can_add_operation(29_000, &op_gas, 200_000));

        // Cannot fit when at limit
        assert!(!BundleBuilder::can_add_operation(30_000, &op_gas, 200_000));
        assert!(!BundleBuilder::can_add_operation(200_000, &op_gas, 200_000));
    }

    #[test]
    fn test_estimate_bundle_gas() {
        // Single op with defaults: base + buffer + 1 * 100k
        let gas = BundleBuilder::estimate_bundle_gas(1, None);
        assert_eq!(gas, 21_000 + ENTRYPOINT_BUFFER_GAS + 100_000); // 221,000

        // Multiple ops: base + buffer + 5 * 100k
        let gas = BundleBuilder::estimate_bundle_gas(5, None);
        assert_eq!(gas, 21_000 + ENTRYPOINT_BUFFER_GAS + 500_000); // 621,000

        // Custom gas per op: base + buffer + 3 * 150k
        let gas = BundleBuilder::estimate_bundle_gas(3, Some(150_000));
        assert_eq!(gas, 21_000 + ENTRYPOINT_BUFFER_GAS + 450_000); // 571,000
    }

    #[test]
    fn test_bundle_config_defaults() {
        let config = BundleConfig::default();

        assert!(!config.enabled);
        assert!(config.signer.is_none());
        assert_eq!(config.gas_threshold, 80);
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
}

