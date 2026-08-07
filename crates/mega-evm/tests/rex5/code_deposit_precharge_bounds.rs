//! Size boundary of the REX5 code-deposit compute-gas pre-charge.
//!
//! REX5 charges the canonical `code_len * CODEDEPOSIT` compute gas *before*
//! `process_next_action` commits the CREATE checkpoint, and skips it exactly when revm's
//! `return_create` would not charge it. `will_return_create_charge_code_deposit` mirrors
//! `return_create`'s EIP-170 predicate, `output.len() > max_code_size`, and the two must agree at
//! the boundary: a deployment of exactly `max_code_size` bytes is accepted and must be pre-charged;
//! one byte more is rejected and must not be.
//!
//! Under REX5 nothing else records that gas — `after_frame_run` deliberately passes `None` for
//! CREATE results — so a predicate that disagrees with revm shows up directly as a compute-gas
//! total that is short (or inflated) by `code_len * CODEDEPOSIT`.

use mega_evm::MegaTransaction;
use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    alloy_op_evm::OpTxError, constants, op_revm::OpTransaction, test_utils::MemoryDatabase,
    EVMError, EmptyExternalEnv, EvmTxRuntimeLimits, LimitUsage, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId,
};
use revm::{
    context::{result::ResultAndState, BlockEnv, TxEnv},
    handler::EvmTr,
    primitives::TxKind,
};

const CALLER: Address = address!("0000000000000000000000000000000000500300");
/// revm's per-byte code-deposit gas (`revm::interpreter::gas::CODEDEPOSIT`).
const CODEDEPOSIT: u64 = 200;
/// Enough to cover the `10_000`-gas-per-byte code-deposit storage gas on a 512 KiB deployment.
const GAS_LIMIT: u64 = 50_000_000_000;

/// Init code returning `len` zero bytes from untouched memory: `PUSH4 len; PUSH0; RETURN`.
fn init_code(len: u64) -> Bytes {
    let mut code = vec![0x63];
    code.extend_from_slice(&(len as u32).to_be_bytes());
    code.extend_from_slice(&[0x5F, 0xF3]);
    Bytes::from(code)
}

fn deploy(len: u64) -> (ResultAndState<MegaHaltReason>, LimitUsage) {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1u64));
    let mut context = MegaContext::new(db, MegaSpecId::REX5)
        .with_block(BlockEnv { gas_limit: u64::MAX, ..Default::default() })
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let mut tx = MegaTransaction(OpTransaction::new(TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        data: init_code(len),
        gas_limit: GAS_LIMIT,
        gas_price: 0,
        ..Default::default()
    }));
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm: MegaEvm<_, revm::inspector::NoOpInspector, EmptyExternalEnv> =
        MegaEvm::new(context);
    let result: Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, OpTxError>> =
        alloy_evm::Evm::transact_raw(&mut evm, tx);
    let result = result.expect("CREATE tx must execute");
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (result, usage)
}

/// A deployment of exactly `MAX_CONTRACT_SIZE` bytes is inside revm's EIP-170 bound, so REX5 must
/// pre-charge its full code-deposit compute gas.
#[test]
fn test_rex5_precharges_code_deposit_at_max_contract_size() {
    let len = constants::mini_rex::MAX_CONTRACT_SIZE as u64;
    let (result, usage) = deploy(len);

    assert!(
        result.result.is_success(),
        "a deployment of exactly MAX_CONTRACT_SIZE bytes must succeed, got {:?}",
        result.result,
    );
    let code_deposit_gas = len * CODEDEPOSIT;
    assert!(
        usage.compute_gas >= code_deposit_gas,
        "compute gas ({}) must include the {code_deposit_gas} code-deposit charge at the \
         EIP-170 boundary",
        usage.compute_gas,
    );
}

/// One byte over the bound, `return_create` rejects the deployment and charges nothing, so REX5
/// must not pre-charge either — otherwise the transaction is metered for a code deposit that never
/// happened.
#[test]
fn test_rex5_skips_code_deposit_precharge_above_max_contract_size() {
    let len = constants::mini_rex::MAX_CONTRACT_SIZE as u64 + 1;
    let (result, usage) = deploy(len);

    assert!(
        !result.result.is_success(),
        "a deployment one byte over MAX_CONTRACT_SIZE must fail, got {:?}",
        result.result,
    );
    assert!(
        usage.compute_gas < constants::mini_rex::MAX_CONTRACT_SIZE as u64 * CODEDEPOSIT,
        "an over-sized deployment must not be pre-charged code-deposit compute gas; \
         compute_gas was {}",
        usage.compute_gas,
    );
}
