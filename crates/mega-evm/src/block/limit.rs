//! What a block admits, and what it counts of the transactions it packed.
//!
//! The limits come from two places, by who must agree on them:
//!
//! - [`ProtocolLimits`] are the limits every node holds a chain's blocks and transactions to,
//!   because a different value is a different result: the per-transaction limits the EVM enforces
//!   and the block's data-size budget. They are fixed per spec, not configured:
//!   [`ProtocolLimits::for_spec`] is the one table of them, which block execution, the EVM factory
//!   and a bare [`MegaContext`](crate::MegaContext) all read.
//! - [`BlockLimits`] is the building policy a node passes in the block execution context: the
//!   limits only a builder applies — a transaction's declared gas, the encoded sizes and the
//!   data-availability sizes — and a cap a builder may put on the block's data-size budget below
//!   the protocol's. It never loosens a protocol limit ([`BlockLimits::within`]), and a validator
//!   leaves it at its default, which restricts nothing.
//!
//! [`BlockLimiter`] is the state one block keeps while it executes, held to both.
//!
//! # When each limit is checked
//!
//! A limit known before execution is checked before the transaction runs, and refuses it:
//! its own gas limit, encoded size and data-availability size, and what each of those would add
//! to the block.
//!
//! The one limit only known after execution — the data-size bytes a transaction kept — is
//! accumulated when the transaction commits. The transaction that crosses it is therefore still
//! packed, and the ones after it are refused; this is what keeps a block full rather than dropping
//! the work already done. A block whose counter has reached its limit refuses every later
//! transaction before it starts — a deposit excepted — so the overshoot is bounded by one
//! transaction, besides what deposits add.
//!
//! # Deposits
//!
//! A deposit is an L1 message the chain cannot censor: the block derived from L1 must include it,
//! and the builder does not choose it. So no limit only a builder applies refuses a deposit — its
//! declared gas, its encoded size and its data-availability size, and what those add to the block
//! — and a deposit does not count towards the block's data-availability size. The data-size limit
//! is a packing budget for the transactions the builder chooses, so it does not refuse a deposit
//! either. A deposit still counts towards it, and towards the block's encoded size, so the
//! transactions after it find the room it used. The one limit that holds a deposit is
//! the block's gas limit, which is the header's.
//!
//! The per-transaction limits the block installs on the EVM are not block limits: they hold a
//! deposit's own execution the way they hold any transaction's, and a deposit that crosses one is
//! included with the stop as its result.
//!
//! # Which dimensions are enforced
//!
//! The three gas ledgers the block counts ([`BlockGasCounters`]) have no block limit beside the
//! block's gas limit, the header's: state gas prices the state a block adds, so the block's gas
//! limit bounds it, and the history bytes beside the history ledger are reported, not limited. The
//! write-record count, which is the block's KV count, is reported and has no limit: every record
//! weighs forty bytes of data size, so a block held to
//! [`BLOCK_DATA_LIMIT`](crate::constants::BLOCK_DATA_LIMIT) keeps at most 327,680 records.

use alloy_primitives::TxHash;

#[cfg(not(feature = "std"))]
use alloc as std;
use std::boxed::Box;

use alloy_evm::block::{BlockExecutionError, BlockValidationError};

use crate::{
    constants::{
        BLOCK_DATA_LIMIT, BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS, TX_DATA_LIMIT,
    },
    BlockGasCounters, EvmTxRuntimeLimits, LimitUsage, MegaBlockLimitExceededError, MegaGasUsage,
    MegaSpecId, MegaTxLimitExceededError,
};

/// The limits the protocol holds every block and every transaction to, fixed per spec.
///
/// Each of them changes what a validator computes when it re-executes a block, so they are not
/// configuration: [`for_spec`](Self::for_spec) is the one table of them, keyed by the spec a block
/// executes, and every node of every chain holds a block of that spec to the same values. Block
/// execution, the EVM factory and a bare [`MegaContext`](crate::MegaContext) read it, so an
/// execution outside a block — a call, a gas estimate, a simulation — is held to what block
/// execution holds it to without being told.
///
/// - [`tx_runtime_limits`](Self::tx_runtime_limits) holds each transaction's own execution: a
///   transaction that crosses one is stopped, or a frame reverted, and that is the result its
///   receipt records. There is no tighter setting a builder could run a transaction under and still
///   agree with a validator; a builder that wants to avoid such a transaction leaves it out.
/// - The block's data-size budget holds what a block's transactions keep together. A validator
///   refuses a block that packs a transaction after the block reached it; a builder may pack
///   tighter ([`BlockLimits`]), never looser.
///
/// A tool or a test that runs under other values says so explicitly
/// ([`MegaContext::with_tx_runtime_limits`](crate::MegaContext::with_tx_runtime_limits),
/// [`MegaEvmFactory::with_tx_runtime_limits`](crate::MegaEvmFactory::with_tx_runtime_limits), and
/// in block execution `MegaBlockExecutor::with_protocol_limits`, which exists for tests and tools
/// only); what it computes is then a counterfactual, not the chain's result.
///
/// It serializes in the shape a tool takes an override in: camelCase, every field required, no
/// other field accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtocolLimits {
    /// The limits every transaction runs under, which the block executor installs on the EVM:
    /// data size, per transaction and per frame, and gas detention's two caps.
    pub tx_runtime_limits: EvmTxRuntimeLimits,
    /// The most data-size bytes a block's transactions may keep together. The transaction that
    /// reaches it is packed; after it, only a deposit is.
    pub block_txs_data_limit: u64,
}

