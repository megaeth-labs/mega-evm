//! The tracer `replay --trace` declares read-only has to survive a contract creation.
//!
//! `replay` is the one command that hands the EVM a declared observer, because it is the one whose
//! transaction the canonical block path would admit. A declaration is checked in debug builds: the
//! shim measures the tracer anyway and asserts it booked nothing, so a tracer that writes anything
//! back panics at the callback that did it.
//!
//! `TracingInspector` asks each creation for the address it will occupy, which fills a memo on the
//! inputs, and the shim used to read a filled memo as a rewritten input — so this panicked at the
//! first `CREATE` in a replayed transaction. The offline replay fixtures deploy nothing, so the
//! shape reached no gate at all. This is that gate, over the object the command actually builds:
//! an RPC capture of a deploying transaction would be the other way to write it, and the tracer's
//! declaration is what both would be checking.

use clap::Parser;
use mega_evm::{
    revm::{
        bytecode::opcode::{CREATE, CREATE2, MSTORE, POP, STOP},
        context::tx::TxEnv,
        primitives::{address, Address, Bytes, TxKind, U256},
    },
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew,
};
use mega_evme::TraceArgs;

/// The account the transaction is sent from.
const CALLER: Address = address!("0000000000000000000000000000000000300000");

/// The account holding the deploying code.
const CONTRACT: Address = address!("0000000000000000000000000000000000300001");

/// `PUSH1 0 PUSH1 0 RETURN` — init code that deploys an empty contract.
const RETURN_EMPTY: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xf3];

/// Writes `code` into memory from offset zero, one 32-byte word at a time.
fn write_to_memory(builder: BytecodeBuilder, code: &[u8]) -> BytecodeBuilder {
    let mut builder = builder;
    for (index, chunk) in code.chunks(32).enumerate() {
        let mut word = [0u8; 32];
        word[..chunk.len()].copy_from_slice(chunk);
        builder = builder.push_bytes(word).push_number((index * 32) as u64).append(MSTORE);
    }
    builder
}

/// Init code that creates a contract of its own before returning.
fn nested_init_code() -> Vec<u8> {
    write_to_memory(BytecodeBuilder::default(), &RETURN_EMPTY)
        .push_number(RETURN_EMPTY.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .append(POP)
        .append_many(RETURN_EMPTY)
        .build_vec()
}

/// A `CREATE` and a `CREATE2` of init code that creates once more: four creations, both schemes,
/// two depths.
fn deploying_code() -> Bytes {
    let init = nested_init_code();
    let size = init.len() as u64;
    write_to_memory(BytecodeBuilder::default(), &init)
        .push_number(size)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .append(POP)
        .push_number(0x5A17u64)
        .push_number(size)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE2)
        .append(POP)
        .append(STOP)
        .build()
}

/// ★ The declared tracer runs a deploying transaction without writing anything back.
///
/// In a debug build — which is how this suite runs — the assertion inside the shim is what fails
/// if the declaration stops holding, and it names the callback. In a release build the run simply
/// has to succeed.
#[test]
fn test_the_declared_tracer_survives_a_transaction_that_deploys() {
    let mut db = MemoryDatabase::default()
        .account_code(CONTRACT, deploying_code())
        .account_balance(CALLER, U256::from(1u64) << 64);

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX7);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let mut inspector = TraceArgs::parse_from(["mega-evme", "--trace"]).create_trusted_inspector();
    let mut evm = MegaEvm::new(context).with_trusted_inspector(&mut inspector);

    let mut tx = MegaTransaction::new(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit: 10_000_000,
        gas_price: 0,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());

    let outcome = evm.execute_transaction(tx).expect("the replayed transaction must execute");
    assert!(
        outcome.result_and_state.result.is_success(),
        "the fixture must deploy, got {:?}",
        outcome.result_and_state.result,
    );
    assert_eq!(
        outcome.result_and_state.state.values().filter(|account| account.is_created()).count(),
        4,
        "the fixture must create four contracts, or it is not exercising the shape",
    );
    assert!(
        outcome.inspector_ledger.is_zero(),
        "the tracer the command declares read-only must book nothing: {:?}",
        outcome.inspector_ledger,
    );
}
