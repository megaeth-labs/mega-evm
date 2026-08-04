//! Tests for the EIP-2929 coldness of an account that a *previous* transaction of the same block
//! left resident in the journal, when the *current* transaction lists it in its EIP-2930 access
//! list.
//!
//! A journal reused across the transactions of a block carries every entry earlier transactions
//! materialized, and such an entry is cold for the current transaction — its transaction id is
//! stale. `MegaETH` prices that resident-entry path from the entry alone, so an address that
//! belongs to a pre-warmed set — a precompile, the coinbase — is still a cold access on its first
//! touch. That rule is pinned by `precompile_access_coldness`.
//!
//! The access list is the one warming source that wins over it. A transaction is charged for every
//! listed `(address, slots)` pair, and a pair that carries at least one slot is loaded eagerly
//! before the first opcode runs, so every access to that account in that transaction is warm no
//! matter what earlier transactions left in the journal.
//!
//! A listed address with *no* storage slots is the deliberate exception: nothing loads it eagerly,
//! so a resident entry for it is still cold on its first touch, while a fresh entry for it is warm.
//! The asymmetry is not a rounding of the rule — it is what every `MegaETH` spec charges — and the
//! tests below pin both sides of it.

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    alloy_op_evm::OpTx,
    op_revm::OpTransaction,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, TxEnv},
    ExecuteEvm,
};

const CALLER: Address = address!("0000000000000000000000000000000000600000");
const CONTRACT: Address = address!("0000000000000000000000000000000000600001");

/// The account the first transaction leaves resident in the journal and the probe transaction
/// touches.
const RESIDENT: Address = address!("0000000000000000000000000000000000600002");

/// Absent from the database *and* untouched by the first transaction, so its journal entry is
/// created by the probe transaction itself: the fresh-entry path, where every warming source
/// applies.
const FRESH: Address = address!("00000000000000000000000000000000006000ff");

/// `COLD_ACCOUNT_ACCESS_COST - WARM_STORAGE_READ_COST`: what a cold first touch costs on top of the
/// warm price the interpreter charges for every access.
const COLD_ACCOUNT_SURCHARGE: u64 = 2_500;

/// `COLD_ACCOUNT_ACCESS_COST`. `SELFDESTRUCT` has no warm baseline to subtract — the whole cold
/// access cost is added to its flat price — so its surcharge is 2600, not
/// [`COLD_ACCOUNT_SURCHARGE`].
const COLD_ACCOUNT_ACCESS_COST: u64 = 2_600;

/// `WARM_STORAGE_READ_COST` plus the `PUSH20` and `POP` around it: what one more `EXTCODESIZE` of
/// an already-touched account adds to a program.
const WARM_EXTCODESIZE_STEP: u64 = 100 + 3 + 2;

/// Every spec `MegaETH` runs. The account-load matrix below is asserted for all of them, so a new
/// spec that changes the instruction table has to state which side of the boundary it lands on.
const ALL_SPECS: [MegaSpecId; 9] = [
    MegaSpecId::EQUIVALENCE,
    MegaSpecId::MINI_REX,
    MegaSpecId::REX,
    MegaSpecId::REX1,
    MegaSpecId::REX2,
    MegaSpecId::REX3,
    MegaSpecId::REX4,
    MegaSpecId::REX5,
    MegaSpecId::REX6,
];

/// Specs whose instruction table has a working `SELFDESTRUCT`: `MiniRex` disabled it and `Rex2`
/// re-enabled it with EIP-6780 semantics, while `Equivalence` runs the unmodified mainnet table.
const SELFDESTRUCT_SPECS: [MegaSpecId; 6] = [
    MegaSpecId::EQUIVALENCE,
    MegaSpecId::REX2,
    MegaSpecId::REX3,
    MegaSpecId::REX4,
    MegaSpecId::REX5,
    MegaSpecId::REX6,
];

/// How the probe transaction lists the account it touches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Listing {
    /// Not listed at all.
    Absent,
    /// Listed with one storage slot, which is what makes the account eagerly warm.
    WithSlot,
    /// Listed with no storage slots, which warms nothing that is already resident.
    AddressOnly,
}

/// The three listings, in the order the assertions read best: the one that warms a resident entry
/// first, then the two that must keep charging cold.
const LISTINGS: [Listing; 3] = [Listing::WithSlot, Listing::Absent, Listing::AddressOnly];

impl Listing {
    fn access_list(self, address: Address) -> AccessList {
        let storage_keys = match self {
            Self::Absent => return AccessList::default(),
            Self::WithSlot => vec![B256::ZERO],
            Self::AddressOnly => Vec::new(),
        };
        AccessList(vec![AccessListItem { address, storage_keys }])
    }

    /// What the first touch of a *resident* entry costs on top of a repeat touch.
    fn resident_surcharge(self, cold: u64) -> u64 {
        match self {
            Self::WithSlot => 0,
            Self::Absent | Self::AddressOnly => cold,
        }
    }

