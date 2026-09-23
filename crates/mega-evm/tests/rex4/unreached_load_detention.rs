//! Beneficiary detention on the paths where revm 40 halts ahead of the target load.
//!
//! The deployed implementation loaded a CALL-family or `EXTCODECOPY` target before charging the
//! opcode anything, and loading the block beneficiary marks beneficiary access, which caps the
//! transaction's remaining compute gas. revm 40 charges first — the CALL family's static gas and
//! value-transfer cost, `EXTCODECOPY`'s copy cost and memory expansion — so a frame that runs out
//! of gas on one of those never reaches the load. The handlers recreate the mark wherever the
//! deployed schedule would have reached it, and leave it off wherever that schedule stopped
//! earlier. Every expectation here was measured on the deployed implementation.
//!
//! The observable is consensus-level. The beneficiary cap is lowered to [`CAP`]; `INNER` runs one
//! opcode under a gas budget and halts; `OUTER` then (optionally) runs `BALANCE(PLAIN)` — a
//! volatile-wrapped opcode whose tail applies whatever cap the tracker already holds — and burns
//! more compute gas than the cap allows. A transaction whose `INNER` frame marked beneficiary
//! access halts in that burn; one that did not runs to completion.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BucketHasher, BucketId, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _, TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, BlockEnv},
    database::AccountState,
    state::Bytecode,
};
use std::convert::Infallible;

const CALLER: Address = address!("0000000000000000000000000000000000420000");
const OUTER: Address = address!("0000000000000000000000000000000000420001");
const INNER: Address = address!("0000000000000000000000000000000000420002");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000420099");
/// An EIP-7702 delegator whose delegate is the beneficiary.
const DELEGATOR: Address = address!("00000000000000000000000000000000004200db");
/// A funded account that is not the beneficiary.
const PLAIN: Address = address!("00000000000000000000000000000000004200f1");

/// The lowered beneficiary detention cap, well under [`BURN_PAIRS`]' compute gas.
const CAP: u64 = 5_000;
/// `PUSH1 1; POP` pairs `OUTER` burns after `INNER` returns: 10,000 gas.
const BURN_PAIRS: usize = 2_000;

/// Leaves a CALL-family opcode under its 100-gas static charge (a handful of pushes precede it).
const STATIC_CHARGE_BUDGET: u64 = 50;
/// Covers the static charge but not the 9,000-gas value transfer.
const VALUE_TRANSFER_BUDGET: u64 = 5_000;
/// Reaches the opcode with ~150 gas: a 1,024-byte return range (98 gas) fits only if the 100-gas
/// static charge is still in the frame, as it was on the deployed schedule.
const MEMORY_WINDOW_BUDGET: u64 = 171;
/// Reaches the opcode with ~59 gas: a 1,024-byte return range is unaffordable either way.
const MEMORY_UNAFFORDABLE_BUDGET: u64 = 80;
/// Clears `EXTCODECOPY`'s static charge but not a 32 KiB copy.
const COPY_WINDOW_BUDGET: u64 = 5_000;
/// Gas the seven operand pushes ahead of a valued CALL consume.
const CALL_PUSHES_GAS: u64 = 21;
/// The new-account storage charge at SALT multiplier 2.
const NEW_ACCOUNT_CHARGE_AT_2X: u64 = mega_evm::constants::rex::NEW_ACCOUNT_STORAGE_GAS_BASE;

