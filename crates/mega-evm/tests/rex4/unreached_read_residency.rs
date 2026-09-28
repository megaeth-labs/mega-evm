//! Journal residency left by an account read revm 40 skips, observed by a later CALL.
//!
//! The deployed implementation read a CALL-family target (and its EIP-7702 delegate) and an
//! `EXTCODECOPY` target before charging, so a frame that ran out of gas on those charges had
//! already read them. A read of an absent pre-warmed address (a precompile, the coinbase) left a
//! warm journal entry behind once the frame reverted. revm 40 charges first, so the same frame
//! never reads, and a later CALL's storage-gas wrapper then inspects the address into a cold entry
//! the deployed implementation never had. Every expectation here was measured on the deployed
//! implementation.
//!
//! Each case is a `probe(COLD) - probe(target)` difference measured through a CALL in `OUTER` after
//! `INNER` halts: [`LEFT_WARM`] when `INNER` left the target warm, [`LEFT_COLD`] otherwise.
//! [`EXTCODESIZE`]-based probes cannot see this: they never go through the CALL wrapper's
//! inspection, and take the pre-warmed fresh-entry path either way.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, BlockEnv},
    database::AccountState,
    state::Bytecode,
};

const CALLER: Address = address!("0000000000000000000000000000000000430000");
const OUTER: Address = address!("0000000000000000000000000000000000430001");
const INNER: Address = address!("0000000000000000000000000000000000430002");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000430099");
/// A pre-warmed precompile.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");
/// An EIP-7702 delegator whose delegate is [`IDENTITY`].
const DELEGATOR: Address = address!("00000000000000000000000000000000004300aa");
/// A funded account that is not pre-warmed: the cold reference, and a control target.
const PLAIN: Address = address!("00000000000000000000000000000000004300ff");
/// A second account that is not pre-warmed, touched only by the reference probe.
const COLD: Address = address!("00000000000000000000000000000000004300ee");

const LEFT_COLD: u64 = 0;
const LEFT_WARM: u64 = 2_500;

const WRAPPED_CALL_SPECS: [MegaSpecId; 3] = [MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

/// `INNER`: `EXTCODECOPY(target, 0, 0, len)`.
fn inner_extcodecopy(target: Address, len: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(len)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(target)
        .append(EXTCODECOPY)
        .stop()
        .build()
}

/// `INNER`: one CALL-family opcode to `target` forwarding no gas, with empty ranges.
fn inner_call(opcode: u8, target: Address) -> Bytes {
    let mut b = BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64);
    if opcode == CALL || opcode == CALLCODE {
        b = b.push_number(0_u64);
    }
    b.push_address(target).push_number(0_u64).append(opcode).stop().build()
}

/// `OUTER`: hand `INNER` `budget` gas, then `CALL(0, probe, 0)`.
fn outer_code(budget: u64, probe: Address) -> Bytes {
    let call = |b: BytecodeBuilder, target: Address, gas: u64| {
        b.push_number(0_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_address(target)
            .push_number(gas)
            .append(CALL)
            .append(POP)
    };
    call(call(BytecodeBuilder::default(), INNER, budget), probe, 0).stop().build()
}

fn install(db: &mut MemoryDatabase, address: Address, code: Bytes) {
    let code = Bytecode::new_raw(code);
    let code_hash = code.hash_slow();
    let account = db.load_account(address).expect("in-memory account load");
    account.info.code = Some(code);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
}

fn gas_used(spec: MegaSpecId, budget: u64, inner: &Bytes, probe: Address) -> u64 {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_000_u64))
        .account_balance(PLAIN, U256::from(1))
        .account_balance(COLD, U256::from(1))
        .account_balance(BENEFICIARY, U256::from(1));
    let mut raw = vec![0xef, 0x01, 0x00];
    raw.extend_from_slice(IDENTITY.as_slice());
    let designation = Bytes::from(raw);
    assert!(
        Bytecode::new_raw(designation.clone()).is_eip7702(),
        "fixture must install a real delegation"
    );
    install(&mut db, DELEGATOR, designation);
    install(&mut db, INNER, inner.clone());
    install(&mut db, OUTER, outer_code(budget, probe));

    let mut context = MegaContext::new(&mut db, spec)
        .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(CALLER).call(OUTER).gas_limit(5_000_000).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx must execute");
    assert!(outcome.result.is_success(), "{spec:?}: {:?}", outcome.result);
    #[allow(deprecated)]
    outcome.result.gas_used()
}

/// How much cheaper the later CALL to `probe` is than the same CALL to a cold address.
fn discount(spec: MegaSpecId, budget: u64, inner: Bytes, probe: Address) -> u64 {
    gas_used(spec, budget, &inner, COLD) - gas_used(spec, budget, &inner, probe)
}

/// `EXTCODECOPY` validates its operands, charges its copy cost and expands memory before its
/// load; a frame halting on any of those had already read the target on the deployed schedule.
#[test]
fn test_extcodecopy_halted_before_its_read_leaves_a_prewarmed_target_warm() {
    // 15 gas reaches the opcode with 3: short of a 32-byte copy plus its memory. 5,000 reaches it
    // short of a 4 MiB copy.
    for spec in WRAPPED_CALL_SPECS {
        for (budget, len) in [(15, 32), (5_000, 0x40_0000)] {
            for target in [IDENTITY, BENEFICIARY] {
                assert_eq!(
                    discount(spec, budget, inner_extcodecopy(target, len), target),
                    LEFT_WARM,
                    "{spec:?}: EXTCODECOPY of {target} ({len} bytes, budget {budget})",
                );
            }
            assert_eq!(
                discount(spec, budget, inner_extcodecopy(PLAIN, len), PLAIN),
                LEFT_COLD,
                "{spec:?}: EXTCODECOPY of an account that is not pre-warmed",
            );
        }
    }
}