    /// What the first touch of a *fresh* entry costs on top of a repeat touch: there both listings
    /// warm the account, because the pre-warmed sets decide that path.
    fn fresh_surcharge(self, cold: u64) -> u64 {
        match self {
            Self::WithSlot | Self::AddressOnly => 0,
            Self::Absent => cold,
        }
    }
}

// ============================================================================
// HELPERS
// ============================================================================

fn wrap(tx: TxEnv) -> OpTx {
    let mut tx = OpTx(OpTransaction::new(tx));
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// The transaction that makes [`RESIDENT`] resident: a bare value transfer to it, so the journal
/// carries its entry into the next transaction of the same block.
fn materializing_tx() -> OpTx {
    wrap(
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(RESIDENT)
            .value(U256::from(1))
            .gas_limit(1_000_000)
            .build_fill(),
    )
}

/// Runs the materializing transaction and then `code` as [`CONTRACT`] **on the same journal**, and
/// returns the probe transaction's gas used.
///
/// Both run through one [`MegaEvm`] via `transact_one`, which commits each transaction into the
/// journal instead of finalizing it away, so the entry the first transaction leaves for
/// [`RESIDENT`] is exactly the resident-but-cold entry the probe transaction has to price.
fn probe_gas_used(spec: MegaSpecId, access_list: AccessList, code: Bytes) -> u64 {
    // `RESIDENT` exists and is non-empty in the database, so neither the value transfer below nor a
    // `SELFDESTRUCT` to it ever pays an account-creation cost: the only thing that varies across
    // the programs differenced below is the warm/cold surcharge.
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(RESIDENT, U256::from(1_000u64))
        .account_code(CONTRACT, code);

    let mut context = MegaContext::new(&mut db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);

    let first = evm.transact_one(materializing_tx()).unwrap();
    assert!(first.is_success(), "{spec:?}: the materializing tx must succeed, got {first:?}");

    // Nonce 1: the journal carries the caller's nonce bump from the materializing transaction,
    // which is the same reuse that carries `RESIDENT`'s entry.
    let probe = wrap(
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .nonce(1)
            .gas_limit(1_000_000)
            .access_list(access_list)
            .build_fill(),
    );
    let probe = evm.transact_one(probe).unwrap();
    assert!(probe.is_success(), "{spec:?}: probe tx must succeed, got {probe:?}");
    probe.tx_gas_used()
}

/// `n` back-to-back `EXTCODESIZE`s of `target`.
fn repeated_extcodesize(target: Address, n: usize) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..n {
        code = code.push_address(target).append(EXTCODESIZE).append(POP);
    }
    code.stop().build()
}

/// `n` back-to-back `STATICCALL`s to `target` with empty calldata and no return data.
fn repeated_static_call(target: Address, n: usize) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..n {
        code = code
            .push_number(0_u64) // retSize
            .push_number(0_u64) // retOffset
            .push_number(0_u64) // argsSize
            .push_number(0_u64) // argsOffset
            .push_address(target)
            .push_number(10_000_u64) // gas
            .append(STATICCALL)
            .append(POP);
    }
    code.stop().build()
}

/// `n` `EXTCODESIZE`s of `target` followed by a `SELFDESTRUCT` that sends the (zero) balance to it.
fn extcodesize_then_selfdestruct(target: Address, n: usize) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..n {
        code = code.push_address(target).append(EXTCODESIZE).append(POP);
    }
    code.push_address(target).append(SELFDESTRUCT).build()
}

/// How much more the first touch of `target` costs than a repeat of the identical opcode sequence,
/// when the probe transaction lists `target` per `listing`.
///
/// Measured by differencing four programs that run the touch 0..=3 times, so every per-touch cost
/// other than the warm/cold surcharge — the operand pushes, the opcode's own static gas, the
/// callee's execution — cancels out, as does the intrinsic cost of the access list itself, which is
/// identical in all four. The two repeat touches are cross-checked against each other so a program
/// whose marginal cost is not constant fails loudly instead of silently skewing the difference.
fn first_touch_surcharge(
    spec: MegaSpecId,
    listing: Listing,
    target: Address,
    program: fn(Address, usize) -> Bytes,
) -> u64 {
    let list = listing.access_list(target);
    let gas: [u64; 4] =
        core::array::from_fn(|n| probe_gas_used(spec, list.clone(), program(target, n)));
    assert_eq!(
        gas[2] - gas[1],
        gas[3] - gas[2],
        "{spec:?}/{listing:?}: repeat touches of {target} must cost the same, got {gas:?}",
    );
    (gas[1] - gas[0]) - (gas[2] - gas[1])
}

// ============================================================================
// TESTS
// ============================================================================

