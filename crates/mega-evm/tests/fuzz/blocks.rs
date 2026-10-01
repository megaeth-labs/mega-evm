//! Blocks: several random transactions through `MegaBlockExecutor`, under random valid
//! `ProtocolLimits` and a random building policy, with the block's admission rules restated and
//! its counters summed.
//!
//! Every transaction is built right before it runs, with its sender's live nonce, so a refusal
//! comes from the block's rules or the transaction's own validity and not from a stale nonce.

use alloy_consensus::{transaction::Recovered, Sealed, Signed, TxEip1559, TxEip2930, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_evm::{
    block::{BlockExecutionError, BlockExecutor, BlockValidationError},
    Evm, EvmEnv, EvmFactory,
};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{keccak256, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    system::{SequencerRegistryConfig, MEGA_SYSTEM_ADDRESS},
    test_utils::MemoryDatabase,
    BlockGasCounters, BlockLimiter, BlockLimits, HardforkParams, LimitUsage, MegaBlockExecutionCtx,
    MegaBlockExecutor, MegaEvmFactory, MegaHardforkConfig, MegaTxEnvelope, ProtocolLimits,
    WRITE_RECORD_SIZE,
};
use op_alloy_consensus::TxDeposit;
use proptest::prelude::*;
use revm::{
    context::{transaction::AccessList, BlockEnv},
    database::State,
    Database,
};

use crate::{
    gen::{
        case::{case, Case, Envs},
        tx::{GasTier, Price},
        value, who, Flavor, Value, Who, CALLER, CALLER2, CHAIN_ID,
    },
    harness::{check, fail, prop_check, prop_eq},
};

/// The number of cases the block properties run in the bounded mode.
const CASES: u32 = 256;

/// The header's gas limit: room for an above-cap transaction and a few more.
const BLOCK_GAS_LIMIT: u64 = 300_000_000;

/// Who sends a block's transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sender {
    Caller,
    Caller2,
}

impl Sender {
    const fn address(self) -> Address {
        match self {
            Self::Caller => CALLER,
            Self::Caller2 => CALLER2,
        }
    }
}

/// The envelope a block transaction takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Envelope {
    Legacy { to: Who },
    Eip1559 { to: Who },
    Eip2930 { to: Who, slots: u8 },
    Create,
    Deposit { to: Who, mint: Value, create: bool },
}

/// One transaction of a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockTx {
    pub(crate) sender: Sender,
    pub(crate) envelope: Envelope,
    pub(crate) value: Value,
    pub(crate) gas: GasTier,
    pub(crate) price: Price,
    pub(crate) data_len: u16,
}

fn block_tx() -> impl Strategy<Value = BlockTx> {
    let envelope = prop_oneof![
        5 => who().prop_map(|to| Envelope::Legacy { to }),
        3 => who().prop_map(|to| Envelope::Eip1559 { to }),
        2 => (who(), 0u8..=3).prop_map(|(to, slots)| Envelope::Eip2930 { to, slots }),
        2 => Just(Envelope::Create),
        3 => (who(), value(), prop_oneof![4 => Just(false), 1 => Just(true)])
            .prop_map(|(to, mint, create)| Envelope::Deposit { to, mint, create }),
    ];
    (
        prop_oneof![3 => Just(Sender::Caller), 1 => Just(Sender::Caller2)],
        envelope,
        value(),
        prop_oneof![
            1 => Just(GasTier::Tiny),
            4 => Just(GasTier::Small),
            4 => Just(GasTier::Medium),
            2 => Just(GasTier::Large),
            1 => Just(GasTier::AboveCap),
        ],
        prop_oneof![5 => Just(Price::Free), 2 => Just(Price::One), 1 => Just(Price::Dear)],
        0u16..=100,
    )
        .prop_map(|(sender, envelope, value, gas, price, data_len)| BlockTx {
            sender,
            envelope,
            value,
            gas,
            price,
            data_len,
        })
}

/// One of the block's four protocol budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Budget {
    Unlimited,
    /// Crossed by the first transaction that spends anything on the dimension.
    One,
    /// A small number in the dimension's unit.
    Small(u32),
    /// A number a few transactions fit under.
    Medium(u32),
}

impl Budget {
    fn value(self) -> u64 {
        match self {
            Self::Unlimited => u64::MAX,
            Self::One => 1,
            Self::Small(v) | Self::Medium(v) => u64::from(v),
        }
    }
}