const CALL_FAMILY: [u8; 4] = [CALL, STATICCALL, DELEGATECALL, CALLCODE];
const WRAPPED_CALL_SPECS: [MegaSpecId; 3] = [MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

/// Every SALT lookup goes to one bucket, so its capacity sets the storage-gas multiplier.
const ONE_BUCKET: BucketId = 7;

#[derive(Debug, Clone, Copy)]
struct OneBucket;

impl BucketHasher for OneBucket {
    fn bucket_id(_key: &[u8]) -> BucketId {
        ONE_BUCKET
    }
}

/// How `INNER` is reached and what surrounds it.
#[derive(Clone, Copy)]
struct Setup {
    spec: MegaSpecId,
    /// `OUTER` reaches `INNER` through `STATICCALL` rather than `CALL`.
    static_frame: bool,
    /// The beneficiary holds a balance; otherwise it is an empty account.
    beneficiary_funded: bool,
    /// SALT multiplier. At 1 a new account costs no storage gas; at 2 it costs the base.
    salt_multiplier: u64,
    /// Value `OUTER` sends with its call into `INNER`, which grants `INNER` a storage stipend.
    outer_value: u64,
    /// `OUTER` runs `BALANCE(PLAIN)` before its burn.
    later_tail: bool,
}

impl Setup {
    const fn on(spec: MegaSpecId) -> Self {
        Self {
            spec,
            static_frame: false,
            beneficiary_funded: true,
            salt_multiplier: 1,
            outer_value: 0,
            later_tail: true,
        }
    }
}

/// `INNER` bytecode: one CALL-family opcode to `target` with an empty input range and a
/// `ret_size`-byte return range. `value` is pushed only for the two opcodes that carry one.
fn inner_call(opcode: u8, target: Address, value: u64, ret_size: u64) -> Bytes {
    let mut b = BytecodeBuilder::default()
        .push_number(ret_size)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64);
    if opcode == CALL || opcode == CALLCODE {
        b = b.push_number(value);
    }
    b.push_address(target).push_number(0_u64).append(opcode).build()
}

/// `INNER` bytecode: one `EXTCODECOPY` of `len` bytes of `target`'s code to memory offset 0.
fn inner_extcodecopy(target: Address, len: u128) -> Bytes {
    BytecodeBuilder::default()
        .push_number(len)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(target)
        .append(EXTCODECOPY)
        .build()
}

fn outer_code(budget: u64, setup: Setup) -> Bytes {
    let mut b = BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64);
    if !setup.static_frame {
        b = b.push_number(setup.outer_value);
    }
    b = b
        .push_address(INNER)
        .push_number(budget)
        .append(if setup.static_frame { STATICCALL } else { CALL })
        .append(POP);
    if setup.later_tail {
        b = b.push_address(PLAIN).append(BALANCE).append(POP);
    }
    for _ in 0..BURN_PAIRS {
        b = b.push_number(1_u8).append(POP);
    }
    b.build()
}

/// Installs `code` as raw bytecode and refuses to run unless a designator decodes as a real
/// delegation — `MemoryDatabase::account_code` would store it as legacy code that delegates
/// nothing.
fn install_delegation(db: &mut MemoryDatabase, address: Address, delegate: Address) {
    let mut raw = vec![0xef, 0x01, 0x00];
    raw.extend_from_slice(delegate.as_slice());
    let code = Bytecode::new_raw(Bytes::from(raw));
    assert!(code.is_eip7702(), "fixture must install a real EIP-7702 delegation at {address}");
    let code_hash = code.hash_slow();
    let account = db.load_account(address).expect("in-memory account load");
    account.info.code = Some(code);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
}

/// Runs the transaction and reports whether it was cut off by the detention cap.
fn detained(setup: Setup, budget: u64, inner_code: Bytes) -> bool {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_000_u64))
        .account_code(OUTER, outer_code(budget, setup))
        .account_balance(OUTER, U256::from(1_000_000_000_u64))
        .account_code(INNER, inner_code)
        .account_balance(INNER, U256::from(1_000_000_000_000_000_000_u128))
        .account_balance(PLAIN, U256::from(1_u64));
    if setup.beneficiary_funded {
        db = db.account_balance(BENEFICIARY, U256::from(1_u64));
    }
    install_delegation(&mut db, DELEGATOR, BENEFICIARY);

    let mut limits = EvmTxRuntimeLimits::from_spec(setup.spec);
    limits.block_env_access_compute_gas_limit = CAP;
    let envs = TestExternalEnvs::<Infallible, OneBucket>::new()
        .with_bucket_capacity(ONE_BUCKET, setup.salt_multiplier * MIN_BUCKET_SIZE as u64);
    let mut context = MegaContext::new(&mut db, setup.spec)
        .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() })
        .with_external_envs(envs.into())
        .with_tx_runtime_limits(limits);
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
    assert!(
        outcome.result.is_success() || outcome.result.is_halt(),
        "{:?}: OUTER absorbs INNER's failure, so the tx either completes or is cut off by the \
         cap: {:?}",
        setup.spec,
        outcome.result
    );
    outcome.result.is_halt()
}

