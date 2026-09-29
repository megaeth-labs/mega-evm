//! What a block admits, and what it counts of the transactions it packed.
//!
//! The limits come from two places, by who must agree on them:
//!
//! - [`ProtocolLimits`] are the limits every node of a chain holds its blocks and transactions to,
//!   because a different value is a different result: the per-transaction limits the EVM enforces
//!   and the block's four budgets. They are the Satin fork's parameters, read from the schedule at
//!   the block's timestamp, and their default is [`ProtocolLimits::DEFAULT`]: data size capped at
//!   [`TX_DATA_LIMIT`](crate::constants::TX_DATA_LIMIT) per transaction and
//!   [`BLOCK_DATA_LIMIT`](crate::constants::BLOCK_DATA_LIMIT) per block, gas detention's caps at
//!   the spec's, and every other limit unlimited.
//! - [`BlockLimits`] is the building policy a node passes in the block execution context: the
//!   limits only a builder applies — a transaction's declared gas, the encoded sizes and the
//!   data-availability sizes — and caps a builder may put on the block's budgets below the
//!   protocol's. It never loosens a protocol limit ([`BlockLimits::within`]), and a validator
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
//! A limit only known after execution — the execution ledger, the state ledger, and the data-size
//! bytes and write records a transaction kept — is accumulated when the transaction commits. The
//! transaction that crosses such a limit is therefore still packed, and the ones after it are
//! refused; this is what keeps a block full rather than dropping the work already done. A block
//! whose counter has reached its limit refuses what comes after it, so the overshoot is bounded by
//! one transaction per dimension, besides what deposits add:
//!
//! - the execution ledger, the data-size bytes and the write records are checked before the *next*
//!   transaction starts, and a block that has reached any of them refuses every later transaction —
//!   a deposit excepted;
//! - the state ledger is checked once the next transaction has executed, and a block that has
//!   reached its limit refuses a later transaction only if it adds state gas — a deposit excepted.
//!   One that adds none still fits, and only its own execution can tell which it is.
//!
//! # Deposits
//!
//! A deposit is an L1 message the chain cannot censor: the block derived from L1 must include it,
//! and the builder does not choose it. So a deposit is exempt from both data-availability limits
//! and does not count towards the block's. The execution-gas, state-gas, data-size and KV limits
//! are packing budgets for the transactions the builder chooses, so none of them refuses a deposit
//! either. A deposit still counts towards all four, so the transactions after it find the room it
//! used.
//!
//! The per-transaction limits the block installs on the EVM — data size, write records, state gas
//! — are not block limits: they hold a deposit's own execution the way they hold any
//! transaction's, and a deposit that crosses one is included with the stop as its result.
//!
//! # Which dimensions are enforced
//!
//! Of the three gas ledgers the block counts ([`BlockGasCounters`]), execution and state have a
//! block limit. History has none by design; the history bytes beside it are reported, not
//! limited. The write-record count, which is the block's KV count, has one too:
//! [`ProtocolLimits::block_kv_update_limit`]. Every record weighs forty bytes of data size, so a
//! block held to [`BLOCK_DATA_LIMIT`](crate::constants::BLOCK_DATA_LIMIT) keeps at most 327,680
//! records whatever its KV limit; the KV limit binds only below that, and is unlimited unless the
//! chain sets it.

use alloy_primitives::TxHash;

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{boxed::Box, format};

use alloy_evm::block::{BlockExecutionError, BlockValidationError};

use crate::{
    BlockGasCounters, EvmTxRuntimeLimits, HardforkParams, HardforkParamsError, LimitUsage,
    MegaBlockLimitExceededError, MegaGasUsage, MegaHardfork, MegaTxLimitExceededError,
    TX_BODY_SIZE,
};

