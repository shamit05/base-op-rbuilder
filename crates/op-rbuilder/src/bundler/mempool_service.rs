//! Mempool service for AA UserOperations
//!
//! This module provides a shared mempool that consumes UserOperations from Kafka
//! and makes them available for bundling during block building.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                         op-rbuilder                             │
//! │                                                                 │
//! │  ┌──────────────────────┐     ┌────────────────────────────┐   │
//! │  │  Kafka Consumer      │     │    Payload Builder         │   │
//! │  │  (background task)   │────▶│                            │   │
//! │  │                      │     │  SharedMempool             │   │
//! │  └──────────────────────┘     │  - get_top_operations()    │   │
//! │                               │  - remove_operation()      │   │
//! │                               └────────────────────────────┘   │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

use account_abstraction_core::types::{UserOpHash, WrappedUserOperation};
#[cfg(feature = "kafka")]
use account_abstraction_core::types::VersionedUserOperation;
#[cfg(feature = "kafka")]
use alloy_primitives::B256;
use anyhow::Result;
use parking_lot::RwLock;
use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap},
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        Arc,
    },
};

#[cfg(feature = "kafka")]
use rdkafka::{
    config::ClientConfig,
    consumer::{Consumer, StreamConsumer},
    message::Message,
};
#[cfg(feature = "kafka")]
use tokio::sync::oneshot;
#[cfg(feature = "kafka")]
use tracing::{debug, error, info, warn};

// ============================================================================
// Local Mempool Implementation
// ============================================================================
// We implement our own mempool here because account-abstraction-core's 
// PoolConfig has private fields. This is a simplified version that provides
// the same interface.

/// Ordered pool operation for priority queue
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct OrderedPoolOperation {
    pub pool_operation: WrappedUserOperation,
    pub submission_id: u64,
}

impl OrderedPoolOperation {
    pub fn from_wrapped(operation: &WrappedUserOperation, submission_id: u64) -> Self {
        Self {
            pool_operation: operation.clone(),
            submission_id,
        }
    }

    pub fn sender(&self) -> alloy_primitives::Address {
        self.pool_operation.operation.sender()
    }
}

/// Ordering by max priority fee (desc) then submission id
#[derive(Clone, Debug)]
struct ByMaxFeeAndSubmissionId(OrderedPoolOperation);

impl PartialEq for ByMaxFeeAndSubmissionId {
    fn eq(&self, other: &Self) -> bool {
        self.0.pool_operation.hash == other.0.pool_operation.hash
    }
}
impl Eq for ByMaxFeeAndSubmissionId {}

impl PartialOrd for ByMaxFeeAndSubmissionId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ByMaxFeeAndSubmissionId {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .0
            .pool_operation
            .operation
            .max_priority_fee_per_gas()
            .cmp(&self.0.pool_operation.operation.max_priority_fee_per_gas())
            .then_with(|| self.0.submission_id.cmp(&other.0.submission_id))
            .then_with(|| self.0.pool_operation.hash.cmp(&other.0.pool_operation.hash))
    }
}

/// Ordering by nonce (asc) then submission id
#[derive(Clone, Debug)]
struct ByNonce(OrderedPoolOperation);

impl PartialEq for ByNonce {
    fn eq(&self, other: &Self) -> bool {
        self.0.pool_operation.hash == other.0.pool_operation.hash
    }
}
impl Eq for ByNonce {}

impl PartialOrd for ByNonce {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ByNonce {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .pool_operation
            .operation
            .nonce()
            .cmp(&other.0.pool_operation.operation.nonce())
            .then_with(|| self.0.submission_id.cmp(&other.0.submission_id))
    }
}

/// In-memory mempool for UserOperations
#[derive(Debug)]
pub struct MempoolImpl {
    minimum_max_fee_per_gas: u128,
    best: BTreeSet<ByMaxFeeAndSubmissionId>,
    hash_to_operation: HashMap<UserOpHash, OrderedPoolOperation>,
    operations_by_account: HashMap<alloy_primitives::Address, BTreeSet<ByNonce>>,
    submission_id_counter: AtomicU64,
}

impl MempoolImpl {
    /// Create a new mempool with the given minimum gas price
    pub fn new(minimum_max_fee_per_gas: u128) -> Self {
        Self {
            minimum_max_fee_per_gas,
            best: BTreeSet::new(),
            hash_to_operation: HashMap::new(),
            operations_by_account: HashMap::new(),
            submission_id_counter: AtomicU64::new(0),
        }
    }

