//! Pool client for fetching UserOperations from the mempool
//!
//! This module defines the interface for connecting to the AA mempool service
//! and fetching UserOperations for bundling.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────┐      ┌─────────────────────────────┐
//! │   op-rbuilder   │      │      tips (mempool)         │
//! │                 │      │                             │
//! │  PoolClient ────┼─────→│  MempoolImpl                │
//! │  (this module)  │ RPC  │  - get_top_operations()     │
//! │                 │      │  - remove_operation()       │
//! └─────────────────┘      └─────────────────────────────┘
//! ```
//!
//! ## Usage
//!
//! The `PoolClient` trait mirrors the `Mempool` trait from `account-abstraction-core`.
//! During block building:
//! 1. Call `get_top_operations(n)` to fetch highest-priority ops
//! 2. Build bundles from the returned operations
//! 3. After successful inclusion, call `remove_operation(hash)` to clean up

use alloy_primitives::{Address, Bytes, U256};
use std::sync::Arc;

/// Hash of a UserOperation (32 bytes)
pub type UserOpHash = [u8; 32];

/// A UserOperation with its metadata, ready for bundling
#[derive(Debug, Clone)]
pub struct PoolOperation {
    /// The operation hash for tracking/removal
    pub hash: UserOpHash,
    /// The entry point address (determines v0.6 vs v0.7)
    pub entry_point: Address,
    /// The sender (smart account) address
    pub sender: Address,
    /// Nonce of the operation
    pub nonce: U256,
    /// Gas limits and fees
    pub gas_info: OperationGasInfo,
    /// The raw operation data for building handleOps calldata
    pub operation: UserOperationVariant,
}

/// Gas information for a UserOperation
#[derive(Debug, Clone, Copy)]
pub struct OperationGasInfo {
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

impl OperationGasInfo {
    /// Total gas required for this operation
    pub fn total_gas(&self) -> u64 {
        self.verification_gas_limit
            .saturating_add(self.call_gas_limit)
            .saturating_add(self.pre_verification_gas)
    }
}

/// UserOperation variant (v0.6 unpacked or v0.7 packed)
#[derive(Debug, Clone)]
pub enum UserOperationVariant {
    /// EntryPoint v0.6 (unpacked format)
    V06(UserOperationV06),
    /// EntryPoint v0.7 (packed format)
    V07(UserOperationV07),
}

/// UserOperation v0.6 (unpacked format)
#[derive(Debug, Clone)]
pub struct UserOperationV06 {
    pub sender: Address,
    pub nonce: U256,
    pub init_code: Bytes,
    pub call_data: Bytes,
    pub call_gas_limit: U256,
    pub verification_gas_limit: U256,
    pub pre_verification_gas: U256,
    pub max_fee_per_gas: U256,
    pub max_priority_fee_per_gas: U256,
    pub paymaster_and_data: Bytes,
    pub signature: Bytes,
}

/// UserOperation v0.7 (packed format)
#[derive(Debug, Clone)]
pub struct UserOperationV07 {
    pub sender: Address,
    pub nonce: U256,
    pub init_code: Bytes,
    pub call_data: Bytes,
    /// Packed: verificationGasLimit (16 bytes) | callGasLimit (16 bytes)
    pub account_gas_limits: [u8; 32],
    pub pre_verification_gas: U256,
    /// Packed: maxPriorityFeePerGas (16 bytes) | maxFeePerGas (16 bytes)
    pub gas_fees: [u8; 32],
    pub paymaster_and_data: Bytes,
    pub signature: Bytes,
}

/// Trait for the pool client that fetches UserOperations
///
/// This mirrors the `Mempool` trait from `account-abstraction-core`.
/// Implementations will connect to the actual mempool service.
pub trait PoolClient: Send + Sync {
    /// Get the top N operations sorted by priority (highest gas price first)
    ///
    /// Operations are already sorted by max_priority_fee_per_gas descending.
    /// The bundler should greedily consume from this iterator until gas limit is reached.
    fn get_top_operations(&self, n: usize) -> Vec<Arc<PoolOperation>>;

    /// Remove an operation from the pool by its hash
    ///
    /// Called after an operation has been successfully included in a bundle,
    /// or if it failed and should not be retried.
    fn remove_operation(&self, hash: &UserOpHash) -> Result<(), PoolClientError>;

    /// Check if the pool client is connected and ready
    fn is_ready(&self) -> bool;
}

/// Errors from the pool client
#[derive(Debug, thiserror::Error)]
pub enum PoolClientError {
    /// Operation not found in pool
    #[error("Operation not found: {0:?}")]
    NotFound(UserOpHash),

    /// Connection error to the pool service
    #[error("Connection error: {0}")]
    ConnectionError(String),

    /// Pool service returned an error
    #[error("Pool error: {0}")]
    PoolError(String),
}

/// A no-op pool client for when AA bundling is disabled
#[derive(Debug, Clone, Default)]
pub struct NoOpPoolClient;

impl PoolClient for NoOpPoolClient {
    fn get_top_operations(&self, _n: usize) -> Vec<Arc<PoolOperation>> {
        vec![]
    }

    fn remove_operation(&self, _hash: &UserOpHash) -> Result<(), PoolClientError> {
        Ok(())
    }

    fn is_ready(&self) -> bool {
        false
    }
}

// TODO: (BA-3414) Implement RemotePoolClient that connects to tips mempool service
//
// ```rust
// pub struct RemotePoolClient {
//     /// URL of the mempool service
//     url: String,
//     /// HTTP client for RPC calls
//     client: reqwest::Client,
// }
//
// impl RemotePoolClient {
//     pub fn new(url: String) -> Self {
//         Self {
//             url,
//             client: reqwest::Client::new(),
//         }
//     }
// }
//
// impl PoolClient for RemotePoolClient {
//     fn get_top_operations(&self, n: usize) -> Vec<Arc<PoolOperation>> {
//         // TODO: Call mempool RPC endpoint
//         // POST /rpc { method: "mempool_getTopOperations", params: [n] }
//         todo!()
//     }
//
//     fn remove_operation(&self, hash: &UserOpHash) -> Result<(), PoolClientError> {
//         // TODO: Call mempool RPC endpoint
//         // POST /rpc { method: "mempool_removeOperation", params: [hash] }
//         todo!()
//     }
//
//     fn is_ready(&self) -> bool {
//         // TODO: Health check endpoint
//         true
//     }
// }
// ```

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_op_pool_client() {
        let client = NoOpPoolClient;

        assert!(!client.is_ready());
        assert!(client.get_top_operations(10).is_empty());
        assert!(client.remove_operation(&[0u8; 32]).is_ok());
    }

    #[test]
    fn test_operation_gas_info_total() {
        let gas_info = OperationGasInfo {
            verification_gas_limit: 100_000,
            call_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 100_000_000,
        };

        assert_eq!(gas_info.total_gas(), 171_000);
    }
}