fn budget(
    small: std::ops::RangeInclusive<u32>,
    medium: std::ops::RangeInclusive<u32>,
) -> impl Strategy<Value = Budget> {
    prop_oneof![
        4 => Just(Budget::Unlimited),
        1 => Just(Budget::One),
        2 => small.prop_map(Budget::Small),
        2 => medium.prop_map(Budget::Medium),
    ]
}

/// The block's protocol budgets and the builder's policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Budgets {
    pub(crate) execution: Budget,
    pub(crate) state: Budget,
    pub(crate) data: Budget,
    pub(crate) kv: Budget,
    /// The policy's caps on the same four, which only tighten.
    pub(crate) policy_execution: Budget,
    pub(crate) policy_state: Budget,
    pub(crate) policy_data: Budget,
    pub(crate) policy_kv: Budget,
    /// The policy's cap on a transaction's declared gas.
    pub(crate) policy_tx_gas: Option<u32>,
    /// The policy's cap on a transaction's encoded size, in bytes.
    pub(crate) policy_tx_size: Option<u16>,
    /// The policy's cap on the block's encoded size, in bytes.
    pub(crate) policy_block_size: Option<u16>,
}

fn budgets() -> impl Strategy<Value = Budgets> {
    (
        (
            budget(20_000..=200_000, 500_000..=5_000_000),
            budget(1_000..=200_000, 300_000..=2_000_000),
            budget(300..=600, 1_000..=5_000),
            budget(1..=3, 4..=12),
        ),
        (
            budget(20_000..=200_000, 500_000..=5_000_000),
            budget(1_000..=200_000, 300_000..=2_000_000),
            budget(300..=600, 1_000..=5_000),
            budget(1..=3, 4..=12),
        ),
        proptest::option::weighted(
            0.3,
            prop_oneof![Just(100_000u32), Just(1_000_000), Just(50_000_000)],
        ),
        proptest::option::weighted(0.3, prop_oneof![Just(80u16), Just(150), Just(2_000)]),
        proptest::option::weighted(0.3, prop_oneof![Just(200u16), Just(500), Just(5_000)]),
    )
        .prop_map(
            |((execution, state, data, kv), (pe, ps, pd, pk), tx_gas, tx_size, block_size)| {
                Budgets {
                    execution,
                    state,
                    data,
                    kv,
                    policy_execution: pe,
                    policy_state: ps,
                    policy_data: pd,
                    policy_kv: pk,
                    policy_tx_gas: tx_gas,
                    policy_tx_size: tx_size,
                    policy_block_size: block_size,
                }
            },
        )
}

/// A block: a world with its programs (the case's transaction is not used), the transactions, the
/// budgets and the policy.
#[derive(Clone, Debug)]
pub(crate) struct BlockCase {
    pub(crate) base: Case,
    pub(crate) txs: Vec<BlockTx>,
    pub(crate) budgets: Budgets,
}

pub(crate) fn block_case() -> impl Strategy<Value = BlockCase> {
    (case(Flavor::Satin), proptest::collection::vec(block_tx(), 1..=6), budgets())
        .prop_map(|(base, txs, budgets)| BlockCase { base, txs, budgets })
}

impl BlockCase {
    fn protocol(&self) -> ProtocolLimits {
        ProtocolLimits {
            tx_runtime_limits: self.base.world.limits.runtime(),
            block_execution_gas_limit: self.budgets.execution.value(),
            block_state_gas_limit: self.budgets.state.value(),
            block_txs_data_limit: self.budgets.data.value(),
            block_kv_update_limit: self.budgets.kv.value(),
        }
    }

    fn policy(&self) -> BlockLimits {
        let b = &self.budgets;
        BlockLimits {
            tx_gas_limit: b.policy_tx_gas.map_or(u64::MAX, u64::from),
            tx_encode_size_limit: b.policy_tx_size.map_or(u64::MAX, u64::from),
            block_txs_encode_size_limit: b.policy_block_size.map_or(u64::MAX, u64::from),
            block_execution_gas_limit: b.policy_execution.value(),
            block_state_gas_limit: b.policy_state.value(),
            block_txs_data_limit: b.policy_data.value(),
            block_kv_update_limit: b.policy_kv.value(),
            ..BlockLimits::no_limits()
        }
    }

    fn spec(&self) -> MegaHardforkConfig {
        MegaHardforkConfig::default()
            .with_all_activated()
            .with_params(SequencerRegistryConfig {
                initial_system_address: MEGA_SYSTEM_ADDRESS,
                initial_sequencer: Address::repeat_byte(0x22),
                initial_admin: Address::repeat_byte(0x33),
                initial_from_block: 1,
                min_rotation_delay: 100,
            })
            .with_params(self.protocol())
    }

