use crate::{
    builders::{BuilderConfig, OpPayloadBuilderCtx, flashblocks::FlashblocksConfig},
    bundler::SharedMempool,
    gas_limiter::{AddressGasLimiter, args::GasLimiterArgs},
    metrics::OpRBuilderMetrics,
    resource_metering::ResourceMetering,
    traits::ClientBounds,
    tx_signer::Signer,
};
use op_revm::OpSpecId;
use reth_basic_payload_builder::PayloadConfig;
use reth_evm::EvmEnv;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_payload_builder::{
    OpPayloadBuilderAttributes,
    config::{OpDAConfig, OpGasLimitConfig},
};
use reth_optimism_primitives::OpTransactionSigned;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub(super) struct OpPayloadSyncerCtx {
    /// The type that knows how to perform system calls and configure the evm.
    evm_config: OpEvmConfig,
    /// The DA config for the payload builder
    da_config: OpDAConfig,
    /// The chainspec
    chain_spec: Arc<OpChainSpec>,
    /// Max gas that can be used by a transaction.
    max_gas_per_txn: Option<u64>,
    /// The metrics for the builder
    metrics: Arc<OpRBuilderMetrics>,
    /// Resource metering tracking
    resource_metering: ResourceMetering,
    /// Whether AA bundling is enabled
    aa_bundler_enabled: bool,
    /// AA bundler signer for bundle transactions
    aa_bundler_signer: Option<Signer>,
    /// Gas threshold percentage (when to start bundling)
    aa_gas_threshold: u8,
    /// Gas reserve percentage (how much gas to reserve for bundles)
    aa_gas_reserve: u8,
    /// Shared mempool for AA UserOperations
    aa_mempool: Option<SharedMempool>,
}

impl OpPayloadSyncerCtx {
    pub(super) fn new<Client>(
        client: &Client,
        builder_config: BuilderConfig<FlashblocksConfig>,
        evm_config: OpEvmConfig,
        metrics: Arc<OpRBuilderMetrics>,
    ) -> eyre::Result<Self>
    where
        Client: ClientBounds,
    {
        let chain_spec = client.chain_spec();
        Ok(Self {
            evm_config,
            da_config: builder_config.da_config.clone(),
            chain_spec,
            max_gas_per_txn: builder_config.max_gas_per_txn,
            metrics,
            resource_metering: builder_config.resource_metering.clone(),
            aa_bundler_enabled: builder_config.enable_aa_bundler,
            aa_bundler_signer: builder_config.aa_bundler_signer,
            aa_gas_threshold: builder_config.aa_gas_threshold,
            aa_gas_reserve: builder_config.aa_gas_reserve_percentage,
            aa_mempool: builder_config.aa_mempool.clone(),
        })
    }

    pub(super) fn evm_config(&self) -> &OpEvmConfig {
        &self.evm_config
    }

    pub(super) fn max_gas_per_txn(&self) -> Option<u64> {
        self.max_gas_per_txn
    }

    pub(super) fn into_op_payload_builder_ctx(
        self,
        payload_config: PayloadConfig<OpPayloadBuilderAttributes<OpTransactionSigned>>,
        evm_env: EvmEnv<OpSpecId>,
        block_env_attributes: OpNextBlockEnvAttributes,
        cancel: CancellationToken,
    ) -> OpPayloadBuilderCtx {
        OpPayloadBuilderCtx {
            evm_config: self.evm_config,
            da_config: self.da_config,
            gas_limit_config: OpGasLimitConfig::default(),
            chain_spec: self.chain_spec,
            config: payload_config,
            evm_env,
            block_env_attributes,
            cancel,
            builder_signer: None,
            metrics: self.metrics,
            extra_ctx: (),
            max_gas_per_txn: self.max_gas_per_txn,
            address_gas_limiter: AddressGasLimiter::new(GasLimiterArgs::default()),
            resource_metering: self.resource_metering.clone(),
            aa_bundler_enabled: self.aa_bundler_enabled,
            aa_bundler_signer: self.aa_bundler_signer,
            aa_gas_threshold: self.aa_gas_threshold,
            aa_gas_reserve: self.aa_gas_reserve,
            aa_mempool: self.aa_mempool.clone(),
        }
    }
}
