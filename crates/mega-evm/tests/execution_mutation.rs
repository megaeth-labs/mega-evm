//! Focused mutation-killing regression tests for `evm/execution.rs`.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
};
use revm::{
    context::{result::ResultAndState, tx::TxEnvBuilder, ContextTr},
    handler::{FrameResult, ItemOrResult},
    inspector::InspectorEvmTr,
    interpreter::{
        interpreter::SharedMemory, interpreter_action::FrameInit,
        interpreter_types::InterpreterTypes, CallInput, CallInputs, CallOutcome, CallScheme,
        CallValue, FrameInput, Gas, InstructionResult, InterpreterResult,
    },
    primitives::CALL_STACK_LIMIT,
    Inspector,
};

const CALLER: Address = address!("0000000000000000000000000000000000700000");
const CONTRACT: Address = address!("0000000000000000000000000000000000700001");
const INSPECT_TARGET: Address = address!("0000000000000000000000000000000000700002");
const GAS_LIMIT: u64 = 100_000;
const REFUND_SLOTS: u64 = 4;

#[derive(Default)]
struct AlwaysInterceptInspector {
    call_count: usize,
    call_end_count: usize,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for AlwaysInterceptInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.call_count += 1;
        Some(CallOutcome {
            result: InterpreterResult {
                result: InstructionResult::Stop,
                output: Bytes::new(),
                gas: Gas::new(inputs.gas_limit),
            },
            memory_offset: inputs.return_memory_offset.clone(),
        })
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_end_count += 1;
    }
}

fn build_evm(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
) -> MegaEvm<&mut MemoryDatabase, revm::inspector::NoOpInspector, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    MegaEvm::new(context)
}

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: MegaTransaction,
) -> ResultAndState<MegaHaltReason> {
    let mut evm = build_evm(spec, db);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transaction should not error")
}

fn refund_contract_code() -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for slot in 0..REFUND_SLOTS {
        builder = builder.sstore(U256::from(slot), U256::ZERO);
    }
    builder.stop().build()
}

fn make_refund_tx(is_deposit: bool) -> MegaTransaction {
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CONTRACT)
        .gas_limit(200_000)
        .gas_price(0)
        .build_fill();
    let mut tx = MegaTransaction::new(tx);
    if is_deposit {
        tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
    }
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

fn make_call_frame_init(depth: usize) -> FrameInit {
    FrameInit {
        depth,
        memory: SharedMemory::new(),
        frame_input: FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: GAS_LIMIT,
            bytecode_address: INSPECT_TARGET,
            target_address: INSPECT_TARGET,
            caller: CALLER,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
        })),
    }
}

#[test]
fn test_deposit_refund_matches_regular_tx_under_isthmus() {
    let mut normal_db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_u64))
        .account_code(CONTRACT, refund_contract_code());
    for slot in 0..REFUND_SLOTS {
        normal_db = normal_db.account_storage(CONTRACT, U256::from(slot), U256::from(1_u64));
    }
    let normal = transact(MegaSpecId::REX4, &mut normal_db, make_refund_tx(false));
    assert!(normal.result.is_success(), "regular transaction should succeed: {:?}", normal.result);
    let revm::context::result::ExecutionResult::Success {
        gas_refunded: normal_refunded,
        gas_used: normal_used,
        ..
    } = &normal.result
    else {
        panic!("expected success result");
    };

    let mut deposit_db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_u64))
        .account_code(CONTRACT, refund_contract_code());
    for slot in 0..REFUND_SLOTS {
        deposit_db = deposit_db.account_storage(CONTRACT, U256::from(slot), U256::from(1_u64));
    }
    let deposit = transact(MegaSpecId::REX4, &mut deposit_db, make_refund_tx(true));
    assert!(
        deposit.result.is_success(),
        "deposit transaction should succeed: {:?}",
        deposit.result
    );
    let revm::context::result::ExecutionResult::Success {
        gas_refunded: deposit_refunded,
        gas_used: deposit_used,
        ..
    } = &deposit.result
    else {
        panic!("expected success result");
    };
    assert_eq!(
        normal_used, deposit_used,
        "all Mega specs map to OpSpecId::ISTHMUS, so REGOLITH refund rules must apply equally to deposit and regular transactions",
    );
    assert!(
        *normal_refunded > 0,
        "the SSTORE clear path must produce a non-zero refund so the branch stays observable",
    );
    assert!(
        *normal_refunded < REFUND_SLOTS * 4_800,
        "the multi-slot clear must hit the London refund cap; otherwise skipping set_final_refund stays observationally identical",
    );
    assert_eq!(
        normal_refunded, deposit_refunded,
        "under ISTHMUS/REGOLITH, deposit transactions must preserve the same final gas refund as regular transactions",
    );
}

#[test]
fn test_inspect_frame_init_depth_equal_limit_preserves_inspector_result() {
    let mut db = MemoryDatabase::default();
    let evm = build_evm(MegaSpecId::REX5, &mut db);
    let mut evm = evm.with_inspector(AlwaysInterceptInspector::default());

    let result = InspectorEvmTr::inspect_frame_init(
        &mut evm,
        make_call_frame_init(CALL_STACK_LIMIT as usize),
    )
    .expect("inspect_frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected Call result");
    };

    assert_eq!(
        outcome.result.result,
        InstructionResult::Stop,
        "depth == CALL_STACK_LIMIT is still permitted; the inspector result must survive",
    );
    let inspector = evm.inspector();
    assert_eq!(inspector.call_count, 1);
    assert_eq!(inspector.call_end_count, 1);
}

#[test]
fn test_inspect_frame_init_low_depth_preserves_inspector_result() {
    let mut db = MemoryDatabase::default();
    let evm = build_evm(MegaSpecId::REX5, &mut db);
    let mut evm = evm.with_inspector(AlwaysInterceptInspector::default());

    let result = InspectorEvmTr::inspect_frame_init(&mut evm, make_call_frame_init(0))
        .expect("inspect_frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected Call result");
    };

    assert_eq!(
        outcome.result.result,
        InstructionResult::Stop,
        "the depth guard must not fire below CALL_STACK_LIMIT",
    );
    let inspector = evm.inspector();
    assert_eq!(inspector.call_count, 1);
    assert_eq!(inspector.call_end_count, 1);
}