    fn block_env(&self) -> BlockEnv {
        BlockEnv { gas_limit: BLOCK_GAS_LIMIT, ..self.base.block() }
    }

    /// The state the block runs on: the case's pre-state with the second sender funded.
    fn state(&self) -> State<MemoryDatabase> {
        let mut db = self.base.database();
        db.set_account_balance(CALLER2, U256::from(10u64.pow(18)));
        State::builder().with_database(db).build()
    }

    /// The `index`th transaction as an envelope with its sender recovered, at `nonce`.
    fn envelope(&self, index: usize, nonce: u64) -> Recovered<MegaTxEnvelope> {
        let tx = &self.txs[index];
        let hash = keccak256((index as u64).to_be_bytes());
        let input: Bytes = vec![0xab; tx.data_len as usize].into();
        let init: Bytes = self.base.main.assemble().into();
        let gas_limit = tx.gas.limit();
        let value = tx.value.wei();
        let signature = Signature::test_signature();
        let envelope = match tx.envelope {
            Envelope::Legacy { to } => MegaTxEnvelope::Legacy(Signed::new_unchecked(
                TxLegacy {
                    chain_id: Some(CHAIN_ID),
                    nonce,
                    gas_price: tx.price.wei(),
                    gas_limit,
                    to: TxKind::Call(to.address()),
                    value,
                    input,
                },
                signature,
                hash,
            )),
            Envelope::Eip1559 { to } => MegaTxEnvelope::Eip1559(Signed::new_unchecked(
                TxEip1559 {
                    chain_id: CHAIN_ID,
                    nonce,
                    gas_limit,
                    max_fee_per_gas: tx.price.wei(),
                    max_priority_fee_per_gas: tx.price.wei(),
                    to: TxKind::Call(to.address()),
                    value,
                    access_list: AccessList::default(),
                    input,
                },
                signature,
                hash,
            )),
            Envelope::Eip2930 { to, slots } => MegaTxEnvelope::Eip2930(Signed::new_unchecked(
                TxEip2930 {
                    chain_id: CHAIN_ID,
                    nonce,
                    gas_price: tx.price.wei(),
                    gas_limit,
                    to: TxKind::Call(to.address()),
                    value,
                    access_list: AccessList(vec![alloy_eips::eip2930::AccessListItem {
                        address: to.address(),
                        storage_keys: (0..slots).map(B256::with_last_byte).collect(),
                    }]),
                    input,
                },
                signature,
                hash,
            )),
            Envelope::Create => MegaTxEnvelope::Legacy(Signed::new_unchecked(
                TxLegacy {
                    chain_id: Some(CHAIN_ID),
                    nonce,
                    gas_price: tx.price.wei(),
                    gas_limit,
                    to: TxKind::Create,
                    value,
                    input: init,
                },
                signature,
                hash,
            )),
            Envelope::Deposit { to, mint, create } => {
                MegaTxEnvelope::Deposit(Sealed::new_unchecked(
                    TxDeposit {
                        source_hash: hash,
                        from: tx.sender.address(),
                        to: if create { TxKind::Create } else { TxKind::Call(to.address()) },
                        mint: mint.wei().saturating_to(),
                        value,
                        gas_limit,
                        is_system_transaction: false,
                        input: if create { init } else { input },
                    },
                    hash,
                ))
            }
        };
        Recovered::new_unchecked(envelope, tx.sender.address())
    }
}

/// Whether the block admits a transaction of `gas_limit` and `tx_size` before it runs, by the
/// limiter's rules: a deposit is held to the header's gas limit alone; any other transaction to
/// the policy's per-transaction caps and to the four packing budgets, which refuse the next
/// transaction once reached.
fn admits_before(limiter: &BlockLimiter, gas_limit: u64, tx_size: u64, is_deposit: bool) -> bool {
    let limits = &limiter.limits;
    if !is_deposit && gas_limit > limits.tx_gas_limit {
        return false;
    }
    if limiter.block_gas_used.saturating_add(gas_limit) > limits.block_gas_limit {
        return false;
    }
    if is_deposit {
        return true;
    }
    tx_size <= limits.tx_encode_size_limit &&
        tx_size.saturating_add(limiter.block_tx_size_used) <= limits.block_txs_encode_size_limit &&
        limiter.gas.execution < limits.block_execution_gas_limit &&
        limiter.usage.data_size < limits.block_txs_data_limit &&
        limiter.usage.write_records < limits.block_kv_update_limit
}

