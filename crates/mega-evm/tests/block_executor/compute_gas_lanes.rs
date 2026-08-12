//! The Rex7 executed / destroyed compute-gas split, as the block sees it.
//!
//! A frame that halts exceptionally destroys the budget it was still holding. Rex7 reports that
//! remainder as compute gas but enforces no limit against it — the transaction level already
//! settled that, and the block level has to reach the same answer, because a transaction that
//! destroyed a large gas envelope while performing almost no work would otherwise close the
//! block's compute capacity for every transaction behind it.
//!
//! So the block keeps two compute-gas counters: `block_compute_gas_used` reports every
//! transaction's whole total, and `block_compute_gas_enforced` carries only the work performed and
//! is what admission compares. These tests drive both counters through the real commit path, in
//! the two shapes that produce a destroyed remainder — an ordinary exceptional frame, and one
//! nested inside the `KeylessDeploy` sandbox, which merges a whole separate tracker back across a
//! boundary the classification has to survive.
//!
//! Nothing is destroyed before Rex7, so the frozen specs are pinned here too: the two counters
//! must not diverge by so much as a gas unit under Rex6.

use std::convert::Infallible;

use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall as _;
use mega_evm::{
    alloy_consensus::{transaction::Recovered, Signed, TxLegacy},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, IKeylessDeploy, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory,
    MegaHardforkConfig, MegaSpecId, MegaTxEnvelope, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    bytecode::opcode::{ADD, STOP},
    context::BlockEnv,
    database::State,
};

/// Sends every transaction in these tests.
const CALLER: Address = address!("2000000000000000000000000000000000000002");
/// `ADD` on an empty stack: the call halts on its first opcode, so nearly the whole envelope it
/// was forwarded is destroyed rather than performed.
const HALTING: Address = address!("1000000000000000000000000000000000000001");
/// `STOP`: the cheap transaction that has to still fit in the block afterwards.
const CHEAP: Address = address!("1000000000000000000000000000000000000002");

/// Gas envelope the halting transaction destroys. Large enough that its reported compute total
/// alone dwarfs [`BLOCK_COMPUTE_GAS_LIMIT`], while the work it performed before failing — one
/// three-gas `ADD` on top of the intrinsic cost — stays far below it.
const HALTING_TX_GAS_LIMIT: u64 = 5_000_000;

/// Gas envelope the `KeylessDeploy` sandbox destroys, passed as the call's gas-limit override.
const SANDBOX_GAS_OVERRIDE: u64 = 4_000_000;

/// The block compute-gas ceiling these tests build around: above what any of these transactions
/// executes, below what the halting ones report.
const BLOCK_COMPUTE_GAS_LIMIT: u64 = 1_000_000;

/// Builds a legacy transaction from `CALLER`.
fn envelope(nonce: u64, gas_limit: u64, to: Address, input: Bytes) -> MegaTxEnvelope {
    let tx = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input,
    };
    MegaTxEnvelope::Legacy(Signed::new_unchecked(tx, Signature::test_signature(), B256::ZERO))
}

/// The pre-EIP-155 deployment transaction the `KeylessDeploy` sandbox replays, carrying a
/// constructor that halts on its first opcode.
fn keyless_deploy_call_data() -> Bytes {
    let init_code = BytecodeBuilder::default().append(ADD).append(STOP).build();
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: 200_000,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
        chain_id: None,
    };
    let word = U256::from_be_bytes([0x33; 32]);
    let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
    let mut encoded = Vec::new();
    signed.rlp_encode(&mut encoded);

    Bytes::from(
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: Bytes::from(encoded),
            gasLimitOverride: U256::from(SANDBOX_GAS_OVERRIDE),
        }
        .abi_encode(),
    )
}

/// The database every test here runs against: the two callees plus a funded sender.
fn build_db() -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_code(HALTING, BytecodeBuilder::default().append(ADD).append(STOP).build());
    db.set_account_code(CHEAP, BytecodeBuilder::default().stop().build());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000_000u64));
    db
}