impl ProtocolLimits {
    /// The limits a block of `spec`, and every transaction in it, is held to.
    ///
    /// [`MegaSpecId::SATIN`]: a transaction keeps at most [`TX_DATA_LIMIT`] (13,107,200) bytes of
    /// data size, and a block's transactions together at most [`BLOCK_DATA_LIMIT`] (13,107,200);
    /// every frame gets 98/100 of what its parent has left, the transaction's own frame what the
    /// transaction has left, with no further per-frame cap; a read of volatile data caps the
    /// transaction's compute at its compute at the read plus [`BLOCK_ENV_ACCESS_COMPUTE_GAS`] or
    /// [`ORACLE_ACCESS_COMPUTE_GAS`] (20,000,000 each). Provisional, as the constants are, until
    /// the economics sign-off fixes them.
    ///
    /// Every field is written out, so a limit added later cannot be left unlimited by omission.
    pub const fn for_spec(spec: MegaSpecId) -> Self {
        match spec {
            MegaSpecId::SATIN => Self {
                tx_runtime_limits: EvmTxRuntimeLimits {
                    tx_data_size_limit: TX_DATA_LIMIT,
                    frame_data_size_limit: u64::MAX,
                    block_env_access_compute_gas_limit: BLOCK_ENV_ACCESS_COMPUTE_GAS,
                    oracle_access_compute_gas_limit: ORACLE_ACCESS_COMPUTE_GAS,
                },
                block_txs_data_limit: BLOCK_DATA_LIMIT,
            },
        }
    }

    /// No limit at all, gas detention's caps included, so no transaction is detained
    /// ([`EvmTxRuntimeLimits::no_limits`]).
    ///
    /// For runs that take every limit out — the execution-spec gate's equivalence mode and tests —
    /// never for executing the chain.
    pub const fn no_limits() -> Self {
        Self { tx_runtime_limits: EvmTxRuntimeLimits::no_limits(), block_txs_data_limit: u64::MAX }
    }

    /// Sets the limits every transaction runs under.
    pub const fn with_tx_runtime_limits(mut self, limits: EvmTxRuntimeLimits) -> Self {
        self.tx_runtime_limits = limits;
        self
    }

    /// Sets the block's data-size limit.
    pub const fn with_block_txs_data_limit(mut self, limit: u64) -> Self {
        self.block_txs_data_limit = limit;
        self
    }
}

/// The building policy one block is packed under: the limits only a builder applies, and the cap
/// it may put on the block's data-size budget below the protocol's.
///
/// A node passes it in the block execution context. None of it is a protocol value: the
/// protocol's limits are [`ProtocolLimits`], which the block executor reads from the spec's
/// table, and this can only make a block tighter than they do ([`within`](Self::within)). So a
/// builder packs under it and a validator re-executing the block leaves it at [`Default`], which
/// restricts nothing: every block a builder packed under a policy fits the protocol's limits, and
/// a validator that applied the builder's policy too would only refuse blocks, never accept one
/// the protocol refuses.
///
/// - [`tx_gas_limit`](Self::tx_gas_limit), the two encoded-size limits and the two
///   data-availability size limits are the builder's alone: the protocol holds a transaction's
///   declared gas to the block's gas limit, and what a block writes to its data-size budget, which
///   counts calldata.
/// - [`block_txs_data_limit`](Self::block_txs_data_limit) is the builder's cap on the protocol's
///   data-size budget: the block executor holds a block to the smaller of the two.
/// - [`block_gas_limit`](Self::block_gas_limit) is the header's: the block executor always sets it
///   from the block environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockLimits {
    /// The most gas a single transaction may declare. Deposits are exempt.
    pub tx_gas_limit: u64,
    /// The block's gas limit: a transaction is admitted only if its declared gas limit fits in
    /// what the block has left. Also the budget the data-availability footprint of the block's
    /// transactions is held to.
    pub block_gas_limit: u64,
    /// The most bytes a single transaction's EIP-2718 encoding may take. Deposits are exempt.
    pub tx_encode_size_limit: u64,
    /// The most bytes the block's transaction bodies may take together, uncompressed. Deposits are
    /// exempt, and count towards it.
    pub block_txs_encode_size_limit: u64,
    /// The most data-availability bytes a single transaction may take. Deposits are exempt.
    pub tx_da_size_limit: u64,
    /// The most data-availability bytes the block's transactions may take together. Deposits are
    /// exempt and do not count towards it.
    pub block_da_size_limit: u64,
    /// The most data-size bytes the block's transactions may keep together. The transaction that
    /// reaches it is packed; after it, only a deposit is, which it never refuses and which counts
    /// towards it.
    ///
    /// A builder's cap: the block is held to the smaller of this and
    /// [`ProtocolLimits::block_txs_data_limit`].
    pub block_txs_data_limit: u64,
}

