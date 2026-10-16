//! The chain's limits and the builder's policy: the limits a block is held to are the ones its
//! schedule carries, whatever the node puts in the block context, and a builder's policy changes
//! which transactions it packs, never what a packed block computes.

use alloy_consensus::transaction::Recovered;
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, LimitCheck, LimitKind, MegaHardforkConfig, MegaTransactionExt, MegaTxEnvelope,
    ProtocolLimits, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use op_alloy_consensus::OpReceiptEnvelope;
use revm::{
    bytecode::opcode::{CALLDATALOAD, SSTORE},
    database::State,
    Database as _,
};

use crate::common::{self, incompressible};

/// A contract that writes one fresh slot.
const WRITES_ONE: Address = address!("0x1000000000000000000000000000000000000011");

/// A contract that writes two fresh slots.
const WRITES_TWO: Address = address!("0x1000000000000000000000000000000000000012");

/// A contract that writes the slot its calldata names.
const WRITES_NAMED: Address = address!("0x1000000000000000000000000000000000000013");

/// The gas limit of the transactions here: enough for two writes at any byte price a measurement
/// build runs, so what stops or refuses them is a limit, never their gas.
const GAS_LIMIT: u64 = 2_000_000;

/// Senders, one per transaction, so a transaction a builder leaves out does not stall the nonces
/// of the ones after it.
const SENDERS: [Address; 5] = [
    address!("0x3000000000000000000000000000000000000001"),
    address!("0x3000000000000000000000000000000000000002"),
    address!("0x3000000000000000000000000000000000000003"),
    address!("0x3000000000000000000000000000000000000004"),
    address!("0x3000000000000000000000000000000000000005"),
];

/// The database every block here runs on: the three contracts, and every sender funded.
fn database() -> MemoryDatabase {
    let mut db = common::database();
    db.set_account_code(
        WRITES_ONE,
        BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build(),
    );
    db.set_account_code(
        WRITES_TWO,
        BytecodeBuilder::default()
            .sstore(U256::from(1), U256::from(1))
            .sstore(U256::from(2), U256::from(1))
            .stop()
            .build(),
    );
    db.set_account_code(
        WRITES_NAMED,
        BytecodeBuilder::default()
            .push_number(1_u8)
            .push_number(0_u8)
            .append(CALLDATALOAD)
            .append(SSTORE)
            .stop()
            .build(),
    );
    for sender in SENDERS {
        db.set_account_balance(sender, U256::from(1_000_000_000_000_000_u64));
    }
    db
}

/// A transaction from `sender`, its first, to `to` carrying `input`.
fn tx_from(
    sender: Address,
    to: Address,
    input: Bytes,
    gas_limit: u64,
) -> Recovered<MegaTxEnvelope> {
    Recovered::new_unchecked(common::tx(0, to, input, gas_limit), sender)
}

/// A schedule that carries `limits` the way a chain configuration does: through the checked
/// route, so the values are ones a chain may carry.
fn chain_with(limits: ProtocolLimits) -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with_all_activated()
        .with_params(common::registry_config())
        .with_params(limits)
}