/// Whether the block still admits an executed transaction that adds `state_gas`: the state-gas
/// budget refuses one that adds any once reached, and never a deposit. The executor applies it
/// after the transaction ran, in `run_transaction`, and again at commit.
fn admits_after(limiter: &BlockLimiter, state_gas: u64, is_deposit: bool) -> bool {
    is_deposit || state_gas == 0 || limiter.gas.state < limiter.limits.block_state_gas_limit
}

/// Whether a refusal is the state-gas budget's, applied after the transaction ran.
fn is_state_gas_refusal(error: &BlockExecutionError) -> bool {
    match error {
        BlockExecutionError::Validation(BlockValidationError::InvalidTx { error, .. }) => {
            (error.as_ref() as &dyn std::any::Any)
                .downcast_ref::<mega_evm::MegaBlockLimitExceededError>()
                .is_some_and(|e| {
                    matches!(e, mega_evm::MegaBlockLimitExceededError::StateGasLimit { .. })
                })
        }
        _ => false,
    }
}

/// Whether a refusal names a block or transaction limit rather than the transaction's validity:
/// `MegaETH`'s limit errors have no upstream `InvalidTransaction` counterpart, and the header's
/// gas limit has its own variant.
fn is_limit_refusal(error: &BlockExecutionError) -> bool {
    match error {
        BlockExecutionError::Validation(BlockValidationError::InvalidTx { error, .. }) => {
            error.as_invalid_tx_err().is_none()
        }
        BlockExecutionError::Validation(
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
        ) => true,
        _ => false,
    }
}

/// What running a block produced, rendered so two runs compare.
#[derive(Debug, Default)]
struct BlockRun {
    committed: Vec<usize>,
    refused: Vec<(usize, String)>,
    gas: BlockGasCounters,
    gas_used: u64,
    usage: LimitUsage,
    receipts: String,
    accounts: String,
}

