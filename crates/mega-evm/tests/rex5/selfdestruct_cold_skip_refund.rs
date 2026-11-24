//! A `SELFDESTRUCT` too poor for its beneficiary's cold read still refunds the destroyed account.
//!
//! From Rex4 a `SELFDESTRUCT` of an account created in the same transaction refunds that
//! account's state growth, and from Rex5 one that sends value to an empty beneficiary records the
//! new account before the opcode runs. The deployed implementation read the beneficiary and
//! destroyed the account before charging for either, so a frame that could not afford the cold
//! read still recorded the refund before it ran out of gas; revm 40 skips the read and halts
//! first. The halt discards the refund with the rest of the frame's usage, but the frame-end limit
//! check sees it: without it, the new beneficiary puts a frame with a tight state-growth budget
//! over, and the out-of-gas halt becomes a revert that returns the frame's gas to its caller.
//! Every expectation here was measured on the deployed implementation.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, BlockEnv},
    database::AccountState,
    state::Bytecode,
};

const SENDER: Address = address!("0000000000000000000000000000000000440000");
const OUTER: Address = address!("0000000000000000000000000000000000440001");
const CREATOR: Address = address!("0000000000000000000000000000000000440002");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000440099");
/// An empty account: sending value to it creates it.
const EMPTY: Address = address!("00000000000000000000000000000000004400ee");

/// A transaction-wide state-growth limit that leaves the frame under test a budget one short of
/// the new beneficiary.
const STATE_GROWTH_LIMIT: u64 = 3;

/// `SELFDESTRUCT(EMPTY)`.
fn selfdestruct_to_empty() -> Vec<u8> {
    BytecodeBuilder::default().push_address(EMPTY).append(SELFDESTRUCT).build_vec()
}

/// Writes `bytes` to memory from offset 0, then `CREATE(value, 0, len)`.
fn create(mut b: BytecodeBuilder, bytes: &[u8], value: u64) -> BytecodeBuilder {
    for (i, chunk) in bytes.chunks(32).enumerate() {
        let mut word = [0u8; 32];
        word[..chunk.len()].copy_from_slice(chunk);
        b = b.push_bytes(word).push_number(32 * i as u64).append(MSTORE);
    }
    b.push_number(bytes.len() as u64).push_number(0_u64).push_number(value).append(CREATE)
}

/// `CALL(gas, target, 0, 0, 0, 0, 0)`, with `target` pushed by `push_target`.
fn call(
    b: BytecodeBuilder,
    gas: u64,
    push_target: impl FnOnce(BytecodeBuilder) -> BytecodeBuilder,
) -> BytecodeBuilder {
    let b = b
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64);
    push_target(b).push_number(gas).append(CALL).append(POP)
}

fn install(db: &mut MemoryDatabase, address: Address, code: Bytes, balance: u64) {
    let code = Bytecode::new_raw(code);
    let code_hash = code.hash_slow();
    let account = db.load_account(address).expect("in-memory account load");
    account.info.code = Some(code);
    account.info.code_hash = code_hash;
    account.info.balance = U256::from(balance);
    account.account_state = AccountState::None;
}

/// Runs `OUTER` with `CREATOR` installed and returns the transaction's `gas_used`.
fn gas_used(spec: MegaSpecId, outer: Bytes, creator: Bytes) -> u64 {
    let mut db = MemoryDatabase::default()
        .account_balance(SENDER, U256::from(10).pow(U256::from(30)))
        .account_balance(BENEFICIARY, U256::from(1));
    install(&mut db, OUTER, outer, 1_000);
    install(&mut db, CREATOR, creator, 1_000);
    let mut limits = EvmTxRuntimeLimits::from_spec(spec);
    limits.tx_state_growth_limit = STATE_GROWTH_LIMIT;
    let mut context = MegaContext::new(&mut db, spec)
        .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() })
        .with_tx_runtime_limits(limits);
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
    #[allow(deprecated)]
    outcome.result.gas_used()
}

/// A contract's initcode `SELFDESTRUCT`s with the endowment it was created with. `OUTER` hands
/// `CREATOR` `budget`, which leaves the initcode short of the beneficiary's cold read.
#[test]
fn test_initcode_selfdestruct_below_its_cold_read_stays_out_of_gas() {
    let creator =
        create(BytecodeBuilder::default(), &selfdestruct_to_empty(), 1).append(POP).stop().build();
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        // The last budget affords the cold read: a control that never skipped it.
        for (budget, expected) in
            [(32_050, 94_673), (33_000, 95_606), (34_675, 97_247), (34_700, 97_272)]
        {
            let outer = call(BytecodeBuilder::default(), budget, |b| b.push_address(CREATOR))
                .stop()
                .build();
            assert_eq!(
                gas_used(spec, outer, creator.clone()),
                expected,
                "{spec:?}: CREATOR given {budget}",
            );
        }
    }
}

/// A contract created earlier in the transaction, with one storage slot, `SELFDESTRUCT`s when
/// called with `budget`, too little for the beneficiary's cold read.
#[test]
fn test_same_transaction_contract_below_its_cold_read_stays_out_of_gas() {
    // Initcode: `SSTORE(0, 1)`, then return the runtime `SELFDESTRUCT(EMPTY)`.
    let runtime = selfdestruct_to_empty();
    let len = runtime.len() as u8;
    let initcode = BytecodeBuilder::default()
        .push_number(1_u8)
        .push_number(0_u8)
        .append(SSTORE)
        .push_bytes(&runtime)
        .push_number(0_u8)
        .append(MSTORE)
        .push_number(len)
        .push_number(32 - len)
        .append(RETURN)
        .build_vec();
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        // The last budget affords the cold read: a control that never skipped it.
        for (budget, expected) in
            [(100, 338_786), (2_000, 340_686), (2_602, 341_288), (2_700, 341_386)]
        {
            // `OUTER`: create the contract with an endowment, then call it with `budget`.
            let outer = create(BytecodeBuilder::default(), &initcode, 1);
            let outer = call(outer, budget, |b| b.append(DUP6)).append(POP).stop().build();
            assert_eq!(
                gas_used(spec, outer, Bytes::new()),
                expected,
                "{spec:?}: the contract called with {budget}",
            );
        }
    }
}