/// A chain whose limits are not the defaults is held to its own: a transaction that crosses the
/// chain's data-size limit is stopped at that limit, and the block refuses a transaction once it
/// reached the chain's KV budget. The node passed no limits at all.
#[test]
fn test_a_block_is_held_to_the_limits_its_chain_carries() {
    // One storage write fits the transaction's data size; a second crosses it.
    let tx_data_size_limit = TX_BODY_SIZE + WRITE_RECORD_SIZE;
    let limits = ProtocolLimits::DEFAULT
        .with_tx_runtime_limits(
            ProtocolLimits::DEFAULT.tx_runtime_limits.with_tx_data_size_limit(tx_data_size_limit),
        )
        .with_block_kv_update_limit(1);
    assert_ne!(limits, ProtocolLimits::DEFAULT);

    let mut state = State::builder().with_database(database()).build();
    let mut executor = common::executor_with_spec(
        &mut state,
        common::block_ctx(BlockLimits::default()),
        chain_with(limits),
    );
    assert_eq!(executor.protocol_limits(), Some(&limits));
    assert_eq!(*executor.evm().tx_runtime_limits(), limits.tx_runtime_limits);
    assert_eq!(executor.limiter().limits.block_kv_update_limit, 1);
    executor.apply_pre_execution_changes().expect("the block starts");

    // Two writes: stopped at the chain's transaction limit, and included with the stop.
    let outcome = executor
        .run_transaction(&tx_from(SENDERS[0], WRITES_TWO, Bytes::new(), GAS_LIMIT))
        .expect("the transaction runs");
    assert_eq!(
        outcome.inner.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: tx_data_size_limit,
            used: TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE,
            frame_local: false,
        })
    );
    executor.commit_transaction_outcome(outcome).expect("the stopped transaction is included");
    assert_eq!(executor.limiter().usage.write_records, 0, "the stop kept no record");

    // One write fits, and fills the chain's KV budget.
    let outcome = executor
        .run_transaction(&tx_from(SENDERS[1], WRITES_ONE, Bytes::new(), GAS_LIMIT))
        .expect("the transaction runs");
    assert!(outcome.inner.result.is_success(), "{:?}", outcome.inner.result);
    executor.commit_transaction_outcome(outcome).expect("it is included");
    assert_eq!(executor.limiter().usage.write_records, 1);

    // The block has reached the chain's KV budget: the next transaction is refused.
    let err = executor
        .run_transaction(&tx_from(SENDERS[2], WRITES_ONE, Bytes::new(), GAS_LIMIT))
        .expect_err("the block's write records have reached the chain's limit");
    assert!(err.to_string().contains("Block KV update limit reached"), "{err}");
    assert!(err.to_string().contains("limit=1"), "{err}");

    // On the default chain the same two-write transaction keeps both writes.
    let mut state = State::builder().with_database(database()).build();
    let mut executor = common::executor_with_spec(
        &mut state,
        common::unlimited_ctx(),
        chain_with(ProtocolLimits::DEFAULT),
    );
    executor.apply_pre_execution_changes().expect("the block starts");
    let outcome = executor
        .run_transaction(&tx_from(SENDERS[0], WRITES_TWO, Bytes::new(), GAS_LIMIT))
        .expect("the transaction runs");
    assert!(outcome.inner.result.is_success(), "{:?}", outcome.inner.result);
    assert_eq!(outcome.inner.limit_exceeded, None);
}

/// What a block produced, with the state it left behind: the receipts, the counters and every
/// account and slot the candidates could have touched.
#[derive(Debug, PartialEq)]
struct Block {
    receipts: Vec<OpReceiptEnvelope>,
    gas_used: u64,
    blob_gas_used: u64,
    counters: (mega_evm::BlockGasCounters, mega_evm::LimitUsage),
    accounts: Vec<(Address, Option<(U256, u64)>)>,
    slots: Vec<U256>,
}

/// Executes `candidates` in order on a fresh block of the chain `limits` describes, packed under
/// `policy`: a candidate the block refuses is left out, as a builder leaves it out. Returns the
/// block, and for each candidate whether it was packed or the refusal that left it out.
fn build(
    limits: ProtocolLimits,
    policy: BlockLimits,
    candidates: &[Recovered<MegaTxEnvelope>],
) -> (Block, Vec<Result<(), String>>) {
    let mut state = State::builder().with_database(database()).build();
    let result = {
        let mut executor =
            common::executor_with_spec(&mut state, common::block_ctx(policy), chain_with(limits));
        executor.apply_pre_execution_changes().expect("the block starts");
        let packed: Vec<_> = candidates
            .iter()
            .map(|candidate| {
                executor.execute_transaction(candidate).map(drop).map_err(|err| err.to_string())
            })
            .collect();
        let (_, result) = executor.finish_with_counters().expect("the block finishes");
        (result, packed)
    };
    let (result, packed) = result;
    let block = Block {
        receipts: result.inner.receipts,
        gas_used: result.inner.gas_used,
        blob_gas_used: result.inner.blob_gas_used,
        counters: (result.gas, result.usage),
        accounts: SENDERS
            .iter()
            .chain([&Address::ZERO])
            .map(|address| {
                let info = state.basic(*address).expect("readable");
                (*address, info.map(|info| (info.balance, info.nonce)))
            })
            .collect(),
        slots: (0..6_u64)
            .map(|slot| state.storage(WRITES_NAMED, U256::from(slot)).expect("readable"))
            .collect(),
    };
    (block, packed)
}

