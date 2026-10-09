//! REX5+ negative control: code-deposit pre-charge stays ahead of CREATE commit.
//!
//! Under REX5 the same CREATE shape (NUMBER → LOG0 → large RETURN) charges
//! `code_len * CODEDEPOSIT` compute gas in `after_frame_run_instructions` **before**
//! `process_next_action` / `return_create`. An exceed therefore rewrites the interpreter
//! result to OOG while the CREATE checkpoint is still open, so `return_create` reverts it
//! and the constructor logs never reach the journal's committed region.
//!
//! This test is expected to pass **with and without** the `execution_result` log strip —
//! it pins the pre-charge ordering invariant that is why REX5+ never produced the halt-logs
//! leak on mainnet, not the strip itself.
//!
//! Spec: `MegaSpecId::REX5`. Path: REX5 pre-charge in `after_frame_run_instructions`
//! (`execution.rs`) before CREATE checkpoint commit.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EmptyExternalEnv, EvmTxRuntimeLimits, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::{LOG0, NUMBER, POP, PUSH0, RETURN},
    context::{result::ResultAndState, BlockEnv, TxEnv},
    primitives::{TxKind, KECCAK_EMPTY},
};

const CALLER: Address = address!("0000000000000000000000000000000000500500");
/// Relative detention cap after `NUMBER` under REX5 (post-access allowance).
const DETENTION_CAP: u64 = 100_000;
/// Runtime code length: deposit compute gas exceeds the relative detention window.
const CODE_LEN: u64 = 1_000;
const GAS_LIMIT: u64 = 50_000_000;

/// Same shape as the REX leak test: detain via `NUMBER`, emit `LOG0`, return large runtime code.
fn init_code() -> Bytes {
    let mut code = vec![NUMBER, POP, PUSH0, PUSH0, LOG0, 0x63];
    code.extend_from_slice(&(CODE_LEN as u32).to_be_bytes());
    code.extend_from_slice(&[PUSH0, RETURN]);
    Bytes::from(code)
}

fn deploy() -> ResultAndState<MegaHaltReason> {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1u64));
    let mut context = MegaContext::new(db, MegaSpecId::REX5)
        .with_block(BlockEnv { gas_limit: u64::MAX, ..Default::default() })
        .with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(DETENTION_CAP),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let mut tx = MegaTransaction::new(TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        data: init_code(),
        gas_limit: GAS_LIMIT,
        gas_price: 0,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm: MegaEvm<_, revm::inspector::NoOpInspector, EmptyExternalEnv> =
        MegaEvm::new(context);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("CREATE tx must execute")
}

/// REX5 pre-charges deposit before commit: halt leaves no logs and no deployed account.
///
/// Passes both with and without the non-Success log strip — protects pre-charge ordering only.
#[test]
fn test_rex5_precharge_create_halt_has_empty_logs_and_reverted_state() {
    let result = deploy();

    assert!(result.result.is_halt(), "REX5 pre-charge exceed must Halt, got {:?}", result.result,);
    assert!(
        result.result.logs().is_empty(),
        "pre-charge OOG reverts the CREATE checkpoint, so logs must already be empty; got {} \
         log(s)",
        result.result.logs().len(),
    );

    let created = CALLER.create(0);
    let still_deployed = result.state.get(&created).is_some_and(|acc| {
        acc.info.code.as_ref().is_some_and(|c| !c.is_empty()) || acc.info.code_hash != KECCAK_EMPTY
    });
    assert!(
        !still_deployed,
        "REX5 pre-charge must run before commit so return_create reverts the checkpoint; \
         found committed code at {created}",
    );
}
