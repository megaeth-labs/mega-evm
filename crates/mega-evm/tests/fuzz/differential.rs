//! The neutral differential: under the neutral configuration, `MegaEvm` must agree exactly with
//! the fork's op-revm on the same configuration, for priced transactions, and with revm's mainnet
//! EVM on the fork's spec, for unpriced ones, on random programs.
//!
//! The cases are the generators' in the neutral flavor: what only `MegaETH` has — the system
//! contracts and their interceptors, the Oracle's answers, the `SLOTNUM` opcode on the Osaka base
//! (a registered deviation of the execution-spec gate), keyless deployments and system-address
//! transactions — is replaced by its plain counterpart, because the references cannot express it.
//! The SALT environment is left out too, so every bucket is minimal, as the gate's runner has it.
//! Everything else is drawn from the same space as the properties.
//!
//! One thing more is taken out, for one arm on one fork. A contract destroyed in the transaction
//! that created it is settled on Amsterdam by EIP-8246, which Satin's Osaka base does not have:
//! the gate's other registered deviation. op-revm shares that base, and Ethereum on Osaka has the
//! same rule, so both are compared on the case as drawn. Ethereum on Amsterdam is compared on the
//! case with every self-destruction inside init code taken out
//! (`Case::without_destruction_in_creation`): the ending of a creation transaction's own init
//! code, of the init code a `CREATE` or `CREATE2` runs, and of the code either borrows through
//! `CALLCODE` or `DELEGATECALL`, which runs as the account being created. No case is skipped for
//! it, and the test prints how many it rewrote.
//!
//! op-revm's arm takes every transaction but a deposit from an account that does not exist yet:
//! Satin charges the account such a deposit creates for its caller, op-revm does not, and the
//! op-revm baseline (`tests/satin/equivalence.rs`) pins that divergence; under Amsterdam's
//! configuration, where state gas is on, the charge is there too. Ethereum has no deposits and
//! routes fees differently, so its arm takes the other transactions at a gas price of zero in a
//! block with no base fee, and op-revm's three fee vaults, touched empty, are taken out of
//! `MegaEvm`'s state before the two are compared, as `tests/satin/neutral.rs` does.

use std::sync::atomic::{AtomicU32, Ordering};

use alloy_op_evm::OpTx;
use mega_evm::{
    test_utils::{
        neutral_cfg, neutral_gas_table, neutralize_evm, zero_fee_l1_block_info, MemoryDatabase,
    },
    EthSpecId, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
};
use op_revm::{
    constants::{BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT},
    L1BlockInfo, OpEvm, OpHaltReason, OpSpecId, OpTransaction,
};
use revm::{
    context::{
        result::{ExecResultAndState, ExecutionResult},
        BlockEnv, CfgEnv, Context, ContextTr, TxEnv,
    },
    handler::EvmTr,
    inspector::NoOpInspector,
    state::EvmState,
    ExecuteEvm, Journal, MainBuilder, MainContext,
};

use crate::{
    gen::{
        case::{case, Case},
        Flavor,
    },
    harness::{check, prop_eq},
    render::render_state,
};

type Outcome = ExecResultAndState<ExecutionResult<MegaHaltReason>, EvmState>;
type OpContext = Context<
    BlockEnv,
    OpTransaction<TxEnv>,
    CfgEnv<OpSpecId>,
    MemoryDatabase,
    Journal<MemoryDatabase>,
    L1BlockInfo,
>;

const FORKS: [EthSpecId; 2] = [EthSpecId::OSAKA, EthSpecId::AMSTERDAM];

/// The number of cases each differential runs in the bounded mode, each on both forks.
const CASES: u32 = 512;

/// An outcome or a refusal, rendered so two engines' compare: the result with its gas, logs and
/// output, and the state sorted.
fn render(outcome: &Result<Outcome, String>) -> String {
    match outcome {
        Ok(outcome) => {
            format!("result: {:?}\nstate:\n{}", outcome.result, render_state(&outcome.state))
        }
        Err(error) => format!("refused: {error}\n"),
    }
}

/// Runs `case` through a neutral `MegaEvm` for `fork`, in `block`, without a SALT environment,
/// and returns the outcome and the configuration it ran on.
fn run_mega(
    fork: EthSpecId,
    case: &Case,
    block: BlockEnv,
    tx: OpTransaction<TxEnv>,
) -> (Result<Outcome, String>, CfgEnv<OpSpecId>) {
    let ctx = MegaContext::new(case.database(), MegaSpecId::SATIN)
        .with_neutral_cfg(neutral_cfg(fork).expect("a neutral fork"))
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits())
        .with_block(block)
        .with_chain(zero_fee_l1_block_info());
    let cfg = ctx.cfg().clone();
    let mut mega = MegaEvm::new(ctx);
    neutralize_evm(&mut mega, fork).expect("a neutral fork");
    let outcome = mega
        .execute_transaction(OpTx(tx))
        .map(|outcome| {
            ExecResultAndState::new(outcome.result_and_state.result, outcome.result_and_state.state)
        })
        .map_err(|error| format!("{error}"));
    (outcome, cfg)
}