/// Runs the block and checks the admission rules, the counters and the receipts along the way.
fn run_block(case: &BlockCase) -> Result<BlockRun, proptest::test_runner::TestCaseError> {
    let protocol = case.protocol();
    prop_check!(
        protocol.validate().is_ok(),
        "the generated protocol limits are valid: {protocol:?}"
    );
    let envs: Envs = case.base.envs();
    let mut state = case.state();
    let mut cfg = revm::context::CfgEnv::new_with_spec(mega_evm::MegaSpecId::SATIN);
    cfg.chain_id = CHAIN_ID;
    let evm = MegaEvmFactory::new()
        .with_external_env_factory(envs)
        .create_evm(&mut state, EvmEnv::new(cfg, case.block_env()));
    let ctx = MegaBlockExecutionCtx::new(B256::ZERO, Some(B256::ZERO), Bytes::new(), case.policy());
    let mut executor =
        MegaBlockExecutor::new(evm, ctx, case.spec(), OpAlloyReceiptBuilder::default());
    executor
        .apply_pre_execution_changes()
        .map_err(|error| fail(format!("the block does not start: {error:?}")))?;

    let mut run = BlockRun::default();
    let mut expected = BlockGasCounters::default();
    let mut expected_gas_used = 0u64;
    let mut expected_usage = LimitUsage::default();
    for index in 0..case.txs.len() {
        let sender = case.txs[index].sender.address();
        let nonce = executor
            .evm_mut()
            .db_mut()
            .basic(sender)
            .map_err(|error| fail(format!("{error:?}")))?
            .map_or(0, |info| info.nonce);
        let tx = case.envelope(index, nonce);
        let is_deposit = matches!(case.txs[index].envelope, Envelope::Deposit { .. });
        let tx_size = tx.inner().encode_2718_len() as u64;
        let gas_limit = case.txs[index].gas.limit();
        let admitted = admits_before(executor.limiter(), gas_limit, tx_size, is_deposit);
        match executor.run_transaction(&tx) {
            Ok(outcome) => {
                prop_check!(
                    admitted,
                    "tx {index} ran though the block's rules refuse it\n{:?}",
                    executor.limiter()
                );
                let gas = outcome.gas;
                let result_gas = outcome.result.gas();
                prop_eq!(
                    gas.regular + gas.state + gas.history,
                    result_gas.total_gas_spent(),
                    "tx {index}: the ledgers add up"
                );
                prop_check!(
                    outcome.usage.write_records * WRITE_RECORD_SIZE <= outcome.usage.data_size,
                    "tx {index}: KV x 40 within the data size"
                );
                let fits = admits_after(executor.limiter(), gas.state, is_deposit);
                let logs = outcome.result.logs().to_vec();
                // What the transaction itself reports it kept, taken before the commit: the
                // block's usage must be the sum of these, not a figure read back from the block.
                let usage = outcome.usage;
                match executor.commit_transaction_outcome(outcome) {
                    Ok(_) => {
                        prop_check!(
                            fits,
                            "tx {index} was committed though the state-gas budget refuses it"
                        );
                        expected.record(&gas);
                        expected_gas_used += gas.gas_used;
                        expected_usage.data_size += usage.data_size;
                        expected_usage.write_records += usage.write_records;
                        let receipt = executor
                            .receipts()
                            .last()
                            .expect("the committed transaction has a receipt");
                        prop_eq!(
                            alloy_consensus::TxReceipt::cumulative_gas_used(receipt),
                            expected_gas_used,
                            "tx {index}: the receipt's cumulative gas is the running sum"
                        );
                        prop_eq!(
                            alloy_consensus::TxReceipt::logs(receipt).to_vec(),
                            logs,
                            "tx {index}: the receipt's logs"
                        );
                        run.committed.push(index);
                    }
                    Err(error) => {
                        prop_check!(
                            !fits,
                            "tx {index} was refused at commit though it fits: {error:?}"
                        );
                        run.refused.push((index, format!("{error:?}")));
                    }
                }
            }
            Err(error) => {
                let message = format!("{error:?}");
                let on_a_limit = is_limit_refusal(&error);
                if admitted && is_state_gas_refusal(&error) {
                    // The transaction ran and added state gas once the budget was reached.
                    prop_check!(
                        !is_deposit && !admits_after(executor.limiter(), 1, false),
                        "tx {index} was refused on the state-gas budget the rules do not name: {message}\n{:?}",
                        executor.limiter()
                    );
                } else if admitted {
                    prop_check!(
                        !on_a_limit,
                        "tx {index} was refused on a limit the rules do not name: {message}\n{:?}",
                        executor.limiter()
                    );
                } else {
                    prop_check!(
                        on_a_limit,
                        "tx {index} was refused for another reason than the rule: {message}"
                    );
                }
                if is_deposit {
                    prop_check!(
                        !on_a_limit || message.contains("MoreThanAvailableBlockGas"),
                        "a deposit refused on a packing budget: {message}"
                    );
                }
                run.refused.push((index, message));
            }
        }
    }
    prop_eq!(*executor.gas(), expected, "the block's counters are the sum of its transactions'");
    prop_eq!(
        executor.limiter().block_gas_used,
        expected_gas_used,
        "the block's gas used is its receipts' sum"
    );
    prop_eq!(
        executor.limiter().usage,
        expected_usage,
        "the block's usage is the sum of its transactions'"
    );
    prop_eq!(
        executor.receipts().len(),
        run.committed.len(),
        "one receipt per committed transaction"
    );
    run.gas = *executor.gas();
    run.gas_used = executor.limiter().block_gas_used;
    run.usage = executor.limiter().usage;
    run.receipts = format!("{:?}", executor.receipts());
    let (_, result) = executor
        .finish_with_counters()
        .map_err(|error| fail(format!("the block does not finish: {error:?}")))?;
    prop_eq!(result.gas, expected, "the finished block carries the counters");
    prop_eq!(result.gas_used, expected_gas_used, "the finished block carries the gas used");
    prop_eq!(result.usage, expected_usage, "the finished block carries the usage");
    for address in [
        CALLER,
        CALLER2,
        Who::Contract.address(),
        Who::A.address(),
        Who::B.address(),
        Who::Fresh.address(),
        case.base.world.beneficiary_address(),
    ] {
        let info = state.basic(address).map_err(|error| fail(format!("{error:?}")))?;
        run.accounts.push_str(&format!("{address}: {info:?}\n"));
    }
    Ok(run)
}

/// A block's counters are the sum of its committed transactions', its gas used the sum of its
/// receipts', and every budget holds as the block executor states it: a transaction runs only if
/// the packing budgets and the policy admit it, is refused after it ran if it adds state gas once
/// the state-gas budget is reached, and a deposit is refused by the header's gas limit alone.
#[test]
fn test_property_block_counters_and_budgets() {
    check("block_counters_and_budgets", CASES, block_case, |case| {
        run_block(case)?;
        Ok(())
    });
}

/// The same block twice gives the same committed set, refusals, counters, receipts and accounts.
#[test]
fn test_property_block_determinism() {
    check("block_determinism", CASES / 2, block_case, |case| {
        let first = format!("{:?}", run_block(case)?);
        let second = format!("{:?}", run_block(case)?);
        prop_eq!(first, second, "two runs of the same block differ");
        Ok(())
    });
}