/// `EXTCODESIZE` of an account a previous transaction left in the journal: warm when this
/// transaction paid for it with an access-list entry that carries a storage slot, cold otherwise.
#[test]
fn test_extcodesize_of_resident_account_is_warm_only_when_the_access_list_loads_it() {
    for spec in ALL_SPECS {
        for listing in LISTINGS {
            let surcharge = first_touch_surcharge(spec, listing, RESIDENT, repeated_extcodesize);
            let expected = listing.resident_surcharge(COLD_ACCOUNT_SURCHARGE);
            assert_eq!(
                surcharge, expected,
                "{spec:?}/{listing:?}: first EXTCODESIZE of the resident account must cost \
                 {expected} more than a repeat, got {surcharge}",
            );
        }
    }
}

/// The CALL family reaches the same host entry, and from `Rex` on its storage-gas wrapper inspects
/// the target before the opcode loads it — which is itself one of the ways an entry ends up
/// resident but cold. The access-list verdict has to survive that inspection too.
#[test]
fn test_static_call_to_resident_account_is_warm_only_when_the_access_list_loads_it() {
    for spec in ALL_SPECS {
        for listing in LISTINGS {
            let surcharge = first_touch_surcharge(spec, listing, RESIDENT, repeated_static_call);
            let expected = listing.resident_surcharge(COLD_ACCOUNT_SURCHARGE);
            assert_eq!(
                surcharge, expected,
                "{spec:?}/{listing:?}: first STATICCALL to the resident account must cost \
                 {expected} more than a repeat, got {surcharge}",
            );
        }
    }
}

/// `SELFDESTRUCT` prices its beneficiary through the same rule.
///
/// Measured on the step from 0 to 1 leading `EXTCODESIZE`s of the beneficiary. That first
/// `EXTCODESIZE` pays the first-touch price itself and leaves the `SELFDESTRUCT` warm, so the step
/// is its own cost plus its own surcharge minus the beneficiary surcharge the `SELFDESTRUCT` no
/// longer pays — which makes the step *smaller* than a plain warm `EXTCODESIZE` exactly when the
/// beneficiary access was cold. The step from 1 to 2 is the warm baseline the first step is read
/// against.
#[test]
fn test_selfdestruct_to_resident_beneficiary_is_warm_only_when_the_access_list_loads_it() {
    for spec in SELFDESTRUCT_SPECS {
        for listing in LISTINGS {
            let list = listing.access_list(RESIDENT);
            let gas: [u64; 3] = core::array::from_fn(|n| {
                probe_gas_used(spec, list.clone(), extcodesize_then_selfdestruct(RESIDENT, n))
            });
            let expected_step = WARM_EXTCODESIZE_STEP +
                listing.resident_surcharge(COLD_ACCOUNT_SURCHARGE) -
                listing.resident_surcharge(COLD_ACCOUNT_ACCESS_COST);
            assert_eq!(
                [gas[1] - gas[0], gas[2] - gas[1]],
                [expected_step, WARM_EXTCODESIZE_STEP],
                "{spec:?}/{listing:?}: leading EXTCODESIZEs of the SELFDESTRUCT beneficiary must \
                 step by {expected_step} then {WARM_EXTCODESIZE_STEP}, got {gas:?}",
            );
        }
    }
}

/// A second touch inside the same transaction is warm however the first one was priced: the first
/// access marks the entry warm, and the access-list rule never makes a repeat *more* expensive.
///
/// This is what makes the surcharges above attributable to the first touch alone. The differencing
/// helper asserts that the marginal cost is constant; this asserts what that constant is.
#[test]
fn test_second_touch_in_the_same_transaction_is_always_warm() {
    for spec in ALL_SPECS {
        for listing in LISTINGS {
            let list = listing.access_list(RESIDENT);
            let gas: [u64; 3] = core::array::from_fn(|n| {
                probe_gas_used(spec, list.clone(), repeated_extcodesize(RESIDENT, n + 1))
            });
            assert_eq!(
                [gas[1] - gas[0], gas[2] - gas[1]],
                [WARM_EXTCODESIZE_STEP; 2],
                "{spec:?}/{listing:?}: every EXTCODESIZE after the first must cost \
                 {WARM_EXTCODESIZE_STEP}, got {gas:?}",
            );
        }
    }
}

/// The rule prices the entry a *previous* transaction left behind, not the address. An account no
/// earlier transaction materialized takes the fresh-entry path, where an address-only listing warms
/// it just like a listing with slots — the side of the asymmetry that must not leak into the
/// resident-entry rule.
#[test]
fn test_first_touch_of_a_fresh_account_follows_the_pre_warmed_sets() {
    for spec in ALL_SPECS {
        for listing in LISTINGS {
            let surcharge = first_touch_surcharge(spec, listing, FRESH, repeated_extcodesize);
            let expected = listing.fresh_surcharge(COLD_ACCOUNT_SURCHARGE);
            assert_eq!(
                surcharge, expected,
                "{spec:?}/{listing:?}: first EXTCODESIZE of a fresh account must cost {expected} \
                 more than a repeat, got {surcharge}",
            );
        }
    }
}