/// A frame below the static charge never reached revm's load, but the deployed schedule charged
/// that gas after loading — so it marked, and the caller's tail capped the transaction.
#[test]
fn test_static_charge_window_call_to_beneficiary_is_detained() {
    for spec in WRAPPED_CALL_SPECS {
        for opcode in CALL_FAMILY {
            assert!(
                detained(
                    Setup::on(spec),
                    STATIC_CHARGE_BUDGET,
                    inner_call(opcode, BENEFICIARY, 0, 0)
                ),
                "{spec:?}: opcode 0x{opcode:02x} to the beneficiary below its static charge",
            );
        }
    }
}

/// The value-transfer cost is the other charge revm 40 takes ahead of its load.
#[test]
fn test_value_transfer_window_call_to_beneficiary_is_detained() {
    for spec in WRAPPED_CALL_SPECS {
        assert!(
            detained(Setup::on(spec), VALUE_TRANSFER_BUDGET, inner_call(CALL, BENEFICIARY, 1, 0)),
            "{spec:?}: a valued CALL to the beneficiary short of the transfer cost",
        );
    }
}

/// The body's memory expansions precede the load on both schedules, but on the deployed one the
/// static charge was still in the frame when they ran.
#[test]
fn test_memory_window_follows_the_budget_the_deployed_schedule_had() {
    let spec = MegaSpecId::REX6;
    assert!(
        detained(Setup::on(spec), MEMORY_WINDOW_BUDGET, inner_call(CALL, BENEFICIARY, 0, 1024)),
        "an expansion affordable only with the static charge still in the frame reached the load",
    );
    assert!(
        !detained(
            Setup::on(spec),
            MEMORY_UNAFFORDABLE_BUDGET,
            inner_call(CALL, BENEFICIARY, 0, 1024)
        ),
        "an expansion unaffordable either way stopped the deployed schedule before its load",
    );
}

/// revm rejects a valued CALL in a static frame before its memory expansions and its load.
#[test]
fn test_value_call_in_static_frame_is_not_detained() {
    let setup = Setup { static_frame: true, ..Setup::on(MegaSpecId::REX6) };
    assert!(!detained(setup, STATIC_CHARGE_BUDGET, inner_call(CALL, BENEFICIARY, 1, 0)));
}

/// A valued call to an empty beneficiary first pays the storage-gas wrapper's new-account charge,
/// which the deployed schedule took before its load. Free at the minimum SALT multiplier; at the
/// base cost the frame cannot afford it unless a storage stipend covers most of it.
#[test]
fn test_value_call_to_empty_beneficiary_follows_the_new_account_charge() {
    let empty = Setup { beneficiary_funded: false, ..Setup::on(MegaSpecId::REX6) };
    let inner = || inner_call(CALL, BENEFICIARY, 1, 0);
    assert!(
        detained(empty, STATIC_CHARGE_BUDGET, inner()),
        "no new-account charge at the minimum multiplier: the deployed schedule reached the load",
    );
    assert!(
        !detained(Setup { salt_multiplier: 2, ..empty }, STATIC_CHARGE_BUDGET, inner()),
        "an unaffordable new-account charge stopped the deployed schedule before its load",
    );
    assert!(
        detained(
            Setup { salt_multiplier: 2, outer_value: 1, ..empty },
            STATIC_CHARGE_BUDGET,
            inner()
        ),
        "a storage stipend that covers the charge let the deployed schedule reach its load",
    );
}