/// The limits the protocol holds every block and every transaction to: the Satin fork's
/// parameters.
///
/// Each of them changes what a validator computes when it re-executes a block, so every node of
/// a chain holds the same values, and they travel with the chain configuration, attached to the
/// fork ([`HardforkParams`]). Block execution reads them from the schedule at the block's
/// timestamp ([`MegaHardforks::protocol_limits`](crate::MegaHardforks::protocol_limits)); a node
/// does not pass them with a block.
///
/// - [`tx_runtime_limits`](Self::tx_runtime_limits) holds each transaction's own execution: a
///   transaction that crosses one is stopped, or a frame reverted, and that is the result its
///   receipt records. There is no tighter setting a builder could run a transaction under and still
///   agree with a validator; a builder that wants to avoid such a transaction leaves it out.
/// - The four block budgets hold what a block's transactions spend and keep together. A validator
///   refuses a block that packs a transaction after the block reached one; a builder may pack
///   tighter ([`BlockLimits`]), never looser.
///
/// [`Default`] is [`DEFAULT`](Self::DEFAULT): the values of [`crate::constants`], provisional
/// until the economics sign-off fixes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtocolLimits {
    /// The limits every transaction runs under, which the block executor installs on the EVM:
    /// data size, write records and state gas, per transaction and per frame, and gas
    /// detention's two caps.
    pub tx_runtime_limits: EvmTxRuntimeLimits,
    /// The most execution gas a block's transactions may spend together. The transaction that
    /// reaches it is packed; after it, only a deposit is.
    pub block_execution_gas_limit: u64,
    /// The most state gas a block's transactions may spend together. The transaction that reaches
    /// it is packed; after it, only a transaction that adds no state gas is, or a deposit.
    pub block_state_gas_limit: u64,
    /// The most data-size bytes a block's transactions may keep together. The transaction that
    /// reaches it is packed; after it, only a deposit is.
    pub block_txs_data_limit: u64,
    /// The most write records a block's transactions may keep together: the block's KV count. The
    /// transaction that reaches it is packed; after it, only a deposit is.
    pub block_kv_update_limit: u64,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl ProtocolLimits {
    /// Satin's limits: each transaction holds [`TX_DATA_LIMIT`] bytes of data size and a block
    /// [`BLOCK_DATA_LIMIT`], a read of volatile data caps the transaction's compute at
    /// [`BLOCK_ENV_ACCESS_COMPUTE_GAS`] or [`ORACLE_ACCESS_COMPUTE_GAS`], and every other limit is
    /// unlimited. Provisional, as the constants are.
    ///
    /// [`TX_DATA_LIMIT`]: crate::constants::TX_DATA_LIMIT
    /// [`BLOCK_DATA_LIMIT`]: crate::constants::BLOCK_DATA_LIMIT
    /// [`BLOCK_ENV_ACCESS_COMPUTE_GAS`]: crate::constants::BLOCK_ENV_ACCESS_COMPUTE_GAS
    /// [`ORACLE_ACCESS_COMPUTE_GAS`]: crate::constants::ORACLE_ACCESS_COMPUTE_GAS
    pub const DEFAULT: Self = Self {
        tx_runtime_limits: EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(crate::constants::TX_DATA_LIMIT)
            .with_block_env_access_compute_gas_limit(crate::constants::BLOCK_ENV_ACCESS_COMPUTE_GAS)
            .with_oracle_access_compute_gas_limit(crate::constants::ORACLE_ACCESS_COMPUTE_GAS),
        block_execution_gas_limit: u64::MAX,
        block_state_gas_limit: u64::MAX,
        block_txs_data_limit: crate::constants::BLOCK_DATA_LIMIT,
        block_kv_update_limit: u64::MAX,
    };

    /// No limit at all, gas detention's caps included, so no transaction is detained
    /// ([`EvmTxRuntimeLimits::no_limits`]).
    ///
    /// For tests and equivalence runs: [`validate`](HardforkParams::validate) refuses it, so no
    /// chain configuration carries it.
    pub const fn no_limits() -> Self {
        Self {
            tx_runtime_limits: EvmTxRuntimeLimits::no_limits(),
            block_execution_gas_limit: u64::MAX,
            block_state_gas_limit: u64::MAX,
            block_txs_data_limit: u64::MAX,
            block_kv_update_limit: u64::MAX,
        }
    }

    /// Sets the limits every transaction runs under.
    pub const fn with_tx_runtime_limits(mut self, limits: EvmTxRuntimeLimits) -> Self {
        self.tx_runtime_limits = limits;
        self
    }

    /// Sets the block's execution-gas limit.
    pub const fn with_block_execution_gas_limit(mut self, limit: u64) -> Self {
        self.block_execution_gas_limit = limit;
        self
    }

    /// Sets the block's state-gas limit.
    pub const fn with_block_state_gas_limit(mut self, limit: u64) -> Self {
        self.block_state_gas_limit = limit;
        self
    }

    /// Sets the block's data-size limit.
    pub const fn with_block_txs_data_limit(mut self, limit: u64) -> Self {
        self.block_txs_data_limit = limit;
        self
    }

    /// Sets the block's KV limit: the most write records its transactions may keep together.
    pub const fn with_block_kv_update_limit(mut self, limit: u64) -> Self {
        self.block_kv_update_limit = limit;
        self
    }
}

impl HardforkParams for ProtocolLimits {
    const FORK: MegaHardfork = MegaHardfork::Satin;
    const NAME: &'static str = "ProtocolLimits";

