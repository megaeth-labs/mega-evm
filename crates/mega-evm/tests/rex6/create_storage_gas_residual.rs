//! Pins `create_rex6` storage-gas residual arithmetic (`create_contract_storage_gas - drained`).

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    alloy_op_evm::OpTxError,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _, SaltEnv, TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{
    bytecode::opcode::{CREATE, STOP},
    context::{
        result::{EVMError, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
};

const CALLER: Address = address!("0000000000000000000000000000000000200000");
const SENDER: Address = address!("0000000000000000000000000000000000200001");
const RECEIVER: Address = address!("0000000000000000000000000000000000200002");

fn transact(
    db: &mut MemoryDatabase,
    external_envs: &TestExternalEnvs<Infallible>,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, OpTxError>> {
    let mut context = MegaContext::new(db, MegaSpecId::REX6)
        .with_external_envs(external_envs.into())
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// Receiver that runs CREATE(value=0, offset=0, size=0) — empty initcode deploy.
fn build_create_receiver() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // size
        .push_number(0_u64) // offset
        .push_number(0_u64) // value
        .append(CREATE)
        .append(STOP)
        .build()
}

/// Parent CALL with value so the child frame receives `STORAGE_CALL_STIPEND`.
fn build_value_call(to: Address, gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(1_u64) // value
        .push_address(to)
        .push_number(gas)
        .append(revm::bytecode::opcode::CALL)
        .append(STOP)
        .build()
}

/// CREATE under REX6 inside a stipend-receiving frame must compute
/// `storage_charged = create_contract_storage_gas - drained`. With a non-min
/// bucket multiplier the base is non-zero and the residual is visible in gas.
#[test]
fn test_create_rex6_storage_gas_residual_subtracts_drained_stipend() {
    const MULTIPLIER: u64 = 10;
    const CREATE_BASE: u64 = 32_000;
    const STORAGE_CALL_STIPEND: u64 = 23_000;

    let run = |multiplier: u64| -> u64 {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10_000_000_000_000u128))
            .account_balance(SENDER, U256::from(10_000_000_000u128))
            .account_code(SENDER, build_value_call(RECEIVER, 1_000_000))
            .account_code(RECEIVER, build_create_receiver())
            .account_balance(RECEIVER, U256::from(1_000_000u64));

        // CREATE address depends on RECEIVER's nonce; use a broad high-multiplier
        // for all account buckets by setting capacity on a known target is hard.
        // Instead, set a high default via many bucket ids is impractical.
        // RECEIVER is the creator; created address = RECEIVER.create(nonce).
        // Price against the creator's account bucket first — CREATE storage gas
        // uses create_contract_storage_gas(created_address).
        // Precompute created address for nonce=1 (receiver starts at nonce 1 with code).
        // Safer approach: set capacity for many sequential addresses.
        // Raise capacity for every plausible CREATE destination (nonce 0..16).
        let mut envs = TestExternalEnvs::<Infallible>::new();
        for nonce in 0u64..16 {
            let created = RECEIVER.create(nonce);
            let bucket = TestExternalEnvs::<Infallible>::bucket_id_for_account(created);
            envs = envs.with_bucket_capacity(bucket, MIN_BUCKET_SIZE as u64 * multiplier);
        }

        let tx =
            TxEnvBuilder::default().caller(CALLER).call(SENDER).gas_limit(100_000_000).build_fill();
        let result = transact(&mut db, &envs, tx).expect("tx must not error");
        assert!(result.result.is_success(), "execution must succeed: {:?}", result.result);
        result.result.tx_gas_used()
    };

    let gas1 = run(1);
    let gas10 = run(MULTIPLIER);
    let base = CREATE_BASE * (MULTIPLIER - 1);
    let drained = STORAGE_CALL_STIPEND.min(base);
    let expected_extra = base - drained;
    assert_eq!(
        gas10 - gas1,
        expected_extra,
        "create_rex6 residual must be base-drained; gas1={gas1} gas10={gas10}",
    );
}