/// Runs `tx` through op-revm's `OpEvm` on `cfg`, the configuration the neutral `MegaEvm` ran on,
/// with `fork`'s static opcode prices installed: op-revm's Karst base carries Osaka's, and
/// Amsterdam raises `EXTCODESIZE`'s and `EXTCODECOPY`'s, which the neutral `MegaEvm` carries as
/// Ethereum does.
fn run_op(
    case: &Case,
    fork: EthSpecId,
    cfg: CfgEnv<OpSpecId>,
    block: BlockEnv,
    tx: OpTransaction<TxEnv>,
) -> Result<Outcome, String> {
    let op_ctx = OpContext::new(case.database(), OpSpecId::KARST)
        .with_cfg(cfg)
        .with_block(block)
        .with_chain(zero_fee_l1_block_info());
    let mut op = OpEvm::new(op_ctx, NoOpInspector);
    let (_, instructions, _, _) = op.0.all_mut();
    *instructions.gas_table_mut() = neutral_gas_table(fork).expect("a neutral fork");
    op.transact(tx).map_err(|error| format!("{error}"))
}

/// Runs `tx` through revm's mainnet EVM on `fork`'s spec.
fn run_eth(case: &Case, fork: EthSpecId, block: BlockEnv, tx: TxEnv) -> Result<Outcome, String> {
    let mut cfg = CfgEnv::new();
    cfg.set_spec_and_mainnet_gas_params(fork);
    Context::mainnet()
        .with_db(case.database())
        .with_cfg(cfg)
        .with_block(block)
        .build_mainnet()
        .transact(tx)
        .map(|outcome| {
            ExecResultAndState::new(
                outcome.result.map_haltreason(OpHaltReason::Base),
                outcome.state,
            )
        })
        .map_err(|error| format!("{error}"))
}

/// Under the neutral configuration of each fork, `MegaEvm` and op-revm produce byte-identical
/// outcomes — the result, its gas, logs and output, and the state — or the same refusal, for
/// every transaction but a deposit that creates its caller's account.
#[test]
fn test_differential_neutral_mega_evm_matches_op_revm() {
    check(
        "differential_op_revm",
        CASES,
        || case(Flavor::Neutral),
        |case| {
            let caller = &case.world.caller;
            let caller_is_empty = caller.nonce == 0 &&
                caller.balance == crate::gen::world::Balance::Zero &&
                case.world.delegation.is_none_or(|d| d.delegator != crate::gen::Who::Caller);
            if case.tx.is_deposit() && caller_is_empty {
                return Ok(());
            }
            for fork in FORKS {
                let block = case.block();
                let tx = case.transaction().0;
                let (mega, cfg) = run_mega(fork, case, block.clone(), tx.clone());
                let op = run_op(case, fork, cfg, block, tx);
                prop_eq!(render(&mega), render(&op), "{fork:?}: MegaEvm and op-revm differ");
            }
            Ok(())
        },
    );
}

/// Under the neutral configuration of each fork, an unpriced transaction that is not a deposit
/// produces on `MegaEvm` what revm's mainnet EVM produces on the fork, but for the three fee vaults
/// op-revm touches. On Amsterdam the case runs without the self-destructions its init codes held,
/// which EIP-8246 settles there and Satin's Osaka base does not have; on Osaka it runs as drawn.
#[test]
fn test_differential_neutral_mega_evm_matches_ethereum() {
    let compared = AtomicU32::new(0);
    let rewritten = AtomicU32::new(0);
    check(
        "differential_ethereum",
        CASES,
        || case(Flavor::Neutral),
        |case| {
            if case.tx.is_deposit() {
                return Ok(());
            }
            let on_amsterdam = case.without_destruction_in_creation();
            compared.fetch_add(1, Ordering::Relaxed);
            if on_amsterdam != *case {
                rewritten.fetch_add(1, Ordering::Relaxed);
            }
            for fork in FORKS {
                let case = if fork == EthSpecId::AMSTERDAM { &on_amsterdam } else { case };
                let block = BlockEnv { basefee: 0, ..case.block() };
                let mut tx = case.transaction().0;
                tx.base.gas_price = 0;
                tx.base.gas_priority_fee = tx.base.gas_priority_fee.map(|_| 0);
                let (mega, _) = run_mega(fork, case, block.clone(), tx.clone());
                let mega = mega.map(|mut outcome| {
                    for vault in [BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT] {
                        if outcome.state.get(&vault).is_some_and(|account| account.is_empty()) {
                            outcome.state.remove(&vault);
                        }
                    }
                    outcome
                });
                let eth = run_eth(case, fork, block, tx.base);
                prop_eq!(render(&mega), render(&eth), "{fork:?}: MegaEvm and Ethereum differ");
            }
            Ok(())
        },
    );
    println!(
        "differential_ethereum: {} cases compared on both forks, {} of them on Amsterdam without \
         the self-destruction their init code held",
        compared.load(Ordering::Relaxed),
        rewritten.load(Ordering::Relaxed)
    );
}