    /// Refuses a value no chain can run on:
    ///
    /// - a limit of zero, which is what a field left out of a configuration reads as, and which
    ///   stops or refuses every transaction that uses the dimension;
    /// - a transaction data-size limit below [`TX_BODY_SIZE`], the bytes every transaction's body
    ///   counts, which stops every transaction before it runs;
    /// - an unlimited gas-detention cap: `u64::MAX` caps nothing, and a read of volatile data must
    ///   be held to a finite one on a chain.
    ///
    /// `u64::MAX` is a valid value for every other limit: it leaves the dimension unlimited.
    fn validate(&self) -> Result<(), HardforkParamsError> {
        let EvmTxRuntimeLimits {
            tx_data_size_limit,
            frame_data_size_limit,
            tx_kv_update_limit,
            frame_kv_update_limit,
            tx_state_gas_limit,
            block_env_access_compute_gas_limit,
            oracle_access_compute_gas_limit,
        } = self.tx_runtime_limits;
        let detention_caps = [
            (
                "tx_runtime_limits.block_env_access_compute_gas_limit",
                block_env_access_compute_gas_limit,
            ),
            ("tx_runtime_limits.oracle_access_compute_gas_limit", oracle_access_compute_gas_limit),
        ];
        let limits = [
            ("tx_runtime_limits.tx_data_size_limit", tx_data_size_limit),
            ("tx_runtime_limits.frame_data_size_limit", frame_data_size_limit),
            ("tx_runtime_limits.tx_kv_update_limit", tx_kv_update_limit),
            ("tx_runtime_limits.frame_kv_update_limit", frame_kv_update_limit),
            ("tx_runtime_limits.tx_state_gas_limit", tx_state_gas_limit),
            detention_caps[0],
            detention_caps[1],
            ("block_execution_gas_limit", self.block_execution_gas_limit),
            ("block_state_gas_limit", self.block_state_gas_limit),
            ("block_txs_data_limit", self.block_txs_data_limit),
            ("block_kv_update_limit", self.block_kv_update_limit),
        ];

        let invalid = |message| Err(HardforkParamsError { message });
        for (name, limit) in limits {
            if limit == 0 {
                return invalid(format!("ProtocolLimits.{name} must not be zero"));
            }
        }
        if tx_data_size_limit < TX_BODY_SIZE {
            return invalid(format!(
                "ProtocolLimits.tx_runtime_limits.tx_data_size_limit must be at least \
                 {TX_BODY_SIZE}, the bytes every transaction's body counts"
            ));
        }
        for (name, cap) in detention_caps {
            if cap == u64::MAX {
                return invalid(format!(
                    "ProtocolLimits.{name} must be finite: u64::MAX leaves a read of volatile \
                     data undetained"
                ));
            }
        }
        Ok(())
    }
}

