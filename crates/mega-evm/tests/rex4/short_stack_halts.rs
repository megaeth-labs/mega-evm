//! A storage-writing opcode whose stack is short halts and consumes the frame's gas.
//!
//! The `SSTORE` and `LOG` wrappers read their operands before running the opcode, so a short
//! stack stops them first. That halt is an ordinary exceptional halt: the frame fails and keeps
//! none of its gas, on every spec. Each case runs the opcode in a child frame at two budgets and
//! checks that the transaction's gas grows by exactly the difference. Every case also passes on
//! the deployed implementation.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, BlockEnv},
    database::AccountState,
    state::Bytecode,
};

const SENDER: Address = address!("0000000000000000000000000000000000480000");
const OUTER: Address = address!("0000000000000000000000000000000000480001");
const INNER: Address = address!("0000000000000000000000000000000000480002");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000480099");

const SPECS: [MegaSpecId; 8] = [
    MegaSpecId::MINI_REX,
    MegaSpecId::REX,
    MegaSpecId::REX1,
    MegaSpecId::REX2,
    MegaSpecId::REX3,
    MegaSpecId::REX4,
    MegaSpecId::REX5,
    MegaSpecId::REX6,
];

fn install(db: &mut MemoryDatabase, address: Address, code: Bytes) {
    let code = Bytecode::new_raw(code);
    let code_hash = code.hash_slow();
    let account = db.load_account(address).expect("in-memory account load");
    account.info.code = Some(code);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
}

/// The transaction's gas when `OUTER` calls `INNER` (running `inner`) with `budget`, and the
/// child's success flag, which `OUTER` stores at slot 0.
fn run(spec: MegaSpecId, inner: &Bytes, budget: u64) -> (u64, U256) {
    let mut db = MemoryDatabase::default()
        .account_balance(SENDER, U256::from(10).pow(U256::from(30)))
        .account_balance(BENEFICIARY, U256::from(1));
    install(&mut db, INNER, inner.clone());
    let outer = BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(INNER)
        .push_number(budget)
        .append(CALL)
        .push_number(0_u64)
        .append(SSTORE)
        .stop()
        .build();
    install(&mut db, OUTER, outer);

    let mut context = MegaContext::new(&mut db, spec)
        .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(SENDER).call(OUTER).gas_limit(5_000_000).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx must execute");
    assert!(outcome.result.is_success(), "{spec:?}: {:?}", outcome.result);
    let flag = outcome
        .state
        .get(&OUTER)
        .and_then(|account| account.storage.get(&U256::ZERO))
        .map_or(U256::ZERO, |slot| slot.present_value);
    #[allow(deprecated)]
    (outcome.result.gas_used(), flag)
}

/// `pushes` operands followed by `opcode`.
fn short(opcode: u8, pushes: usize) -> Bytes {
    let mut b = BytecodeBuilder::default();
    for _ in 0..pushes {
        b = b.push_number(1_u64);
    }
    b.append(opcode).stop().build()
}

#[test]
fn test_short_stack_storage_opcodes_halt_and_consume_the_frame() {
    let mut cases = vec![(SSTORE, 0), (SSTORE, 1)];
    for n in 0..=4u8 {
        // `LOGn` takes 2 + n operands: none, one, and one short.
        cases.extend([(LOG0 + n, 0), (LOG0 + n, 1), (LOG0 + n, 1 + n as usize)]);
    }
    for spec in SPECS {
        for &(opcode, pushes) in &cases {
            let inner = short(opcode, pushes);
            let (low, low_flag) = run(spec, &inner, 50_000);
            let (high, high_flag) = run(spec, &inner, 60_000);
            let what = format!("{spec:?}: opcode 0x{opcode:02x} after {pushes} operand(s)");
            assert_eq!(low_flag, U256::ZERO, "{what}: the child must fail");
            assert_eq!(high_flag, U256::ZERO, "{what}: the child must fail");
            assert_eq!(high - low, 10_000, "{what}: the child must consume its whole budget");
        }
    }
}