impl Default for BlockLimits {
    /// No restriction: what a validator passes.
    fn default() -> Self {
        Self::no_limits()
    }
}

impl BlockLimits {
    /// No restriction at all: every limit unlimited, so a block is held to the protocol's limits
    /// alone.
    pub const fn no_limits() -> Self {
        Self {
            tx_gas_limit: u64::MAX,
            block_gas_limit: u64::MAX,
            tx_encode_size_limit: u64::MAX,
            block_txs_encode_size_limit: u64::MAX,
            tx_da_size_limit: u64::MAX,
            block_da_size_limit: u64::MAX,
            block_txs_data_limit: u64::MAX,
        }
    }

    /// These limits held within `protocol`: the block's data-size budget is the smaller of this
    /// policy's cap and the protocol's limit, so a builder's setting can tighten the budget and
    /// never loosen it. The limits only a builder applies are kept as they are.
    ///
    /// A cap above the protocol's limit gives way to it without a word; a node that wants to
    /// refuse or report such a setting compares the two itself.
    pub fn within(self, protocol: &ProtocolLimits) -> Self {
        Self {
            block_txs_data_limit: self.block_txs_data_limit.min(protocol.block_txs_data_limit),
            ..self
        }
    }

    /// Sets the per-transaction gas limit.
    pub const fn with_tx_gas_limit(mut self, limit: u64) -> Self {
        self.tx_gas_limit = limit;
        self
    }

    /// Sets the block's gas limit.
    ///
    /// The block executor overwrites it with the block environment's gas limit, which is the
    /// number consensus holds the block to.
    pub const fn with_block_gas_limit(mut self, limit: u64) -> Self {
        self.block_gas_limit = limit;
        self
    }

    /// Sets the per-transaction encoded-size limit.
    pub const fn with_tx_encode_size_limit(mut self, limit: u64) -> Self {
        self.tx_encode_size_limit = limit;
        self
    }

    /// Sets the block's encoded-size limit.
    pub const fn with_block_txs_encode_size_limit(mut self, limit: u64) -> Self {
        self.block_txs_encode_size_limit = limit;
        self
    }

    /// Sets the per-transaction data-availability size limit.
    pub const fn with_tx_da_size_limit(mut self, limit: u64) -> Self {
        self.tx_da_size_limit = limit;
        self
    }

    /// Sets the block's data-availability size limit.
    pub const fn with_block_da_size_limit(mut self, limit: u64) -> Self {
        self.block_da_size_limit = limit;
        self
    }

    /// Caps the block's data size.
    pub const fn with_block_txs_data_limit(mut self, limit: u64) -> Self {
        self.block_txs_data_limit = limit;
        self
    }

    /// A limiter that holds a block to these limits.
    pub const fn to_block_limiter(self) -> BlockLimiter {
        BlockLimiter::new(self)
    }
}

/// What one committed transaction adds to a block's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockUsage {
    /// The gas the receipt reports.
    pub gas_used: u64,
    /// The gas by ledger.
    pub gas: MegaGasUsage,
    /// The data-size bytes and write records the transaction kept.
    pub usage: LimitUsage,
    /// The transaction's EIP-2718 encoded size, in bytes.
    pub tx_size: u64,
    /// The transaction's data-availability size, in bytes.
    pub da_size: u64,
    /// The transaction's data-availability footprint, in gas.
    pub da_footprint: u64,
    /// Whether the transaction is a deposit, which the data-availability dimensions exempt and
    /// which no limit but the block's gas limit refuses.
    pub is_deposit: bool,
}

/// What a block has used, and the limits it is held to.
///
/// The counters are cumulative over the transactions the block committed; a transaction that was
/// executed and then discarded leaves them untouched.
#[derive(Clone, Debug)]
pub struct BlockLimiter {
    /// The limits this block is held to.
    pub limits: BlockLimits,
    /// The gas the block's receipts report, which is the block header's gas used.
    pub block_gas_used: u64,
    /// The gas the block's transactions spent, by ledger.
    pub gas: BlockGasCounters,
    /// The data-size bytes and write records the block's transactions kept.
    pub usage: LimitUsage,
    /// The encoded size of the block's transaction bodies, in bytes.
    pub block_tx_size_used: u64,
    /// The data-availability size of the block's non-deposit transactions, in bytes.
    pub block_da_size_used: u64,
    /// The data-availability footprint of the block's non-deposit transactions, in gas. This is
    /// what the block reports as its blob gas used.
    pub block_da_footprint_used: u64,
}