    /// Add an operation to the mempool
    pub fn add_operation(
        &mut self,
        operation: &WrappedUserOperation,
    ) -> Result<Option<OrderedPoolOperation>> {
        if operation.operation.max_fee_per_gas() < alloy_primitives::U256::from(self.minimum_max_fee_per_gas) {
            return Err(anyhow::anyhow!(
                "Gas price is below the minimum required"
            ));
        }

        if self.hash_to_operation.contains_key(&operation.hash) {
            return Ok(None);
        }

        let order = self.submission_id_counter.fetch_add(1, AtomicOrdering::SeqCst);
        let ordered_operation = OrderedPoolOperation::from_wrapped(operation, order);

        self.best
            .insert(ByMaxFeeAndSubmissionId(ordered_operation.clone()));
        self.operations_by_account
            .entry(ordered_operation.sender())
            .or_default()
            .insert(ByNonce(ordered_operation.clone()));
        self.hash_to_operation
            .insert(operation.hash, ordered_operation.clone());

        Ok(Some(ordered_operation))
    }

    /// Get top N operations sorted by priority
    pub fn get_top_operations(&self, n: usize) -> impl Iterator<Item = Arc<WrappedUserOperation>> + '_ {
        self.best
            .iter()
            .filter_map(|op_by_fee| {
                let lowest = self
                    .operations_by_account
                    .get(&op_by_fee.0.sender())
                    .and_then(|set| set.first());

                match lowest {
                    Some(lowest)
                        if lowest.0.pool_operation.hash == op_by_fee.0.pool_operation.hash =>
                    {
                        Some(Arc::new(op_by_fee.0.pool_operation.clone()))
                    }
                    _ => None,
                }
            })
            .take(n)
    }

    /// Remove an operation by hash
    pub fn remove_operation(
        &mut self,
        operation_hash: &UserOpHash,
    ) -> Result<Option<WrappedUserOperation>> {
        if let Some(ordered_operation) = self.hash_to_operation.remove(operation_hash) {
            self.best
                .remove(&ByMaxFeeAndSubmissionId(ordered_operation.clone()));
            if let Some(set) = self.operations_by_account.get_mut(&ordered_operation.sender()) {
                set.remove(&ByNonce(ordered_operation.clone()));
            }
            Ok(Some(ordered_operation.pool_operation))
        } else {
            Ok(None)
        }
    }

    /// Get the number of operations in the mempool
    pub fn len(&self) -> usize {
        self.hash_to_operation.len()
    }

    /// Check if the mempool is empty
    pub fn is_empty(&self) -> bool {
        self.hash_to_operation.is_empty()
    }
}

/// Shared mempool that can be accessed from multiple threads
pub type SharedMempool = Arc<RwLock<MempoolImpl>>;

/// Configuration for the mempool service
#[derive(Debug, Clone)]
pub struct MempoolServiceConfig {
    /// Kafka bootstrap servers
    pub kafka_brokers: String,
    /// Kafka topic for UserOperations
    pub kafka_topic: String,
    /// Consumer group ID
    pub consumer_group: String,
    /// Minimum max fee per gas for operations
    pub minimum_max_fee_per_gas: u128,
    /// Additional Kafka config from properties file
    pub kafka_properties: Option<HashMap<String, String>>,
}

impl Default for MempoolServiceConfig {
    fn default() -> Self {
        Self {
            kafka_brokers: "localhost:9092".to_string(),
            kafka_topic: "tips-userop".to_string(),
            consumer_group: "op-rbuilder-bundler".to_string(),
            minimum_max_fee_per_gas: 1_000_000_000, // 1 gwei
            kafka_properties: None,
        }
    }
}

