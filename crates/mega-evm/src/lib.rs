//! The EVM implementation for the `MegaETH` Satin engine.
//!
//! Satin runs a single spec, [`MegaSpecId::SATIN`], on op-revm's Karst handler with EIP-8037
//! state gas and the EIP-2780 intrinsic cost. The legacy engine (specs `Equivalence` through
//! `Rex6`) is a separate crate line; nothing here executes a legacy spec.
//!
//! A volatile read that `MegaAccessControl`'s switch refuses reverts its frame with
//! `VolatileDataAccessDisabled(uint8 accessType)`. A refused `SLOTNUM` names access type 12
//! ([`system::SLOT_NUM_ACCESS_TYPE`]), past the last variant of the contract's
//! `VolatileDataAccessType`, so a Solidity handler must decode the argument as `uint8`;
//! [`decode_volatile_data_access_disabled`] decodes it on the Rust side.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

mod access;
mod block;
pub mod constants;
mod evm;
mod external;
mod limit;
pub mod system;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
mod types;

pub use access::{
    decode_volatile_data_access_disabled, volatile_data_access_disabled_revert_data, Detention,
    VolatileDataAccess,
};
pub use block::{
    chain_activation, estimated_da_size, hardfork_schedule, mainnet_hardforks,
    pre_block_call_gas_limit, read_l1_block_info, testnet_hardforks,
    transact_beacon_root_contract_call, transact_blockhashes_contract_call, BlockGasCounters,
    BlockLimiter, BlockLimits, BlockUsage, ChainActivation, EnrichedMegaTx, MegaBlockExecutionCtx,
    MegaBlockExecutionError, MegaBlockExecutionResult, MegaBlockExecutor, MegaBlockExecutorFactory,
    MegaBlockLimitExceededError, MegaBlockTxResult, MegaFinishedBlock, MegaHardfork,
    MegaHardforkConfig, MegaHardforks, MegaTransactionExt, MegaTxLimitExceededError,
    ProtocolLimits, SatinChainConfig, SatinChainConfigError, ScheduleError, CHAIN_ACTIVATIONS,
    L1_BLOCK_INFO_SLOTS, MAINNET_CHAIN_ID, SATIN_CHAIN_CONFIG_KEYS, SATIN_TIME_KEY, SYSTEM_ADDRESS,
    TESTNET_CHAIN_ID,
};
#[cfg(feature = "satin-price-override")]
pub use evm::install_satin_prices;
pub use evm::{
    active_satin_prices, history_gas, kzg_point_evaluation, name, satin_gas_params,
    satin_gas_params_at, satin_gas_params_history_exempt, satin_precompiles, satin_precompiles_map,
    settle_frame_result, spec_cfg, storage_call_stipend, synthetic_call_result,
    synthetic_frame_result, transaction_body_bytes, tx_body_history_bytes, untouched_call_gas,
    untouched_create_gas, validate_transaction_stateless, with_pools_of, write_record_history_gas,
    BlockHashRecord, BucketRecord, BytePrice, DeclaredObserver, DynPrecompilesBuilder, EthSpecId,
    IntrinsicGas, MegaContext, MegaEvm, MegaEvmError, MegaEvmFactory, MegaGasUsage, MegaHandler,
    MegaSpecId, MegaTransactionOutcome, OpSpecId, OracleRead, OracleReadRecord, ParseMegaSpecError,
    SatinPriceError, SatinPrices, TrustedObserver, CPHB_ENV_VAR, CPSB_ENV_VAR, HISTORY_GAS_PRICED,
    LEGACY_SPEC_NAMES, STATE_GAS_REPRICED, STORAGE_CALL_STIPEND_BYTES,
    SYSTEM_CALL_REGULAR_GAS_LIMIT,
};
#[cfg(any(test, feature = "test-utils"))]
pub use external::{
    AHashBucketHasher, BucketHasher, RecordedHint, SimpleBucketHasher, TestExternalEnvs,
};
pub use external::{
    BucketError, BucketId, BucketMultipliers, EmptyExternalEnv, ExternalEnvFactory,
    ExternalEnvTypes, ExternalEnvs, OracleEnv, SaltEnv, MIN_BUCKET_SIZE, MIN_BUCKET_SIZE_BITS,
};
pub use limit::{
    decode_mega_limit_exceeded, log_history_bytes, AdditionalLimit, EvmTxRuntimeLimits, LimitCheck,
    LimitKind, LimitStop, LimitUsage, MegaLimitExceeded, StagedRecord, ACCESS_LIST_ADDRESS_SIZE,
    ACCESS_LIST_SLOT_SIZE, AUTHORIZATION_SIZE, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, LOG_BASE_SIZE, LOG_TOPIC_SIZE, TRANSFER_LOG_SIZE, TX_BASE_SIZE,
    TX_BODY_OVER_DATA_SIZE_LIMIT, TX_BODY_SIZE, TX_FIXED_WRITE_RECORDS, WRITE_RECORD_SIZE,
};
pub use types::{
    MegaHaltReason, MegaTransaction, MegaTransactionError, MegaTxEnvelope, MegaTxType,
};

/* Items the crate's modules share through the root, not part of the public surface */
pub(crate) use block::transact_pre_block_call;
pub(crate) use evm::{
    charge_write_records_history, MegaInnerContext, MegaInstructions, PricedPrecompiles,
};

/* Re-export of upstream crates, so consumers build against the exact versions used here */
pub use alloy_consensus;
pub use alloy_evm;
pub use alloy_hardforks;
pub use alloy_op_evm;
pub use alloy_primitives;
pub use alloy_sol_types;
pub use op_alloy_consensus;
pub use op_revm;
pub use revm;