impl BlockLimiter {
    /// A limiter with every counter at zero.
    pub const fn new(limits: BlockLimits) -> Self {
        Self {
            limits,
            block_gas_used: 0,
            gas: BlockGasCounters { execution: 0, state: 0, history: 0, history_bytes: 0 },
            usage: LimitUsage { data_size: 0, write_records: 0 },
            block_tx_size_used: 0,
            block_da_size_used: 0,
            block_da_footprint_used: 0,
        }
    }

    /// The gas the block still has for a transaction's declared gas limit.
    pub const fn available_gas(&self) -> u64 {
        self.limits.block_gas_limit.saturating_sub(self.block_gas_used)
    }

    /// The data-availability footprint the block still has. The budget is the block's gas limit,
    /// as the fork's rule states it.
    pub const fn available_da_footprint(&self) -> u64 {
        self.limits.block_gas_limit.saturating_sub(self.block_da_footprint_used)
    }

    /// Whether `tx` may execute in this block.
    ///
    /// Checks the transaction against its own limits, and against what the block has left; a
    /// deposit is held to the block's gas limit alone — to no limit only a builder applies, and not
    /// to the data-size limit. It reads the counters and changes nothing;
    /// [`post_execution_update`](Self::post_execution_update) advances the counters once the
    /// transaction commits.
    ///
    /// # Errors
    ///
    /// A [`MegaTxLimitExceededError`] when the transaction exceeds a limit of its own — it can
    /// never be included, in this block or any other — and a [`MegaBlockLimitExceededError`]
    /// when the block has no room left for it, which a builder answers by trying the next
    /// transaction.
    pub fn pre_execution_check(
        &self,
        tx_hash: TxHash,
        gas_limit: u64,
        tx_size: u64,
        da_size: u64,
        is_deposit: bool,
    ) -> Result<(), BlockExecutionError> {
        // A deposit is an L1 message the chain cannot censor: the block derived from L1 must
        // include it, and the builder does not choose it. So no limit only a builder applies
        // refuses it — its declared gas, its encoded size and its data-availability size, and what
        // those add to the block — and it does not count towards the block's data-availability
        // size. The block's gas limit, the header's, holds it as it holds every transaction.
        if !is_deposit && gas_limit > self.limits.tx_gas_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaTxLimitExceededError::TransactionGasLimit {
                    tx_gas_limit: gas_limit,
                    limit: self.limits.tx_gas_limit,
                },
            ));
        }

        if self.block_gas_used.saturating_add(gas_limit) > self.limits.block_gas_limit {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: gas_limit,
                block_available_gas: self.available_gas(),
            }
            .into());
        }

        // The packing budget below, the data size, is a dimension a transaction's own execution
        // reveals. The transaction that crossed it is already packed, so what it refuses is the
        // next one — never a deposit, which is not the builder's to refuse either. Every committed
        // transaction counts towards it, a deposit included, in `post_execution_update`.
        if is_deposit {
            return Ok(());
        }

        if tx_size > self.limits.tx_encode_size_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaTxLimitExceededError::TransactionEncodeSizeLimit {
                    tx_size,
                    limit: self.limits.tx_encode_size_limit,
                },
            ));
        }

        if tx_size.saturating_add(self.block_tx_size_used) > self.limits.block_txs_encode_size_limit
        {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::TransactionEncodeSizeLimit {
                    block_used: self.block_tx_size_used,
                    tx_used: tx_size,
                    limit: self.limits.block_txs_encode_size_limit,
                },
            ));
        }

        if da_size > self.limits.tx_da_size_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaTxLimitExceededError::DataAvailabilitySizeLimit {
                    da_size,
                    limit: self.limits.tx_da_size_limit,
                },
            ));
        }

        if da_size.saturating_add(self.block_da_size_used) > self.limits.block_da_size_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::DataAvailabilitySizeLimit {
                    block_used: self.block_da_size_used,
                    tx_used: da_size,
                    limit: self.limits.block_da_size_limit,
                },
            ));
        }

        if self.usage.data_size >= self.limits.block_txs_data_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::TransactionDataLimit {
                    block_used: self.usage.data_size,
                    limit: self.limits.block_txs_data_limit,
                },
            ));
        }

        // `self.gas` and `self.usage.write_records` are accumulated and not checked: none of them
        // has a block limit.

        Ok(())
    }

    /// Adds what one committed transaction used.
    ///
    /// Every counter saturates: a block that has already crossed a limit stays crossed, and the
    /// check before the next transaction sees it.
    pub fn post_execution_update(&mut self, usage: &BlockUsage) {
        self.block_gas_used = self.block_gas_used.saturating_add(usage.gas_used);
        self.gas.record(&usage.gas);
        self.usage.data_size = self.usage.data_size.saturating_add(usage.usage.data_size);
        self.usage.write_records =
            self.usage.write_records.saturating_add(usage.usage.write_records);
        self.block_tx_size_used = self.block_tx_size_used.saturating_add(usage.tx_size);
        if !usage.is_deposit {
            self.block_da_size_used = self.block_da_size_used.saturating_add(usage.da_size);
            self.block_da_footprint_used =
                self.block_da_footprint_used.saturating_add(usage.da_footprint);
        }
    }
}