/// What one committed transaction contributed, and where the block's counters stood afterwards.
#[derive(Debug, Clone, Copy)]
struct Contribution {
    /// Whether the transaction's own execution result reports success.
    succeeded: bool,
    /// The transaction's full reported compute total.
    reported: u64,
    /// The part of `reported` its exceptionally halted frames destroyed.
    destroyed: u64,
    /// The block's reported compute counter after the commit.
    block_reported: u64,
    /// The block's enforced compute counter after the commit.
    block_enforced: u64,
    /// Whether the block considers itself full after the commit.
    block_full: bool,
}

/// Runs `txs` through one block at `spec`, committing each in turn, and returns what each
/// contributed. Stops at the first transaction the block refuses to admit, so the returned vector
/// is shorter than `txs` exactly when the block closed early.
fn run_block(
    spec: MegaSpecId,
    block_compute_gas_limit: u64,
    txs: &[MegaTxEnvelope],
) -> Vec<Contribution> {
    let mut db = build_db();
    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = spec;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm = evm_factory.create_evm(&mut state, EvmEnv::new(cfg_env, block_env));

    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits().with_block_compute_gas_limit(block_compute_gas_limit),
    );
    let chain_spec = MegaHardforkConfig::default().with_all_activated_through(spec);
    let mut executor =
        MegaBlockExecutor::new(evm, block_ctx, chain_spec, OpAlloyReceiptBuilder::default());

    let mut contributions = Vec::new();
    for tx in txs {
        let Ok(outcome) = executor.run_transaction(Recovered::new_unchecked(tx, CALLER)) else {
            break;
        };
        let succeeded = outcome.result.is_success();
        let reported = outcome.compute_gas_used;
        let destroyed = outcome.compute_gas_destroyed;
        executor.commit_transaction_outcome(outcome).expect("the commit must be admitted too");

        let limiter = &executor.block_limiter;
        contributions.push(Contribution {
            succeeded,
            reported,
            destroyed,
            block_reported: limiter.block_compute_gas_used,
            block_enforced: limiter.block_compute_gas_enforced,
            block_full: limiter.is_block_limit_reached(),
        });
    }

    let (_, receipts) = executor.finish().expect("the block must finish");
    assert_eq!(
        receipts.receipts.len(),
        contributions.len(),
        "every admitted transaction must have produced a receipt"
    );
    contributions
}

/// Asserts the shape both destroyed-remainder tests need from their first transaction: it reported
/// past the block's compute ceiling, but performed far too little to have earned that.
fn assert_reports_past_the_limit_without_performing_it(label: &str, first: &Contribution) {
    assert!(first.destroyed > 0, "{label}: the frame must have destroyed a remainder");
    assert!(
        first.reported > BLOCK_COMPUTE_GAS_LIMIT,
        "{label}: the reported total must exceed the block limit, got {}",
        first.reported,
    );
    assert!(
        first.reported - first.destroyed < BLOCK_COMPUTE_GAS_LIMIT,
        "{label}: the work performed must stay under the block limit, got {}",
        first.reported - first.destroyed,
    );
}

/// An ordinary exceptional frame: a call that halts on its first opcode with a large envelope
/// still in hand.
///
/// The block must report what the transaction reported — destroyed remainder included, which is
/// what makes the reported counter cross the ceiling — and must still admit the cheap transaction
/// behind it, because the enforced counter only ever saw the three gas the `ADD` charged before
/// underflowing.
#[test]
fn test_rex7_destroyed_remainder_reports_at_block_level_without_closing_the_block() {
    let txs = [
        envelope(0, HALTING_TX_GAS_LIMIT, HALTING, Bytes::new()),
        envelope(1, 100_000, CHEAP, Bytes::new()),
    ];
    let block = run_block(MegaSpecId::REX7, BLOCK_COMPUTE_GAS_LIMIT, &txs);

    assert_eq!(block.len(), 2, "the cheap transaction must still fit in the block");
    let [halting, cheap] = [block[0], block[1]];

    assert!(!halting.succeeded, "the first transaction must halt");
    assert_reports_past_the_limit_without_performing_it("ordinary frame", &halting);

    assert_eq!(
        halting.block_reported, halting.reported,
        "the block's reported statistic must carry the destroyed remainder"
    );
    assert_eq!(
        halting.block_enforced,
        halting.reported - halting.destroyed,
        "the block must enforce only the work performed"
    );
    assert!(
        !halting.block_full,
        "a destroyed remainder must not fill the block's compute capacity"
    );

    assert!(cheap.succeeded, "the second transaction must execute normally");
    assert_eq!(
        cheap.block_reported,
        halting.reported + cheap.reported,
        "the reported counter keeps accumulating whole totals"
    );
    assert_eq!(
        cheap.block_enforced,
        halting.block_enforced + cheap.reported,
        "a transaction that destroys nothing advances both counters by the same amount"
    );
}