/// The building policy one block is packed under: the limits only a builder applies, and the
/// caps it may put on the block's budgets below the protocol's.
///
/// A node passes it in the block execution context. None of it is a protocol value: the
/// protocol's limits are [`ProtocolLimits`], which the block executor reads from the chain's
/// schedule, and this can only make a block tighter than they do ([`within`](Self::within)). So a
/// builder packs under it and a validator re-executing the block leaves it at [`Default`], which
/// restricts nothing: every block a builder packed under a policy fits the protocol's limits, and
/// a validator that applied the builder's policy too would only refuse blocks, never accept one
/// the protocol refuses.
///
/// - [`tx_gas_limit`](Self::tx_gas_limit), the two encoded-size limits and the two
///   data-availability size limits are the builder's alone: the protocol holds a transaction's
///   declared gas to the block's gas limit, and what a block writes to its data-size budget, which
///   counts calldata.
/// - The four block budgets are the builder's caps on the protocol's: the block executor holds a
///   block to the smaller of the two.
/// - [`block_gas_limit`](Self::block_gas_limit) is the header's: the block executor always sets it
///   from the block environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockLimits {
    /// The most gas a single transaction may declare.
    pub tx_gas_limit: u64,
    /// The block's gas limit: a transaction is admitted only if its declared gas limit fits in
    /// what the block has left. Also the budget the data-availability footprint of the block's
    /// transactions is held to.
    pub block_gas_limit: u64,
    /// The most bytes a single transaction's EIP-2718 encoding may take.
    pub tx_encode_size_limit: u64,
    /// The most bytes the block's transaction bodies may take together, uncompressed.
    pub block_txs_encode_size_limit: u64,
    /// The most data-availability bytes a single transaction may take. Deposits are exempt.
    pub tx_da_size_limit: u64,
    /// The most data-availability bytes the block's transactions may take together. Deposits are
    /// exempt and do not count towards it.
    pub block_da_size_limit: u64,
    /// The most execution gas the block's transactions may spend together. The transaction that
    /// reaches it is packed; after it, only a deposit is, which it never refuses and which counts
    /// towards it.
    ///
    /// A builder's cap: the block is held to the smaller of this and
    /// [`ProtocolLimits::block_execution_gas_limit`].
    pub block_execution_gas_limit: u64,
    /// The most state gas the block's transactions may spend together. The transaction that
    /// reaches it is packed; after it, only a transaction that adds no state gas is, or a deposit,
    /// which it never refuses and which counts towards it.
    ///
    /// A builder's cap: the block is held to the smaller of this and
    /// [`ProtocolLimits::block_state_gas_limit`].
    pub block_state_gas_limit: u64,
    /// The most data-size bytes the block's transactions may keep together. The transaction that
    /// reaches it is packed; after it, only a deposit is, which it never refuses and which counts
    /// towards it.
    ///
    /// A builder's cap: the block is held to the smaller of this and
    /// [`ProtocolLimits::block_txs_data_limit`].
    pub block_txs_data_limit: u64,
    /// The most write records the block's transactions may keep together: the block's KV count.
    /// The transaction that reaches it is packed; after it, only a deposit is, which it never
    /// refuses and which counts towards it.
    ///
    /// A builder's cap: the block is held to the smaller of this and
    /// [`ProtocolLimits::block_kv_update_limit`].
    pub block_kv_update_limit: u64,
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
            block_execution_gas_limit: u64::MAX,
            block_state_gas_limit: u64::MAX,
            block_txs_data_limit: u64::MAX,
            block_kv_update_limit: u64::MAX,
        }
    }

    /// These limits held within `protocol`: each of the block's four budgets is the smaller of
    /// this policy's cap and the protocol's limit, so a builder's setting can tighten a budget and
    /// never loosen one. The limits only a builder applies are kept as they are.
    pub fn within(self, protocol: &ProtocolLimits) -> Self {
        Self {
            block_execution_gas_limit: self
                .block_execution_gas_limit
                .min(protocol.block_execution_gas_limit),
            block_state_gas_limit: self.block_state_gas_limit.min(protocol.block_state_gas_limit),
            block_txs_data_limit: self.block_txs_data_limit.min(protocol.block_txs_data_limit),
            block_kv_update_limit: self.block_kv_update_limit.min(protocol.block_kv_update_limit),
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

    /// Caps the block's execution gas.
    pub const fn with_block_execution_gas_limit(mut self, limit: u64) -> Self {
        self.block_execution_gas_limit = limit;
        self
    }

    /// Caps the block's state gas.
    pub const fn with_block_state_gas_limit(mut self, limit: u64) -> Self {
        self.block_state_gas_limit = limit;
        self
    }

    /// Caps the block's data size.
    pub const fn with_block_txs_data_limit(mut self, limit: u64) -> Self {
        self.block_txs_data_limit = limit;
        self
    }

    /// Caps the block's KV count: the write records its transactions may keep together.
    pub const fn with_block_kv_update_limit(mut self, limit: u64) -> Self {
        self.block_kv_update_limit = limit;
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
    /// Whether the transaction is a deposit, which the data-availability dimensions exempt and the
    /// execution-gas, state-gas, data-size and KV limits never refuse.
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
    /// deposit is held to neither data-availability limit, nor to the execution-gas, the data-size
    /// or the KV limit. It reads the counters and changes nothing;
    /// [`post_execution_check`](Self::post_execution_check) checks what only the transaction's
    /// execution reveals, and [`post_execution_update`](Self::post_execution_update) advances the
    /// counters once the transaction commits.
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
        if gas_limit > self.limits.tx_gas_limit {
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

        // A deposit is an L1 message the chain cannot censor, so it is exempt from both
        // data-availability limits and does not count towards the block's.
        if !is_deposit {
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
        }

        // The packing budgets: the dimensions a transaction's own execution reveals. The
        // transaction that crossed one is already packed, so what is refused here is the next
        // one — never a deposit, which is not the builder's to refuse. Every committed
        // transaction counts towards them, a deposit included, in `post_execution_update`.
        if is_deposit {
            return Ok(());
        }

        if self.gas.execution >= self.limits.block_execution_gas_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::ExecutionGasLimit {
                    block_used: self.gas.execution,
                    limit: self.limits.block_execution_gas_limit,
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

        // The block's KV count.
        if self.usage.write_records >= self.limits.block_kv_update_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::KVUpdateLimit {
                    block_used: self.usage.write_records,
                    limit: self.limits.block_kv_update_limit,
                },
            ));
        }

        // `self.gas.state` is checked after execution, in `post_execution_check`: a block that has
        // reached its state gas still admits a transaction that adds none. `self.gas.history` is
        // accumulated and not checked: history has no block limit.

        Ok(())
    }

    /// Whether the executed transaction whose block usage is `usage` may be packed in this block.
    ///
    /// A block that has reached its state-gas limit refuses a transaction that adds state gas, and
    /// only such a transaction: whether one does is known only once it has run. The transaction
    /// that reaches the limit is itself packed — the block had not reached it when that
    /// transaction came — so the block overshoots its limit by at most one transaction's state
    /// gas, besides what deposits add: a deposit is never refused. Like
    /// [`pre_execution_check`](Self::pre_execution_check) it changes nothing.
    ///
    /// # Errors
    ///
    /// A [`MegaBlockLimitExceededError::StateGasLimit`] when the block has no state gas left and
    /// the transaction, not a deposit, adds some, which a builder answers by trying the next
    /// transaction.
    pub fn post_execution_check(
        &self,
        tx_hash: TxHash,
        usage: &BlockUsage,
    ) -> Result<(), BlockExecutionError> {
        // A deposit is not the builder's to refuse; it still counts, in `post_execution_update`.
        if !usage.is_deposit &&
            usage.gas.state > 0 &&
            self.gas.state >= self.limits.block_state_gas_limit
        {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::StateGasLimit {
                    block_used: self.gas.state,
                    tx_used: usage.gas.state,
                    limit: self.limits.block_state_gas_limit,
                },
            ));
        }
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
    use crate::{MegaHardforkConfig, MegaHardforks};
    use alloy_primitives::B256;

    /// The protocol's default limits, written out so a change to one is a visible diff: today's
    /// provisional constants, and unlimited everywhere else.
    #[test]
    fn test_the_default_protocol_limits_are_the_provisional_constants() {
        use crate::constants::{
            BLOCK_DATA_LIMIT, BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS,
            TX_DATA_LIMIT,
        };
        assert_eq!(ProtocolLimits::default(), ProtocolLimits::DEFAULT);
        assert_eq!(
            ProtocolLimits::DEFAULT,
            ProtocolLimits {
                tx_runtime_limits: EvmTxRuntimeLimits {
                    tx_data_size_limit: TX_DATA_LIMIT,
                    frame_data_size_limit: u64::MAX,
                    tx_kv_update_limit: u64::MAX,
                    frame_kv_update_limit: u64::MAX,
                    tx_state_gas_limit: u64::MAX,
                    block_env_access_compute_gas_limit: BLOCK_ENV_ACCESS_COMPUTE_GAS,
                    oracle_access_compute_gas_limit: ORACLE_ACCESS_COMPUTE_GAS,
                },
                block_execution_gas_limit: u64::MAX,
                block_state_gas_limit: u64::MAX,
                block_txs_data_limit: BLOCK_DATA_LIMIT,
                block_kv_update_limit: u64::MAX,
            }
        );
        assert_eq!(
            ProtocolLimits::DEFAULT.tx_runtime_limits,
            EvmTxRuntimeLimits::default().with_tx_data_size_limit(TX_DATA_LIMIT),
            "a transaction's defaults are a bare context's, with the data-size limit"
        );
        assert_eq!(ProtocolLimits::DEFAULT.validate(), Ok(()));
    }

    /// The limits travel with a chain configuration: they survive a round trip through its JSON,
    /// whose shape is written out here, and through a schedule, which hands back what was
    /// attached.
    #[test]
    fn test_protocol_limits_round_trip() {
        let limits = ProtocolLimits::DEFAULT
            .with_tx_runtime_limits(
                ProtocolLimits::DEFAULT
                    .tx_runtime_limits
                    .with_frame_data_size_limit(1)
                    .with_tx_kv_update_limit(2)
                    .with_frame_kv_update_limit(3)
                    .with_tx_state_gas_limit(4),
            )
            .with_block_execution_gas_limit(5)
            .with_block_state_gas_limit(6)
            .with_block_txs_data_limit(7)
            .with_block_kv_update_limit(8);

        let json = serde_json::to_string(&limits).expect("serializes");
        assert_eq!(
            json,
            r#"{"txRuntimeLimits":{"txDataSizeLimit":13107200,"frameDataSizeLimit":1,"txKvUpdateLimit":2,"frameKvUpdateLimit":3,"txStateGasLimit":4,"blockEnvAccessComputeGasLimit":20000000,"oracleAccessComputeGasLimit":20000000},"blockExecutionGasLimit":5,"blockStateGasLimit":6,"blockTxsDataLimit":7,"blockKvUpdateLimit":8}"#
        );
        assert_eq!(serde_json::from_str::<ProtocolLimits>(&json).expect("deserializes"), limits);

        // A field left out is an error, not a zero; so is one the type does not have.
        let missing = json.replace(r#","blockKvUpdateLimit":8"#, "");
        assert!(serde_json::from_str::<ProtocolLimits>(&missing).is_err(), "{missing}");
        let missing_tx = json.replace(r#""txStateGasLimit":4,"#, "");
        assert!(serde_json::from_str::<ProtocolLimits>(&missing_tx).is_err(), "{missing_tx}");
        let unknown = json.replace(r#""blockKvUpdateLimit":8"#, r#""blockKvUpdateLimit":8,"x":1"#);
        assert!(serde_json::from_str::<ProtocolLimits>(&unknown).is_err(), "{unknown}");

        let schedule = MegaHardforkConfig::default().with_all_activated().with_params(limits);
        assert_eq!(schedule.fork_params::<ProtocolLimits>(), Some(&limits));
        assert_eq!(schedule.protocol_limits(0), Some(limits));
    }

    /// Every limit refuses zero, the value a field left out of a configuration reads as; the
    /// transaction's data size refuses less than a body; the detention caps refuse `u64::MAX`,
    /// and every other limit accepts it.
    #[test]
    fn test_validate_refuses_what_no_chain_can_run_on() {
        let base = ProtocolLimits::DEFAULT;
        let tx = |f: fn(EvmTxRuntimeLimits, u64) -> EvmTxRuntimeLimits, value| {
            base.with_tx_runtime_limits(f(base.tx_runtime_limits, value))
        };
        type Set = fn(ProtocolLimits, u64) -> ProtocolLimits;
        let fields: [(&str, Set); 11] = [
            ("tx_runtime_limits.tx_data_size_limit", |l, v| {
                l.with_tx_runtime_limits(l.tx_runtime_limits.with_tx_data_size_limit(v))
            }),
            ("tx_runtime_limits.frame_data_size_limit", |l, v| {
                l.with_tx_runtime_limits(l.tx_runtime_limits.with_frame_data_size_limit(v))
            }),
            ("tx_runtime_limits.tx_kv_update_limit", |l, v| {
                l.with_tx_runtime_limits(l.tx_runtime_limits.with_tx_kv_update_limit(v))
            }),
            ("tx_runtime_limits.frame_kv_update_limit", |l, v| {
                l.with_tx_runtime_limits(l.tx_runtime_limits.with_frame_kv_update_limit(v))
            }),
            ("tx_runtime_limits.tx_state_gas_limit", |l, v| {
                l.with_tx_runtime_limits(l.tx_runtime_limits.with_tx_state_gas_limit(v))
            }),
            ("tx_runtime_limits.block_env_access_compute_gas_limit", |l, v| {
                l.with_tx_runtime_limits(
                    l.tx_runtime_limits.with_block_env_access_compute_gas_limit(v),
                )
            }),
            ("tx_runtime_limits.oracle_access_compute_gas_limit", |l, v| {
                l.with_tx_runtime_limits(
                    l.tx_runtime_limits.with_oracle_access_compute_gas_limit(v),
                )
            }),
            ("block_execution_gas_limit", ProtocolLimits::with_block_execution_gas_limit),
            ("block_state_gas_limit", ProtocolLimits::with_block_state_gas_limit),
            ("block_txs_data_limit", ProtocolLimits::with_block_txs_data_limit),
            ("block_kv_update_limit", ProtocolLimits::with_block_kv_update_limit),
        ];

        for (name, set) in fields {
            assert_eq!(
                set(base, 0).validate(),
                Err(HardforkParamsError {
                    message: std::format!("ProtocolLimits.{name} must not be zero")
                }),
                "{name}"
            );
            assert_eq!(
                set(base, 1).validate().is_ok(),
                name != "tx_runtime_limits.tx_data_size_limit",
                "{name} at one"
            );
            let unlimited = set(base, u64::MAX).validate();
            if name.ends_with("compute_gas_limit") {
                assert_eq!(
                    unlimited,
                    Err(HardforkParamsError {
                        message: std::format!(
                            "ProtocolLimits.{name} must be finite: u64::MAX leaves a read of \
                             volatile data undetained"
                        )
                    }),
                    "{name}"
                );
                assert_eq!(set(base, u64::MAX - 1).validate(), Ok(()), "{name} just below");
            } else {
                assert_eq!(unlimited, Ok(()), "{name} may be unlimited");
            }
        }

        // The transaction's data size holds at least a body, and exactly a body is enough.
        assert_eq!(
            tx(EvmTxRuntimeLimits::with_tx_data_size_limit, TX_BODY_SIZE - 1).validate(),
            Err(HardforkParamsError {
                message: std::format!(
                    "ProtocolLimits.tx_runtime_limits.tx_data_size_limit must be at least \
                     {TX_BODY_SIZE}, the bytes every transaction's body counts"
                )
            })
        );
        assert_eq!(
            tx(EvmTxRuntimeLimits::with_tx_data_size_limit, TX_BODY_SIZE).validate(),
            Ok(())
        );

        // The unlimited set of tests and equivalence runs is refused.
        assert!(ProtocolLimits::no_limits().validate().is_err());
    }

    /// A schedule refuses to carry the unlimited set, so no validated chain configuration does.
    #[test]
    #[should_panic(expected = "Invalid params for fork Satin: \
                    ProtocolLimits.tx_runtime_limits.block_env_access_compute_gas_limit must be \
                    finite")]
    fn test_a_schedule_refuses_the_unlimited_protocol_limits() {
        let _ = MegaHardforkConfig::default()
            .with_all_activated()
            .with_params(ProtocolLimits::no_limits());
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
            block_execution_gas_limit,
            block_state_gas_limit,
            block_txs_data_limit,
            block_kv_update_limit,
        } = BlockLimits::default();
        for limit in [
            tx_gas_limit,
            block_gas_limit,
            tx_encode_size_limit,
            block_txs_encode_size_limit,
            tx_da_size_limit,
            block_da_size_limit,
            block_execution_gas_limit,
            block_state_gas_limit,
            block_txs_data_limit,
            block_kv_update_limit,
        ] {
            assert_eq!(limit, u64::MAX);
        }
        assert_eq!(
            BlockLimits::default().within(&ProtocolLimits::DEFAULT).block_txs_data_limit,
            crate::constants::BLOCK_DATA_LIMIT,
            "held within the protocol's, the default policy is the protocol's"
        );
    }

    /// A builder's cap on a block budget holds the block only where it is below the protocol's
    /// limit: it tightens, and never loosens. The limits only a builder applies pass through.
    #[test]
    fn test_a_building_policy_tightens_a_budget_and_never_loosens_it() {
        let protocol = ProtocolLimits::DEFAULT
            .with_block_execution_gas_limit(100)
            .with_block_state_gas_limit(200)
            .with_block_txs_data_limit(300)
            .with_block_kv_update_limit(400);

        let tighter = BlockLimits::no_limits()
            .with_block_execution_gas_limit(10)
            .with_block_state_gas_limit(20)
            .with_block_txs_data_limit(30)
            .with_block_kv_update_limit(40)
            .with_tx_gas_limit(1)
            .with_tx_encode_size_limit(2)
            .with_block_txs_encode_size_limit(3)
            .with_tx_da_size_limit(4)
            .with_block_da_size_limit(5)
            .with_block_gas_limit(6);
        assert_eq!(tighter.within(&protocol), tighter, "every cap is below the protocol's");

        let looser = BlockLimits::no_limits()
            .with_block_execution_gas_limit(1_000)
            .with_block_state_gas_limit(2_000)
            .with_block_txs_data_limit(3_000)
            .with_block_kv_update_limit(4_000);
        let held = looser.within(&protocol);
        assert_eq!(
            (
                held.block_execution_gas_limit,
                held.block_state_gas_limit,
                held.block_txs_data_limit,
                held.block_kv_update_limit,
            ),
            (100, 200, 300, 400),
            "a cap above the protocol's gives way to it"
        );
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

    /// The execution ledger is the one gas dimension with a block limit: the transaction that
    /// crosses it is already packed, and the next one is refused.
    #[test]
    fn test_execution_gas_limit_refuses_the_transaction_after_the_crossing() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_execution_gas_limit(1_000));

        limiter.post_execution_update(&BlockUsage {
            gas: MegaGasUsage { regular: 999, ..Default::default() },
            ..Default::default()
        });
        assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, false).is_ok());

        limiter.post_execution_update(&BlockUsage {
            gas: MegaGasUsage { regular: 1, ..Default::default() },
            ..Default::default()
        });
        let err = limiter
            .pre_execution_check(B256::ZERO, 0, 0, 0, false)
            .expect_err("the block's execution gas has reached its limit");
        assert!(std::format!("{err}").contains("Block execution gas limit reached"), "{err}");
    }

    /// A deposit is never refused by the execution-gas limit, however far past it the block is,
    /// and still counts towards it: an ordinary transaction after the deposits finds the room
    /// they used.
    #[test]
    fn test_execution_gas_limit_never_refuses_a_deposit_and_counts_it() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_execution_gas_limit(1_000));
        let deposit = BlockUsage {
            gas: MegaGasUsage { regular: 600, ..Default::default() },
            is_deposit: true,
            ..Default::default()
        };

        // Two deposits cross the limit between them, and a third finds the block past it.
        for _ in 0..3 {
            assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, true).is_ok());
            limiter.post_execution_update(&deposit);
        }
        assert_eq!(limiter.gas.execution, 1_800, "every deposit counts");

        let err = limiter
            .pre_execution_check(B256::ZERO, 0, 0, 0, false)
            .expect_err("the deposits used the room an ordinary transaction would need");
        assert!(std::format!("{err}").contains("block_used=1800"), "{err}");
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

    /// The history ledger and the history bytes are accumulated, and no check refuses a
    /// transaction on them; nor on the state ledger or the write records, whose limits are
    /// unlimited unless the chain sets them.
    #[test]
    fn test_history_is_counted_but_not_enforced() {
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
        assert!(limiter.post_execution_check(B256::ZERO, &usage).is_ok());
    }

    /// What one committed transaction kept of write records, and nothing else.
    fn keeps_records(write_records: u64) -> BlockUsage {
        BlockUsage { usage: LimitUsage { data_size: 0, write_records }, ..Default::default() }
    }

    /// The block's KV limit is a packing budget like its data size: the transaction that reaches
    /// it is packed, the next one is refused before it runs, and the limit is an inclusive bound.
    #[test]
    fn test_kv_limit_refuses_the_transaction_after_the_crossing() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_kv_update_limit(1_000));

        limiter.post_execution_update(&keeps_records(999));
        assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, false).is_ok());

        // The crossing transaction had been admitted, and it is packed: the block overshoots.
        limiter.post_execution_update(&keeps_records(5));
        assert_eq!(limiter.usage.write_records, 1_004);
        let err = limiter
            .pre_execution_check(B256::ZERO, 0, 0, 0, false)
            .expect_err("the block's write records have reached their limit");
        assert!(std::format!("{err}").contains("Block KV update limit reached"), "{err}");
        assert!(std::format!("{err}").contains("block_used=1004"), "{err}");

        // A block that kept exactly its limit is full.
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_kv_update_limit(1_000));
        limiter.post_execution_update(&keeps_records(1_000));
        assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, false).is_err());
    }

    /// A deposit is never refused by the block's KV limit, however far past it the block is, and
    /// still counts towards it: an ordinary transaction after the deposits finds the room they
    /// used.
    #[test]
    fn test_kv_limit_never_refuses_a_deposit_and_counts_it() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_kv_update_limit(1_000));
        let deposit = BlockUsage { is_deposit: true, ..keeps_records(600) };

        for _ in 0..3 {
            assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, true).is_ok());
            limiter.post_execution_update(&deposit);
        }
        assert_eq!(limiter.usage.write_records, 1_800, "every deposit counts");

        let err = limiter
            .pre_execution_check(B256::ZERO, 0, 0, 0, false)
            .expect_err("the deposits used the room an ordinary transaction would need");
        assert!(std::format!("{err}").contains("block_used=1800"), "{err}");
    }

    /// What one committed transaction spent on the state ledger, and nothing else.
    fn adds_state(state: u64) -> BlockUsage {
        BlockUsage { gas: MegaGasUsage { state, ..Default::default() }, ..Default::default() }
    }

    /// The state ledger's block limit: the transaction that reaches it is packed, and after it a
    /// transaction is refused only if it adds state gas.
    #[test]
    fn test_state_gas_limit_packs_the_crossing_transaction_and_skips_the_next_that_adds_state() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_state_gas_limit(1_000));

        // The n-th transaction finds the block below its limit and crosses it: it is packed.
        limiter.post_execution_update(&adds_state(900));
        assert!(limiter.post_execution_check(B256::ZERO, &adds_state(300)).is_ok());
        limiter.post_execution_update(&adds_state(300));
        assert_eq!(limiter.gas.state, 1_200, "the block overshoots by that one transaction");

        // The (n+1)-th adds state gas: refused, with the state dimension's own error.
        let err = limiter
            .post_execution_check(B256::ZERO, &adds_state(1))
            .expect_err("the block has no state gas left");
        assert!(std::format!("{err}").contains("Block state gas limit reached"), "{err}");
        assert!(std::format!("{err}").contains("block_used=1200"), "{err}");
        assert!(std::format!("{err}").contains("tx_used=1"), "{err}");

        // One that adds none still fits, before execution and after it.
        assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, false).is_ok());
        assert!(limiter.post_execution_check(B256::ZERO, &adds_state(0)).is_ok());
    }

    /// A deposit is never refused by the state-gas limit, however far past it the block is, and
    /// still counts towards it: an ordinary transaction after the deposits finds the room they
    /// used.
    #[test]
    fn test_state_gas_limit_never_refuses_a_deposit_and_counts_it() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_state_gas_limit(1_000));
        let deposit = |state| BlockUsage { is_deposit: true, ..adds_state(state) };

        // Two deposits cross the limit between them.
        for _ in 0..2 {
            assert!(limiter.post_execution_check(B256::ZERO, &deposit(600)).is_ok());
            limiter.post_execution_update(&deposit(600));
        }
        assert_eq!(limiter.gas.state, 1_200);

        // A third finds the block past its limit and is still admitted.
        assert!(limiter.post_execution_check(B256::ZERO, &deposit(600)).is_ok());
        limiter.post_execution_update(&deposit(600));
        assert_eq!(limiter.gas.state, 1_800, "every deposit counts");

        let err = limiter
            .post_execution_check(B256::ZERO, &adds_state(1))
            .expect_err("the deposits used the room an ordinary transaction would need");
        assert!(std::format!("{err}").contains("block_used=1800"), "{err}");
    }

    /// The state-gas limit is an inclusive bound like every other: a block may spend exactly its
    /// limit, and the block that has is full.
    #[test]
    fn test_state_gas_limit_admits_what_exactly_fills_it() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_state_gas_limit(1_000));
        limiter.post_execution_update(&adds_state(999));
        assert!(limiter.post_execution_check(B256::ZERO, &adds_state(1)).is_ok());

        limiter.post_execution_update(&adds_state(1));
        assert!(
            limiter.post_execution_check(B256::ZERO, &adds_state(1)).is_err(),
            "a block that spent exactly its limit is full"
        );
    }
}