/// Wraps a limit error as the rejection of one transaction.
fn invalid_tx(
    hash: TxHash,
    error: impl alloy_evm::InvalidTxError + 'static,
) -> BlockExecutionError {
    BlockValidationError::InvalidTx { hash, error: Box::new(error) }.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{constants::MAX_TX_COMPUTE_GAS, TX_BODY_SIZE};
    use alloy_primitives::B256;

    /// Satin's limits, written out so a change to one is a visible diff: 13,107,200 bytes of data
    /// size per transaction and per block, no per-frame cap beyond the 98/100 share, and
    /// detention's caps at 20,000,000 each.
    ///
    /// They also hold what a chain configuration's load-time check used to hold a chain's values
    /// to: no limit is zero, a transaction's data size holds at least a body, and both detention
    /// caps are below the most compute a transaction can spend, so a read of volatile data is
    /// detained.
    #[test]
    fn test_the_satin_limits_are_written_out() {
        let limits = ProtocolLimits::for_spec(MegaSpecId::SATIN);
        assert_eq!(
            limits,
            ProtocolLimits {
                tx_runtime_limits: EvmTxRuntimeLimits {
                    tx_data_size_limit: 13_107_200,
                    frame_data_size_limit: u64::MAX,
                    block_env_access_compute_gas_limit: 20_000_000,
                    oracle_access_compute_gas_limit: 20_000_000,
                },
                block_txs_data_limit: 13_107_200,
            }
        );

        let EvmTxRuntimeLimits {
            tx_data_size_limit,
            frame_data_size_limit,
            block_env_access_compute_gas_limit,
            oracle_access_compute_gas_limit,
        } = limits.tx_runtime_limits;
        for limit in [
            tx_data_size_limit,
            frame_data_size_limit,
            block_env_access_compute_gas_limit,
            oracle_access_compute_gas_limit,
            limits.block_txs_data_limit,
        ] {
            assert_ne!(limit, 0);
        }
        assert!(tx_data_size_limit >= TX_BODY_SIZE, "a transaction's data size holds its body");
        assert!(block_env_access_compute_gas_limit < MAX_TX_COMPUTE_GAS, "a cap that binds");
        assert!(oracle_access_compute_gas_limit < MAX_TX_COMPUTE_GAS, "a cap that binds");
    }

    /// The limits serialize in the shape a tool takes an override in, written out here: camelCase,
    /// the per-transaction limits nested. A field left out is an error rather than a zero, and so
    /// is one the type does not have.
    #[test]
    fn test_protocol_limits_round_trip() {
        let satin = ProtocolLimits::for_spec(MegaSpecId::SATIN);
        let limits = satin
            .with_tx_runtime_limits(satin.tx_runtime_limits.with_frame_data_size_limit(1))
            .with_block_txs_data_limit(7);

        let json = serde_json::to_string(&limits).expect("serializes");
        assert_eq!(
            json,
            r#"{"txRuntimeLimits":{"txDataSizeLimit":13107200,"frameDataSizeLimit":1,"blockEnvAccessComputeGasLimit":20000000,"oracleAccessComputeGasLimit":20000000},"blockTxsDataLimit":7}"#
        );
        assert_eq!(serde_json::from_str::<ProtocolLimits>(&json).expect("deserializes"), limits);

        let missing = json.replace(r#","blockTxsDataLimit":7"#, "");
        assert!(serde_json::from_str::<ProtocolLimits>(&missing).is_err(), "{missing}");
        let missing_tx = json.replace(r#""frameDataSizeLimit":1,"#, "");
        assert!(serde_json::from_str::<ProtocolLimits>(&missing_tx).is_err(), "{missing_tx}");
        let unknown = json.replace(r#""blockTxsDataLimit":7"#, r#""blockTxsDataLimit":7,"x":1"#);
        assert!(serde_json::from_str::<ProtocolLimits>(&unknown).is_err(), "{unknown}");
    }

    /// `no_limits` leaves every limit unlimited, detention's caps included. Every field is named,
    /// so a limit added later cannot be left out of it.
    #[test]
    fn test_no_protocol_limits_leaves_every_limit_unlimited() {
        let ProtocolLimits { tx_runtime_limits, block_txs_data_limit } =
            ProtocolLimits::no_limits();
        assert_eq!(tx_runtime_limits, EvmTxRuntimeLimits::no_limits());
        assert_eq!(block_txs_data_limit, u64::MAX);
    }

    /// The default building policy restricts nothing, so a validator that leaves it holds a block
    /// to the protocol's limits alone. Every field is named, so a limit added later cannot be left
    /// out of it.
    #[test]
    fn test_the_default_building_policy_restricts_nothing() {
        assert_eq!(BlockLimits::default(), BlockLimits::no_limits());
        let BlockLimits {
            tx_gas_limit,
            block_gas_limit,
            tx_encode_size_limit,
            block_txs_encode_size_limit,
            tx_da_size_limit,
            block_da_size_limit,
            block_txs_data_limit,
        } = BlockLimits::default();
        for limit in [
            tx_gas_limit,
            block_gas_limit,
            tx_encode_size_limit,
            block_txs_encode_size_limit,
            tx_da_size_limit,
            block_da_size_limit,
            block_txs_data_limit,
        ] {
            assert_eq!(limit, u64::MAX);
        }
        assert_eq!(
            BlockLimits::default()
                .within(&ProtocolLimits::for_spec(MegaSpecId::SATIN))
                .block_txs_data_limit,
            crate::constants::BLOCK_DATA_LIMIT,
            "held within the protocol's, the default policy is the protocol's"
        );
    }

    /// A builder's cap on the block's data-size budget holds the block only where it is below the
    /// protocol's limit: it tightens, and never loosens. The limits only a builder applies pass
    /// through.
    #[test]
    fn test_a_building_policy_tightens_a_budget_and_never_loosens_it() {
        let protocol = ProtocolLimits::for_spec(MegaSpecId::SATIN).with_block_txs_data_limit(300);

        let tighter = BlockLimits::no_limits()
            .with_block_txs_data_limit(30)
            .with_tx_gas_limit(1)
            .with_tx_encode_size_limit(2)
            .with_block_txs_encode_size_limit(3)
            .with_tx_da_size_limit(4)
            .with_block_da_size_limit(5)
            .with_block_gas_limit(6);
        assert_eq!(tighter.within(&protocol), tighter, "every cap is below the protocol's");

        let looser = BlockLimits::no_limits().with_block_txs_data_limit(3_000);
        let held = looser.within(&protocol);
        assert_eq!(held.block_txs_data_limit, 300, "a cap above the protocol's gives way to it");
        assert_eq!(
            BlockLimits { tx_gas_limit: 7, ..held },
            BlockLimits { tx_gas_limit: 7, ..BlockLimits::no_limits().within(&protocol) },
            "nothing else changes"
        );
    }

    fn limits_with_block_gas(block_gas_limit: u64) -> BlockLimits {
        BlockLimits::no_limits().with_block_gas_limit(block_gas_limit).with_tx_gas_limit(u64::MAX)
    }

    fn saturated_usage() -> BlockUsage {
        BlockUsage {
            gas_used: u64::MAX,
            gas: MegaGasUsage {
                regular: u64::MAX,
                state: u64::MAX,
                history: u64::MAX,
                history_bytes: u64::MAX,
                ..Default::default()
            },
            usage: LimitUsage { data_size: u64::MAX, write_records: u64::MAX },
            tx_size: u64::MAX,
            da_size: u64::MAX,
            da_footprint: u64::MAX,
            is_deposit: false,
        }
    }

    #[test]
    fn test_pre_execution_check_block_gas_addition_saturates() {
        // Block has very high accumulated usage and a transaction with a near-`u64::MAX`
        // gas limit; unchecked addition would wrap and accidentally pass the limit check.
        let mut limiter = BlockLimiter::new(limits_with_block_gas(1_000_000));
        limiter.block_gas_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, u64::MAX, 0, 0, false);
        assert!(result.is_err(), "saturating_add must keep the rejection in place");
    }

    #[test]
    fn test_pre_execution_check_block_tx_size_addition_saturates() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_txs_encode_size_limit(1_000_000));
        limiter.block_tx_size_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, 0, u64::MAX, 0, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_pre_execution_check_block_da_size_addition_saturates() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_da_size_limit(1_000_000));
        limiter.block_da_size_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, 0, 0, u64::MAX, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_post_execution_update_saturates_all_counters() {
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        limiter.block_gas_used = u64::MAX - 1;
        limiter.gas = BlockGasCounters {
            execution: u64::MAX - 1,
            state: u64::MAX - 1,
            history: u64::MAX - 1,
            history_bytes: u64::MAX - 1,
        };
        limiter.usage = LimitUsage { data_size: u64::MAX - 1, write_records: u64::MAX - 1 };
        limiter.block_tx_size_used = u64::MAX - 1;
        limiter.block_da_size_used = u64::MAX - 1;
        limiter.block_da_footprint_used = u64::MAX - 1;

        limiter.post_execution_update(&saturated_usage());

        assert_eq!(limiter.block_gas_used, u64::MAX);
        assert_eq!(
            limiter.gas,
            BlockGasCounters {
                execution: u64::MAX,
                state: u64::MAX,
                history: u64::MAX,
                history_bytes: u64::MAX,
            }
        );
        assert_eq!(limiter.usage, LimitUsage { data_size: u64::MAX, write_records: u64::MAX });
        assert_eq!(limiter.block_tx_size_used, u64::MAX);
        assert_eq!(limiter.block_da_size_used, u64::MAX);
        assert_eq!(limiter.block_da_footprint_used, u64::MAX);
    }

    #[test]
    fn test_post_execution_update_skips_da_for_deposits() {
        // Deposit transactions should not advance the data-availability counters, even when the
        // value would otherwise saturate.
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        limiter.block_da_size_used = 100;
        limiter.block_da_footprint_used = 200;

        limiter.post_execution_update(&BlockUsage {
            is_deposit: true,
            tx_size: 7,
            ..saturated_usage()
        });

        assert_eq!(limiter.block_da_size_used, 100);
        assert_eq!(limiter.block_da_footprint_used, 200);
        assert_eq!(limiter.block_tx_size_used, 7, "a deposit still counts as a transaction body");
    }

    /// A deposit is never refused by the block's data-size limit, however far past it the block
    /// is, and still counts towards it: an ordinary transaction after the deposits finds the room
    /// they used.
    #[test]
    fn test_data_size_limit_never_refuses_a_deposit_and_counts_it() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_txs_data_limit(1_000));
        let deposit = BlockUsage {
            usage: LimitUsage { data_size: 600, write_records: 0 },
            is_deposit: true,
            ..Default::default()
        };

        // Two deposits cross the limit between them, and a third finds the block past it.
        for _ in 0..3 {
            assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, true).is_ok());
            limiter.post_execution_update(&deposit);
        }
        assert_eq!(limiter.usage.data_size, 1_800, "every deposit counts");

        let err = limiter
            .pre_execution_check(B256::ZERO, 0, 0, 0, false)
            .expect_err("the deposits used the room an ordinary transaction would need");
        assert!(std::format!("{err}").contains("Block transactions data limit reached"), "{err}");
        assert!(std::format!("{err}").contains("block_used=1800"), "{err}");
    }

    /// Every limit is an inclusive bound: a transaction that exactly fills what the limit — or
    /// what the block has left of it — is admitted, and one unit more is refused.
    #[test]
    fn test_pre_execution_check_admits_what_exactly_fills_a_limit() {
        const LIMIT: u64 = 1_000;
        const USED: u64 = 400;

        // Each case names the dimension, the limiter it is checked on, what the limit leaves for
        // one transaction, and how that figure reaches `pre_execution_check`.
        type Check = fn(&BlockLimiter, u64) -> Result<(), BlockExecutionError>;
        let cases: [(&str, BlockLimiter, u64, Check); 6] = [
            (
                "a transaction's own gas limit",
                BlockLimiter::new(BlockLimits::no_limits().with_tx_gas_limit(LIMIT)),
                LIMIT,
                |limiter, gas_limit| {
                    limiter.pre_execution_check(B256::ZERO, gas_limit, 0, 0, false)
                },
            ),
            (
                "the block's gas limit",
                BlockLimiter {
                    block_gas_used: USED,
                    ..BlockLimiter::new(BlockLimits::no_limits().with_block_gas_limit(LIMIT))
                },
                LIMIT - USED,
                |limiter, gas_limit| {
                    limiter.pre_execution_check(B256::ZERO, gas_limit, 0, 0, false)
                },
            ),
            (
                "a transaction's own encoded size",
                BlockLimiter::new(BlockLimits::no_limits().with_tx_encode_size_limit(LIMIT)),
                LIMIT,
                |limiter, tx_size| limiter.pre_execution_check(B256::ZERO, 0, tx_size, 0, false),
            ),
            (
                "the block's encoded size",
                BlockLimiter {
                    block_tx_size_used: USED,
                    ..BlockLimiter::new(
                        BlockLimits::no_limits().with_block_txs_encode_size_limit(LIMIT),
                    )
                },
                LIMIT - USED,
                |limiter, tx_size| limiter.pre_execution_check(B256::ZERO, 0, tx_size, 0, false),
            ),
            (
                "a transaction's own data-availability size",
                BlockLimiter::new(BlockLimits::no_limits().with_tx_da_size_limit(LIMIT)),
                LIMIT,
                |limiter, da_size| limiter.pre_execution_check(B256::ZERO, 0, 0, da_size, false),
            ),
            (
                "the block's data-availability size",
                BlockLimiter {
                    block_da_size_used: USED,
                    ..BlockLimiter::new(BlockLimits::no_limits().with_block_da_size_limit(LIMIT))
                },
                LIMIT - USED,
                |limiter, da_size| limiter.pre_execution_check(B256::ZERO, 0, 0, da_size, false),
            ),
        ];

        for (name, limiter, bound, check) in cases {
            assert!(
                check(&limiter, bound).is_ok(),
                "{name}: {bound} exactly fills what is left and is admitted"
            );
            assert!(check(&limiter, bound + 1).is_err(), "{name}: one unit more is refused");
        }
    }

    /// No limit only a builder applies refuses a deposit, which the block derived from L1 must
    /// include: not its declared gas, not its encoded size, not what its encoding adds to the
    /// block's. The same transaction not a deposit is refused by each, and the block's gas limit,
    /// the header's, refuses a deposit as it refuses any transaction.
    #[test]
    fn test_no_building_policy_refuses_a_deposit() {
        let cases: [(&str, BlockLimits, u64, u64); 5] = [
            ("a declared gas", BlockLimits::no_limits().with_tx_gas_limit(100), 101, 0),
            ("an encoded size", BlockLimits::no_limits().with_tx_encode_size_limit(10), 0, 11),
            (
                "the block's encoded size",
                BlockLimits::no_limits().with_block_txs_encode_size_limit(10),
                0,
                11,
            ),
            ("a data-availability size", BlockLimits::no_limits().with_tx_da_size_limit(10), 0, 11),
            (
                "the block's data-availability size",
                BlockLimits::no_limits().with_block_da_size_limit(10),
                0,
                11,
            ),
        ];
        for (name, limits, gas_limit, size) in cases {
            let limiter = BlockLimiter::new(limits);
            // The one size feeds both the encoded and the data-availability size.
            let check = |is_deposit| {
                limiter.pre_execution_check(B256::ZERO, gas_limit, size, size, is_deposit)
            };
            assert!(check(true).is_ok(), "{name} over the policy: a deposit is admitted");
            assert!(check(false).is_err(), "{name} over the policy: a transaction is refused");
        }

        let limiter = BlockLimiter::new(limits_with_block_gas(100));
        let err = limiter
            .pre_execution_check(B256::ZERO, 101, 0, 0, true)
            .expect_err("the block's gas limit holds a deposit");
        assert!(std::format!("{err}").contains("transaction gas limit 101 is more than"), "{err}");
    }

    /// A deposit counts towards the block's encoded size though it is never refused by it, so the
    /// transaction after it finds the room it used.
    #[test]
    fn test_a_deposit_counts_towards_the_blocks_encoded_size() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_txs_encode_size_limit(1_000));
        for _ in 0..2 {
            assert!(limiter.pre_execution_check(B256::ZERO, 0, 600, 0, true).is_ok());
            limiter.post_execution_update(&BlockUsage {
                tx_size: 600,
                is_deposit: true,
                ..Default::default()
            });
        }
        assert_eq!(limiter.block_tx_size_used, 1_200, "every deposit counts");

        let err = limiter
            .pre_execution_check(B256::ZERO, 0, 1, 0, false)
            .expect_err("the deposits used the room an ordinary transaction would need");
        assert!(std::format!("{err}").contains("block_used=1200"), "{err}");
    }

    /// What the block has left is what its refusal reports, and it is the limit minus what the
    /// block used.
    #[test]
    fn test_available_gas_is_the_limit_less_what_the_block_used() {
        let mut limiter = BlockLimiter::new(limits_with_block_gas(1_000));
        assert_eq!(limiter.available_gas(), 1_000, "an empty block has all of it");

        limiter.post_execution_update(&BlockUsage { gas_used: 400, ..Default::default() });
        assert_eq!(limiter.available_gas(), 600);

        let err = limiter
            .pre_execution_check(B256::ZERO, 601, 0, 0, false)
            .expect_err("601 does not fit in 600");
        assert!(
            std::format!("{err}").contains("blocks available gas 600"),
            "the refusal reports what the block has left: {err}"
        );

        // The block cannot owe gas: a counter past the limit reports nothing left.
        limiter.post_execution_update(&BlockUsage { gas_used: u64::MAX, ..Default::default() });
        assert_eq!(limiter.available_gas(), 0);
    }

    /// The state and history ledgers, the history bytes and the write records are accumulated, and
    /// no check refuses a transaction on them.
    #[test]
    fn test_state_and_history_are_counted_but_not_enforced() {
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        let usage = BlockUsage {
            gas: MegaGasUsage {
                state: 10_000,
                history: 20_000,
                history_bytes: 250,
                ..Default::default()
            },
            usage: LimitUsage { data_size: 0, write_records: 30 },
            ..Default::default()
        };

        limiter.post_execution_update(&usage);

        assert_eq!(limiter.gas.state, 10_000);
        assert_eq!(limiter.gas.history, 20_000);
        assert_eq!(limiter.gas.history_bytes, 250);
        assert_eq!(limiter.usage.write_records, 30);
        assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, false).is_ok());
    }
}