/// A `REX5` CALLCODE meters the executing account, not its operand, so its body's read of the
/// operand is the only one the deployed schedule issued.
#[test]
fn test_rex5_callcode_below_its_static_charge_leaves_a_prewarmed_operand_warm() {
    for target in [IDENTITY, BENEFICIARY] {
        assert_eq!(
            discount(MegaSpecId::REX5, 80, inner_call(CALLCODE, target), target),
            LEFT_WARM,
            "REX5: CALLCODE to {target} below its static charge",
        );
    }
}

/// The deployed CALL-family body followed an EIP-7702 designation and read the delegate before
/// charging; a pre-warmed delegate stayed warm after the frame reverted.
#[test]
fn test_delegate_of_a_halted_call_stays_warm_when_prewarmed() {
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        for opcode in [CALL, CALLCODE, DELEGATECALL, STATICCALL] {
            for budget in [80, 2_000] {
                assert_eq!(
                    discount(spec, budget, inner_call(opcode, DELEGATOR), IDENTITY),
                    LEFT_WARM,
                    "{spec:?}: opcode 0x{opcode:02x} to a delegator of a precompile, budget {budget}",
                );
            }
        }
    }
}

/// A contract reached only as a delegate, whose code the database serves lazily and cannot serve.
const CODED: Address = address!("00000000000000000000000000000000004300cc");
/// An EIP-7702 delegator whose delegate is [`CODED`].
const CODED_DELEGATOR: Address = address!("00000000000000000000000000000000004300cd");

/// The deployed CALL-family read loaded an EIP-7702 delegate's account but not its code, so the
/// recreated read must not fetch that code either: a node that cannot serve it — a stateless
/// witness carries only what the deployed execution read — still executes the transaction.
#[test]
fn test_recreated_delegate_read_does_not_fetch_the_delegate_code() {
    let delegate_code = Bytes::from_static(&[STOP]);
    let delegate_code_hash = Bytecode::new_raw(delegate_code).hash_slow();
    let mut raw = vec![0xef, 0x01, 0x00];
    raw.extend_from_slice(CODED.as_slice());
    let designation = Bytes::from(raw);
    assert!(
        Bytecode::new_raw(designation.clone()).is_eip7702(),
        "fixture must install a real delegation"
    );

    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        for opcode in [CALL, CALLCODE, DELEGATECALL, STATICCALL] {
            let mut memory = MemoryDatabase::default()
                .account_balance(CALLER, U256::from(1_000_000_000_000_000_u64))
                .account_lazy_code(CODED, delegate_code_hash);
            install(&mut memory, CODED_DELEGATOR, designation.clone());
            install(&mut memory, INNER, inner_call(opcode, CODED_DELEGATOR));
            install(&mut memory, OUTER, outer_code(80, PLAIN));
            let mut db = ErrorInjectingDatabase::new(memory);
            db.fail_on_code_by_hash = Some(delegate_code_hash);

            let mut context = MegaContext::new(&mut db, spec)
                .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() });
            context.modify_chain(|chain| {
                chain.operator_fee_scalar = Some(U256::ZERO);
                chain.operator_fee_constant = Some(U256::ZERO);
            });
            let mut evm = MegaEvm::new(context);
            let mut tx = MegaTransaction::new(
                TxEnvBuilder::default()
                    .caller(CALLER)
                    .call(OUTER)
                    .gas_limit(5_000_000)
                    .build_fill(),
            );
            tx.enveloped_tx = Some(Bytes::new());
            let outcome = alloy_evm::Evm::transact_raw(&mut evm, tx);
            assert!(
                outcome.as_ref().is_ok_and(|outcome| outcome.result.is_success()),
                "{spec:?}: opcode 0x{opcode:02x} to a delegator below its static charge: {outcome:?}",
            );
        }
    }
}

/// A database error on a read the deployed schedule issued before halting fails the transaction,
/// as the deployed read did, for both the CALL-family delegate and the `EXTCODECOPY` target.
#[test]
fn test_recreated_read_database_error_fails_the_transaction() {
    let mut raw = vec![0xef, 0x01, 0x00];
    raw.extend_from_slice(CODED.as_slice());
    let designation = Bytes::from(raw);
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        for (budget, inner) in
            [(80, inner_call(CALL, CODED_DELEGATOR)), (15, inner_extcodecopy(CODED, 32))]
        {
            let mut memory = MemoryDatabase::default()
                .account_balance(CALLER, U256::from(1_000_000_000_000_000_u64));
            install(&mut memory, CODED_DELEGATOR, designation.clone());
            install(&mut memory, INNER, inner.clone());
            install(&mut memory, OUTER, outer_code(budget, PLAIN));
            let mut db = ErrorInjectingDatabase::new(memory);
            db.fail_on_account = Some(CODED);

            let mut context = MegaContext::new(&mut db, spec)
                .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() });
            context.modify_chain(|chain| {
                chain.operator_fee_scalar = Some(U256::ZERO);
                chain.operator_fee_constant = Some(U256::ZERO);
            });
            let mut evm = MegaEvm::new(context);
            let mut tx = MegaTransaction::new(
                TxEnvBuilder::default()
                    .caller(CALLER)
                    .call(OUTER)
                    .gas_limit(5_000_000)
                    .build_fill(),
            );
            tx.enveloped_tx = Some(Bytes::new());
            let outcome = alloy_evm::Evm::transact_raw(&mut evm, tx);
            assert!(outcome.is_err(), "{spec:?}: budget {budget}: {outcome:?}");
        }
    }
}