/// A frame short of the new-account charge by less than the static charge: the deployed schedule
/// took the storage charge with the static gas still in the frame, paid it, reached its load and
/// marked; revm 40 debits the static charge first and then fails the storage charge. The two
/// outer rows pin the window's edges, where both schedules agree.
#[test]
fn test_storage_charge_window_follows_the_deployed_static_charge() {
    let at_opcode = |gas: u64| gas + CALL_PUSHES_GAS;
    let rows = [
        (at_opcode(NEW_ACCOUNT_CHARGE_AT_2X - 1), false),
        (at_opcode(NEW_ACCOUNT_CHARGE_AT_2X), true),
        (at_opcode(NEW_ACCOUNT_CHARGE_AT_2X + 99), true),
        (at_opcode(NEW_ACCOUNT_CHARGE_AT_2X + 100), true),
    ];
    for spec in WRAPPED_CALL_SPECS {
        let setup = Setup { beneficiary_funded: false, salt_multiplier: 2, ..Setup::on(spec) };
        for (budget, expected) in rows {
            assert_eq!(
                detained(setup, budget, inner_call(CALL, BENEFICIARY, 1, 0)),
                expected,
                "{spec:?}: a valued CALL to an empty beneficiary with a budget of {budget}",
            );
        }
    }
}

/// From `Rex6` the deployed host also marked the operand's one-hop EIP-7702 delegate.
#[test]
fn test_delegate_to_beneficiary_is_detained_from_rex6() {
    let inner = || inner_call(CALL, DELEGATOR, 0, 0);
    assert!(detained(Setup::on(MegaSpecId::REX6), STATIC_CHARGE_BUDGET, inner()));
    assert!(!detained(Setup::on(MegaSpecId::REX5), STATIC_CHARGE_BUDGET, inner()));
}

#[test]
fn test_non_beneficiary_target_is_not_detained() {
    for opcode in CALL_FAMILY {
        assert!(!detained(
            Setup::on(MegaSpecId::REX6),
            STATIC_CHARGE_BUDGET,
            inner_call(opcode, PLAIN, 0, 0)
        ));
    }
}

/// `EXTCODECOPY`'s handler aborts on a halt without applying the cap — on both schedules — so the
/// mark it leaves caps the transaction only once a later volatile-wrapped tail applies it.
#[test]
fn test_extcodecopy_mark_is_latent_until_a_later_tail_applies_it() {
    let copy = || inner_extcodecopy(BENEFICIARY, 0x8000);
    for spec in [MegaSpecId::MINI_REX, MegaSpecId::REX6] {
        assert!(
            detained(Setup::on(spec), COPY_WINDOW_BUDGET, copy()),
            "{spec:?}: the copy-cost halt left the beneficiary marked",
        );
        assert!(
            !detained(Setup { later_tail: false, ..Setup::on(spec) }, COPY_WINDOW_BUDGET, copy()),
            "{spec:?}: with no later tail the mark never caps the transaction",
        );
    }
}

/// An oversized length halts revm 40's body before it charges anything, but the deployed schedule
/// had already loaded by then.
#[test]
fn test_extcodecopy_invalid_length_still_marks() {
    assert!(detained(
        Setup::on(MegaSpecId::REX6),
        COPY_WINDOW_BUDGET,
        inner_extcodecopy(BENEFICIARY, 1 << 64)
    ));
}

/// Before `Rex4` the CALL family has no volatile wrapper to apply the cap, so a halt past the
/// static pre-charge leaves a latent mark there too.
#[test]
fn test_pre_rex4_value_window_leaves_a_latent_mark() {
    assert!(detained(
        Setup::on(MegaSpecId::REX3),
        VALUE_TRANSFER_BUDGET,
        inner_call(CALL, BENEFICIARY, 1, 0)
    ));
}
