//! SSTORE storage-gas pricing error path.
//!
//! The dynamically-scaled storage gas for a first-time non-zero write is charged inline
//! in `additional_limit_ext::sstore`, before the inner SSTORE executes. When the SALT
//! environment fails to price the slot (`sstore_set_storage_gas` returns `None`), the
//! instruction must halt with `FatalExternalError`, which surfaces as `EVMError::Custom`.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BucketId, EVMError, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvs, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionError, SaltEnv,
};
use revm::{
    bytecode::opcode::{SSTORE, STOP},
    context::{result::ResultAndState, TxEnv},
};

const CALLER: Address = address!("2000000000000000000000000000000000000011");
const CONTRACT: Address = address!("1000000000000000000000000000000000000011");

/// A SALT environment that always fails `get_bucket_capacity`, triggering the
/// `sstore_set_storage_gas` → `None` → `FatalExternalError` path in the SSTORE wrapper.
#[derive(Debug)]
struct FailingSaltEnv;

impl SaltEnv for FailingSaltEnv {
    type Error = String;

    fn get_bucket_capacity(&self, _bucket_id: BucketId) -> Result<u64, String> {
        Err("injected salt error".into())
    }

    fn bucket_id_for_account(_account: Address) -> BucketId {
        0
    }

    fn bucket_id_for_slot(_address: Address, _key: U256) -> BucketId {
        0
    }
}

/// SSTORE(key=0, value=1): a first-time non-zero write to an originally-zero slot,
/// the only shape that triggers the dynamic storage-gas charge.
fn sstore_fresh_slot_bytecode() -> Bytes {
    BytecodeBuilder::default()
        .push_number(1u64) // value
        .push_number(0u64) // key (top of stack)
        .append(SSTORE)
        .append(STOP)
        .build()
}

fn transact_with_failing_salt(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let envs: ExternalEnvs<(FailingSaltEnv, EmptyExternalEnv)> =
        ExternalEnvs { salt_env: FailingSaltEnv, oracle_env: EmptyExternalEnv };
    let mut context = MegaContext::new(db, spec)
        .with_external_envs(envs)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        data: Bytes::new(),
        value: U256::ZERO,
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// A failing SALT pricing on a first-time SSTORE write must halt the instruction with
/// `FatalExternalError` before the inner SSTORE executes, surfacing as `EVMError::Custom`.
#[test]
fn test_rex5_sstore_salt_error_halts_with_fatal_external_error() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(CONTRACT, sstore_fresh_slot_bytecode());

    let result = transact_with_failing_salt(MegaSpecId::REX5, &mut db);

    match result {
        Err(EVMError::Custom(msg)) => {
            assert!(
                msg.contains("injected salt error"),
                "error message should contain salt error, got: {msg}"
            );
        }
        Err(other) => panic!("expected EVMError::Custom, got: {other:?}"),
        Ok(result) => panic!("expected error, got success: {:?}", result.result),
    }
}