/// The same shape, one boundary deeper: the frame that halts lives inside the `KeylessDeploy`
/// sandbox, whose usage is merged back into the outer transaction through a separate tracker.
///
/// The outer transaction succeeds — the sandbox reports a failed deployment through the
/// keyless-deploy wire contract rather than failing itself — so nothing about the outer result
/// hints that a remainder was destroyed. If the merge or the outcome dropped the classification,
/// the block would silently enforce a sandbox's destroyed budget against every transaction behind
/// it.
#[test]
fn test_rex7_sandbox_destroyed_remainder_does_not_close_the_block_either() {
    let txs = [
        envelope(0, 30_000_000, KEYLESS_DEPLOY_ADDRESS, keyless_deploy_call_data()),
        envelope(1, 100_000, CHEAP, Bytes::new()),
    ];
    let block = run_block(MegaSpecId::REX7, BLOCK_COMPUTE_GAS_LIMIT, &txs);

    assert_eq!(block.len(), 2, "the cheap transaction must still fit in the block");
    let [sandbox, cheap] = [block[0], block[1]];

    assert!(sandbox.succeeded, "the keyless deploy reports the constructor failure, it is not one");
    assert_reports_past_the_limit_without_performing_it("sandbox frame", &sandbox);

    assert_eq!(
        sandbox.block_reported, sandbox.reported,
        "the block's reported statistic must carry the sandbox's destroyed remainder"
    );
    assert_eq!(
        sandbox.block_enforced,
        sandbox.reported - sandbox.destroyed,
        "the block must enforce only the work the sandbox performed"
    );
    assert!(!sandbox.block_full, "a sandbox's destroyed remainder must not fill the block");
    assert!(cheap.succeeded, "the second transaction must execute normally");
}

/// Executed work still fills the block: the split is a classification, not a way out of the block
/// compute-gas limit.
#[test]
fn test_rex7_executed_work_still_closes_the_block() {
    // A ceiling below what a single halting transaction's `ADD`-plus-intrinsic work costs.
    let txs = [
        envelope(0, HALTING_TX_GAS_LIMIT, HALTING, Bytes::new()),
        envelope(1, 100_000, CHEAP, Bytes::new()),
    ];
    let block = run_block(MegaSpecId::REX7, 1, &txs);

    assert_eq!(block.len(), 1, "the block must close once the enforced counter reaches the limit");
    assert!(block[0].block_full, "the work performed does fill a block this small");
}

/// Rex6 destroys nothing, so the enforced counter must track the reported one exactly — through a
/// block that mixes a successful transaction with one that halts on its first opcode, the shape
/// that diverges under Rex7.
#[test]
fn test_rex6_block_compute_counters_never_diverge() {
    let txs = [
        envelope(0, HALTING_TX_GAS_LIMIT, HALTING, Bytes::new()),
        envelope(1, 100_000, CHEAP, Bytes::new()),
        envelope(2, HALTING_TX_GAS_LIMIT, HALTING, Bytes::new()),
    ];
    let block = run_block(MegaSpecId::REX6, u64::MAX, &txs);

    assert_eq!(block.len(), 3, "no transaction here approaches an unlimited block");
    for (index, contribution) in block.iter().enumerate() {
        assert_eq!(
            contribution.destroyed, 0,
            "tx {index}: no spec before Rex7 destroys compute gas"
        );
        assert_eq!(
            contribution.block_reported, contribution.block_enforced,
            "tx {index}: the two counters must stay identical on a frozen spec"
        );
    }
}