/// Handle to the mempool service
pub struct MempoolService {
    /// Shared mempool for reading operations
    mempool: SharedMempool,
    /// Shutdown signal sender (only used when Kafka is enabled)
    #[cfg(feature = "kafka")]
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl MempoolService {
    /// Create a new mempool service with the given config
    pub fn new(config: MempoolServiceConfig) -> Result<Self> {
        let mempool = Arc::new(RwLock::new(MempoolImpl::new(config.minimum_max_fee_per_gas)));

        Ok(Self {
            mempool,
            #[cfg(feature = "kafka")]
            shutdown_tx: None,
        })
    }

    /// Create a mempool service without Kafka (for testing or when disabled)
    pub fn new_without_kafka(minimum_max_fee_per_gas: u128) -> Self {
        let mempool = Arc::new(RwLock::new(MempoolImpl::new(minimum_max_fee_per_gas)));

        Self {
            mempool,
            #[cfg(feature = "kafka")]
            shutdown_tx: None,
        }
    }

    /// Get a clone of the shared mempool
    pub fn mempool(&self) -> SharedMempool {
        Arc::clone(&self.mempool)
    }

    /// Start the Kafka consumer in a background task
    #[cfg(feature = "kafka")]
    pub fn start_consumer(&mut self, config: MempoolServiceConfig) -> Result<()> {
        let mempool = Arc::clone(&self.mempool);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // Build Kafka consumer config
        let mut client_config = ClientConfig::new();
        client_config
            .set("bootstrap.servers", &config.kafka_brokers)
            .set("group.id", &config.consumer_group)
            .set("enable.auto.commit", "true")
            .set("auto.offset.reset", "latest");

        // Add any additional properties
        if let Some(props) = &config.kafka_properties {
            for (key, value) in props {
                client_config.set(key, value);
            }
        }

        let consumer: StreamConsumer = client_config.create()?;
        consumer.subscribe(&[&config.kafka_topic])?;

        info!(
            topic = %config.kafka_topic,
            brokers = %config.kafka_brokers,
            "Starting Kafka consumer for UserOperations"
        );

        // Spawn background consumer task
        tokio::spawn(async move {
            run_consumer(consumer, mempool, shutdown_rx).await;
        });

        self.shutdown_tx = Some(shutdown_tx);
        Ok(())
    }

    /// Stop the Kafka consumer
    #[cfg(feature = "kafka")]
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Run the Kafka consumer loop
#[cfg(feature = "kafka")]
async fn run_consumer(
    consumer: StreamConsumer,
    mempool: SharedMempool,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    info!("Kafka consumer started");

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!("Kafka consumer shutting down");
                break;
            }
            message = consumer.recv() => {
                match message {
                    Ok(msg) => {
                        if let Some(payload) = msg.payload() {
                            match process_message(payload, &mempool) {
                                Ok(hash) => {
                                    debug!(
                                        hash = %hash,
                                        partition = msg.partition(),
                                        offset = msg.offset(),
                                        "Added UserOperation to mempool"
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        error = %e,
                                        partition = msg.partition(),
                                        offset = msg.offset(),
                                        "Failed to process UserOperation message"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Error receiving Kafka message");
                        // Brief pause before retrying
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    info!("Kafka consumer stopped");
}

/// Process a single Kafka message containing a UserOperation
#[cfg(feature = "kafka")]
fn process_message(payload: &[u8], mempool: &SharedMempool) -> Result<B256> {
    // Deserialize the UserOperation
    let user_op: VersionedUserOperation = serde_json::from_slice(payload)?;

    // Compute the hash
    let hash = compute_user_op_hash(&user_op);

    // Wrap the operation
    let wrapped = WrappedUserOperation {
        operation: user_op,
        hash,
    };

    // Add to mempool
    let mut pool = mempool.write();
    pool.add_operation(&wrapped)?;

    Ok(hash)
}

/// Compute the hash of a UserOperation
/// TODO: This should use the proper ERC-4337 hash computation
#[cfg(feature = "kafka")]
fn compute_user_op_hash(op: &VersionedUserOperation) -> UserOpHash {
    use sha3::{Digest, Keccak256};

    // Simple hash for now - should be replaced with proper ERC-4337 hash
    let encoded = serde_json::to_vec(op).unwrap_or_default();
    let hash = Keccak256::digest(&encoded);
    B256::from_slice(&hash)
}

/// Wrapper to use SharedMempool with the bundler
pub struct SharedMempoolClient {
    mempool: SharedMempool,
}

impl SharedMempoolClient {
    pub fn new(mempool: SharedMempool) -> Self {
        Self { mempool }
    }

    /// Get top N operations from the mempool
    pub fn get_top_operations(&self, n: usize) -> Vec<Arc<WrappedUserOperation>> {
        let pool = self.mempool.read();
        pool.get_top_operations(n).collect()
    }

    /// Remove an operation from the mempool
    pub fn remove_operation(&self, hash: &UserOpHash) -> Result<()> {
        let mut pool = self.mempool.write();
        pool.remove_operation(hash)?;
        Ok(())
    }

    /// Check if the mempool has any operations
    pub fn is_empty(&self) -> bool {
        let pool = self.mempool.read();
        pool.get_top_operations(1).next().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mempool_service_creation() {
        let service = MempoolService::new_without_kafka(1_000_000_000);
        let mempool = service.mempool();

        // Mempool should be empty initially
        let pool = mempool.read();
        assert!(pool.get_top_operations(10).next().is_none());
    }

    #[test]
    fn test_shared_mempool_client() {
        let service = MempoolService::new_without_kafka(1_000_000_000);
        let client = SharedMempoolClient::new(service.mempool());

        assert!(client.is_empty());
        assert!(client.get_top_operations(10).is_empty());
    }
}