/// A builder packs under its own policy — a data-availability cap on one transaction, a cap on a
/// transaction's declared gas, and a block data-size cap below the chain's — and leaves out what
/// it refuses. A validator re-executing the block it packed, under no policy or under a policy of
/// its own that differs, computes the same block: the same receipts, counters and state. A
/// validator on a chain whose limits differ does not, which is why those travel with the chain.
#[test]
fn test_a_builder_and_a_validator_that_differ_on_building_policy_agree_on_the_block() {
    let limits = ProtocolLimits::DEFAULT;
    let writes = |sender, slot: u8, input_len: usize, gas_limit| {
        let mut input = vec![0_u8; 32];
        input[31] = slot;
        input.extend_from_slice(&incompressible(input_len));
        tx_from(sender, WRITES_NAMED, input.into(), gas_limit)
    };
    // The builder's cap on a transaction's declared gas is the candidates' own, so only the one
    // candidate above it is refused for gas.
    let gas = GAS_LIMIT;
    let candidates = [
        writes(SENDERS[0], 1, 0, gas),
        // Refused by the builder's data-availability cap on one transaction.
        writes(SENDERS[1], 2, 4_000, gas),
        // Refused by the builder's cap on a transaction's declared gas.
        writes(SENDERS[2], 3, 0, gas + 1),
        // Packed: it is the transaction that reaches the builder's data-size cap.
        writes(SENDERS[3], 4, 0, gas),
        // Refused: the block has reached the builder's data-size cap, not the chain's.
        writes(SENDERS[4], 5, 0, gas),
    ];
    let da_size = |index: usize| MegaTransactionExt::estimated_da_size(&candidates[index]);
    assert!(da_size(1) > 2_000 && da_size(0) < 1_000, "{} {}", da_size(0), da_size(1));

    // What one small write keeps of data size, so the builder's cap lands after two of them.
    let (probe, _) = build(limits, BlockLimits::default(), &candidates[..1]);
    let one_write = probe.counters.1.data_size;
    assert_eq!(one_write, TX_BODY_SIZE + 32 + WRITE_RECORD_SIZE);

    let builder_policy = BlockLimits::no_limits()
        .with_tx_da_size_limit(2_000)
        .with_tx_gas_limit(gas)
        .with_block_txs_data_limit(2 * one_write);
    let (built, outcomes) = build(limits, builder_policy, &candidates);
    let refusal = |index: usize| outcomes[index].clone().expect_err("the builder refused it");
    assert!(
        refusal(1).contains("Transaction data availability size limit exceeded"),
        "{}",
        refusal(1)
    );
    assert!(
        refusal(2).contains(&format!(
            "Transaction gas limit exceeded: tx_gas_limit={} > limit={gas}",
            gas + 1
        )),
        "{}",
        refusal(2)
    );
    assert!(
        refusal(4).contains(&format!(
            "Block transactions data limit reached: block_used={0} >= limit={0}",
            2 * one_write
        )),
        "{}",
        refusal(4)
    );
    let packed: Vec<_> = (0..candidates.len()).filter(|index| outcomes[*index].is_ok()).collect();
    assert_eq!(packed, [0, 3], "the builder's policy decided what it packed");
    assert_eq!(built.receipts.len(), 2);
    assert!(built.receipts.iter().all(|receipt| receipt.status()));
    assert!(
        limits.block_txs_data_limit > built.counters.1.data_size,
        "the block is well inside the chain's own data-size budget"
    );

    let block: Vec<_> = packed.iter().map(|index| candidates[*index].clone()).collect();
    let validator_policies = [
        BlockLimits::default(),
        // A validator whose node was given a policy of its own, looser than the builder's.
        BlockLimits::no_limits().with_tx_da_size_limit(8_000).with_block_txs_data_limit(10_000),
    ];
    for policy in validator_policies {
        let (validated, outcomes) = build(limits, policy, &block);
        assert_eq!(outcomes, [Ok(()), Ok(())], "{policy:?}: the validator packs the whole block");
        assert_eq!(validated, built, "{policy:?}: the validator computes the builder's block");
    }

    // The same block under a chain whose limits differ computes a different result: the first
    // transaction's write crosses that chain's data-size limit and is stopped.
    let stricter = ProtocolLimits::DEFAULT.with_tx_runtime_limits(
        ProtocolLimits::DEFAULT.tx_runtime_limits.with_tx_data_size_limit(TX_BODY_SIZE + 32),
    );
    let (elsewhere, outcomes) = build(stricter, BlockLimits::default(), &block);
    assert_eq!(outcomes, [Ok(()), Ok(())], "the stopped transactions are packed all the same");
    assert_ne!(elsewhere.receipts, built.receipts);
    assert!(!elsewhere.receipts[0].status(), "the stop is a failed receipt");
    assert!(elsewhere.slots[1].is_zero(), "and its write is not kept");
}
