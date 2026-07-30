//! Direct assertions for the normative claims in `docs/spec/evm/compute-gas.md` that the
//! cross-spec snapshot cannot pin on its own.
//!
//! The snapshot in [`super`] records *numbers*; it detects that a value moved but does not state
//! why the value is what it is. These tests assert the rules behind the numbers, so a refactor
//! that happens to preserve a snapshot value while breaking the underlying rule still fails.
//!
//! Each test names the spec section it pins.

use std::convert::Infallible;

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaLimitControl, LimitKind, MegaLimitExceeded, MegaSpecId, SaltEnv,
    TestExternalEnvs, LIMIT_CONTROL_ADDRESS, MIN_BUCKET_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CREATE, GAS, MSTORE, POP, PUSH0, RETURN, RETURNDATACOPY, SELFDESTRUCT, STATICCALL,
    },
    context::result::ExecutionResult,
};

use crate::{
    base_db, push_call_operands, push_valueless_call_operands, transact, transact_output,
    transact_with_access_list, transact_with_envs, transact_with_limits, Outcome, CALLEE, CALLER,
    CONTRACT, EMPTY_TARGET, EXISTING_TARGET, ONE_ETH, PRECOMPILE_IDENTITY,
};

/// `remainingComputeGas()` — the `MegaLimitControl` selector the interceptor recognizes.
const REMAINING_COMPUTE_GAS_SELECTOR: [u8; 4] =
    IMegaLimitControl::remainingComputeGasCall::SELECTOR;

/// Builds a program that CALLs `remainingComputeGas()` on `MegaLimitControl`, forwarding
/// `forwarded_gas`. The return data is discarded (`retSize = 0`) so the measurement is not
/// perturbed by memory expansion.
fn intercepted_call(forwarded_gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0x0, REMAINING_COMPUTE_GAS_SELECTOR)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(LIMIT_CONTROL_ADDRESS)
        .push_number(forwarded_gas)
        .append(CALL)
        .append(POP)
        .stop()
        .build()
}

/// Spec: [System Contract Interception] — "Where an interceptor performs no metering of its own, a
/// node MUST NOT record compute gas for the interception: the forwarded gas is returned to the
/// caller in full." `MegaLimitControl` is such an interceptor.
///
/// If the interception consumed or recorded any portion of the forwarded gas, changing how much is
/// forwarded would change the transaction's compute gas. Both operands are three bytes wide, so the
/// two programs are byte-identical in length and differ only in the forwarded amount.
#[test]
fn test_interception_records_no_compute_gas() {
    const LOW: u64 = 1_000_000; // 0x0F4240 — PUSH3
    const HIGH: u64 = 9_000_000; // 0x895440 — PUSH3
    assert_eq!(
        intercepted_call(LOW).len(),
        intercepted_call(HIGH).len(),
        "the two programs must differ only in the forwarded-gas operand value"
    );

    // MegaLimitControl is deployed from Rex4 onward.
    for spec in [MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6] {
        let low = transact(spec, base_db(intercepted_call(LOW)));
        let high = transact(spec, base_db(intercepted_call(HIGH)));

        assert_eq!(
            low.outcome, "success",
            "{spec:?}: intercepted call should succeed, got {}",
            low.outcome
        );
        assert_eq!(
            low.compute_gas, high.compute_gas,
            "{spec:?}: forwarding {HIGH} instead of {LOW} gas to an interceptor must not change \
             compute gas — interception records none of it ({} vs {})",
            low.compute_gas, high.compute_gas
        );
        assert_eq!(
            low.gas_used, high.gas_used,
            "{spec:?}: forwarded gas must be returned to the caller in full ({} vs {})",
            low.gas_used, high.gas_used
        );
    }
}

/// Spec: [System Contract Interception] — the *absolute* half of the same rule.
///
/// [`test_interception_records_no_compute_gas`] only shows the recorded amount is independent of
/// how much gas is forwarded. A fixed per-interception charge would satisfy that and still violate
/// the rule. This test pins the absolute cost by differencing against a structurally identical
/// CALL whose only difference is the callee: a plain account instead of the system contract.
///
/// Both targets are cold, both take the same operand encoding, both return immediately with no
/// return data. Any compute gas the interception records itself would show up as a difference.
#[test]
fn test_interception_costs_the_same_as_a_plain_call() {
    /// Same shape as [`intercepted_call`] but targeting an arbitrary address instead of the
    /// system contract, so the CALL is not intercepted.
    fn plain_call(to: alloy_primitives::Address, forwarded_gas: u64) -> Bytes {
        BytecodeBuilder::default()
            .mstore(0x0, REMAINING_COMPUTE_GAS_SELECTOR)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(4_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_address(to)
            .push_number(forwarded_gas)
            .append(CALL)
            .append(POP)
            .stop()
            .build()
    }

    const FORWARDED_GAS: u64 = 1_000_000;
    assert_eq!(
        intercepted_call(FORWARDED_GAS).len(),
        plain_call(EXISTING_TARGET, FORWARDED_GAS).len(),
        "the intercepted and plain programs must be byte-identical apart from the target address"
    );

    for spec in [MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6] {
        let intercepted = transact(spec, base_db(intercepted_call(FORWARDED_GAS)));
        let plain = transact(spec, base_db(plain_call(EXISTING_TARGET, FORWARDED_GAS)));

        assert_eq!(intercepted.outcome, "success", "{spec:?}: intercepted call should succeed");
        assert_eq!(plain.outcome, "success", "{spec:?}: plain call should succeed");
        assert_eq!(
            intercepted.compute_gas, plain.compute_gas,
            "{spec:?}: an intercepted call must cost exactly what the CALL opcode alone costs — \
             the interception itself records no compute gas (intercepted={} plain={})",
            intercepted.compute_gas, plain.compute_gas
        );
    }
}

/// Spec: [Transaction Intrinsic Gas] — "A node MUST NOT include `MegaETH`'s intrinsic storage gas
/// additions ... in the recorded compute gas, even though both are added to the same intrinsic gas
/// total charged against the transaction's gas limit."
///
/// Rex adds `TX_INTRINSIC_STORAGE_GAS` (39,000) on top of `MiniRex`'s intrinsic. With empty
/// calldata that flat charge is the *only* intrinsic difference between the two specs, so it
/// isolates cleanly: `gas_used` must rise by exactly 39,000 while `compute_gas` must not move at
/// all.
#[test]
fn test_intrinsic_compute_gas_excludes_megaeth_storage_gas() {
    const TX_INTRINSIC_STORAGE_GAS: u64 = 39_000;

    // A bare transfer to an account that already exists: no code, no storage gas, no calldata.
    let build = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10 * ONE_ETH))
            .account_balance(EXISTING_TARGET, U256::from(ONE_ETH))
    };

    let mini_rex = transact(MegaSpecId::MINI_REX, build());
    let rex = transact(MegaSpecId::REX, build());

    assert_eq!(
        mini_rex.compute_gas, rex.compute_gas,
        "the 39,000 intrinsic storage gas must not enter compute gas (MiniRex={} Rex={})",
        mini_rex.compute_gas, rex.compute_gas
    );
    assert_eq!(
        rex.gas_used.checked_sub(mini_rex.gas_used).unwrap_or_else(|| panic!(
            "Rex must not charge less total gas than MiniRex (MiniRex={} Rex={})",
            mini_rex.gas_used, rex.gas_used
        )),
        TX_INTRINSIC_STORAGE_GAS,
        "Rex must charge exactly {TX_INTRINSIC_STORAGE_GAS} more total gas than MiniRex \
         (MiniRex={} Rex={})",
        mini_rex.gas_used,
        rex.gas_used
    );
    assert!(
        rex.gas_used > rex.compute_gas,
        "the storage intrinsic must still be charged against the gas limit \
         (gas_used={} compute_gas={})",
        rex.gas_used,
        rex.compute_gas
    );
}

/// Spec: [Refund Exclusion] — "A node MUST NOT subtract EVM gas refunds from compute gas usage.
/// Refunds affect final gas settlement but do not reduce the compute gas recorded during
/// execution."
///
/// Two programs that perform one `SSTORE` each on a slot holding a non-zero value. Clearing the
/// slot earns the EVM's `SSTORE_CLEARS_SCHEDULE` refund; overwriting it with another non-zero value
/// does not. Both charge the same `SSTORE_RESET` cost and encode to the same push width, so the
/// pair isolates the refund exactly: it must lower `gas_used` and leave `compute_gas` untouched.
#[test]
fn test_refunds_do_not_reduce_compute_gas() {
    /// The inherited EVM's refund for clearing a storage slot (EIP-3529).
    const SSTORE_CLEARS_SCHEDULE: u64 = 4_800;

    let program = |new_value: U256| {
        move || {
            base_db(BytecodeBuilder::default().sstore(U256::from(7), new_value).stop().build())
                .account_storage(CONTRACT, U256::from(7), U256::from(1))
        }
    };
    let clearing = program(U256::ZERO); // 1 -> 0: earns the refund
    let overwriting = program(U256::from(2)); // 1 -> 2: no refund

    for spec in [
        MegaSpecId::MINI_REX,
        MegaSpecId::REX,
        MegaSpecId::REX2,
        MegaSpecId::REX4,
        MegaSpecId::REX5,
        MegaSpecId::REX6,
    ] {
        let cleared = transact(spec, clearing());
        let overwritten = transact(spec, overwriting());
        assert_eq!(cleared.outcome, "success", "{spec:?}: slot clear should succeed");
        assert_eq!(overwritten.outcome, "success", "{spec:?}: slot overwrite should succeed");

        assert_eq!(
            overwritten.gas_used.checked_sub(cleared.gas_used).unwrap_or_else(|| panic!(
                "{spec:?}: clearing must not cost more than overwriting \
                 (cleared={} overwritten={})",
                cleared.gas_used, overwritten.gas_used
            )),
            SSTORE_CLEARS_SCHEDULE,
            "{spec:?}: the refund must be settled out of gas_used \
             (cleared={} overwritten={})",
            cleared.gas_used,
            overwritten.gas_used
        );
        assert_eq!(
            cleared.compute_gas, overwritten.compute_gas,
            "{spec:?}: the refund must NOT be subtracted from compute gas \
             (cleared={} overwritten={})",
            cleared.compute_gas, overwritten.compute_gas
        );
    }
}

/// Spec: [Precompiles], the KZG branch — "`KZG_POINT_EVALUATION_GAS_COST`, when the invocation
/// targeted the KZG point-evaluation precompile and its effective gas limit was at least
/// `KZG_POINT_EVALUATION_GAS_COST`".
///
/// The constant is a **`MegaETH` override** (100,000), not the inherited EVM value (50,000). This
/// test pins the number itself rather than merely its presence: on the KZG non-out-of-gas error
/// path, Rex4 records the spent amount (zero, because the precompile never charged) while Rex5
/// records the fixed cost. The difference between the two specs is therefore exactly the
/// constant, and a recording that fell back to the inherited value would miss by half.
#[test]
fn test_kzg_error_path_records_the_megaeth_fixed_cost() {
    /// `MegaETH`'s override, defined in `crates/mega-evm/src/evm/precompiles.rs`.
    /// Deliberately NOT imported from the crate: hard-coding it here means a change to the
    /// implementation constant fails this test instead of silently following it.
    const KZG_POINT_EVALUATION_GAS_COST: u64 = 100_000;

    let program = crate::corpus()
        .into_iter()
        .find(|p| p.name == "precompile_kzg_invalid_input")
        .expect("corpus must contain precompile_kzg_invalid_input");

    let rex4 = transact(MegaSpecId::REX4, (program.build_db)());
    let rex5 = transact(MegaSpecId::REX5, (program.build_db)());
    let rex6 = transact(MegaSpecId::REX6, (program.build_db)());

    assert_eq!(
        rex5.compute_gas.checked_sub(rex4.compute_gas).unwrap_or_else(|| panic!(
            "Rex5 must not record less compute gas than Rex4 (Rex4={} Rex5={})",
            rex4.compute_gas, rex5.compute_gas
        )),
        KZG_POINT_EVALUATION_GAS_COST,
        "Rex5 must record MegaETH's fixed KZG cost where Rex4 recorded the spent amount \
         (Rex4={} Rex5={})",
        rex4.compute_gas,
        rex5.compute_gas
    );
    assert_eq!(
        rex6.compute_gas, rex5.compute_gas,
        "Rex6 inherits the Rex5 KZG recording rule unchanged (Rex5={} Rex6={})",
        rex5.compute_gas, rex6.compute_gas
    );
}

/// Spec: [Storage Gas Exclusion], at the one surcharge site the snapshot corpus cannot reach.
///
/// Rex5 charges dynamic new-account storage gas when `SELFDESTRUCT` materializes an empty
/// beneficiary. That charge is `base × (multiplier − 1)`, and the corpus runs against the empty
/// external environment where every bucket sits at `MIN_BUCKET_SIZE` — so the multiplier is 1, the
/// surcharge is zero, and the corpus's Rex4 and Rex5 rows are identical. A regression that let the
/// surcharge leak into compute gas would leave the snapshot unchanged.
///
/// This test puts the beneficiary's bucket above the minimum so the surcharge is non-zero, then
/// asserts the split: `gas_used` rises by exactly the surcharge while `compute_gas` does not move.
#[test]
fn test_selfdestruct_storage_surcharge_stays_out_of_compute_gas() {
    /// Rex's base cost for materializing an account, scaled by `multiplier − 1`.
    const NEW_ACCOUNT_STORAGE_GAS_BASE: u64 = 25_000;
    /// Bucket capacity multiplier for the beneficiary's bucket.
    const MULTIPLIER: u64 = 4;

    let envs = || {
        let bucket = TestExternalEnvs::<Infallible>::bucket_id_for_account(EMPTY_TARGET);
        TestExternalEnvs::<Infallible>::new()
            .with_bucket_capacity(bucket, MIN_BUCKET_SIZE as u64 * MULTIPLIER)
    };
    let build = || {
        base_db(BytecodeBuilder::default().push_address(EMPTY_TARGET).append(SELFDESTRUCT).build())
    };

    let rex4 = transact_with_envs(MegaSpecId::REX4, build(), envs());
    let rex5 = transact_with_envs(MegaSpecId::REX5, build(), envs());

    let surcharge = NEW_ACCOUNT_STORAGE_GAS_BASE * (MULTIPLIER - 1);
    assert!(surcharge > 0, "the fixture must produce a non-zero surcharge");

    assert_eq!(
        rex5.gas_used.checked_sub(rex4.gas_used).unwrap_or_else(|| panic!(
            "Rex5 must not charge less total gas than Rex4 (Rex4={} Rex5={})",
            rex4.gas_used, rex5.gas_used
        )),
        surcharge,
        "Rex5 must charge the empty-beneficiary storage surcharge that Rex4 does not \
         (Rex4={} Rex5={})",
        rex4.gas_used,
        rex5.gas_used
    );
    assert_eq!(
        rex4.compute_gas, rex5.compute_gas,
        "the surcharge is storage gas and MUST NOT enter compute gas (Rex4={} Rex5={})",
        rex4.compute_gas, rex5.compute_gas
    );
}

/// Looks up a corpus program by name so a claim test and the snapshot exercise the same bytecode.
fn corpus_program(name: &str) -> crate::Program {
    crate::corpus()
        .into_iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("corpus must contain {name}"))
}

/// Spec: [Forwarded Gas Exclusion] — "`forwarded_child_gas` MUST be the child frame's gas limit,
/// less the standard EVM `CALL_STIPEND` when all of the following hold: the spec is Rex5 or later,
/// the call scheme is `CALL` or `CALLCODE`, and the call transfers non-zero value."
///
/// The snapshot records the resulting numbers but not the rule. This test states it: the exclusion
/// makes the parent record exactly `CALL_STIPEND` more compute gas from Rex5 onward, and only for
/// the qualifying scheme-and-value combination. Each of the rule's three conditions gets a
/// negative case, so dropping any one of them fails here.
#[test]
fn test_call_stipend_is_excluded_from_forwarded_gas_only_when_the_rule_applies() {
    /// The inherited EVM's value-transfer call stipend.
    const CALL_STIPEND: u64 = 2_300;

    let delta_rex4_to_rex5 = |name: &str| -> i64 {
        let program = corpus_program(name);
        let rex4 = transact(MegaSpecId::REX4, (program.build_db)());
        let rex5 = transact(MegaSpecId::REX5, (program.build_db)());
        assert_eq!(rex4.outcome, "success", "{name}: Rex4 run should succeed");
        assert_eq!(rex5.outcome, "success", "{name}: Rex5 run should succeed");
        rex5.compute_gas as i64 - rex4.compute_gas as i64
    };

    // Qualifying: CALL and CALLCODE that transfer value.
    for name in ["call_value_to_existing", "call_value_to_empty", "callcode_value"] {
        assert_eq!(
            delta_rex4_to_rex5(name),
            CALL_STIPEND as i64,
            "{name}: Rex5 must exclude CALL_STIPEND from the forwarded amount, making the parent \
             record exactly {CALL_STIPEND} more compute gas than Rex4"
        );
    }

    // Non-qualifying by scheme: DELEGATECALL and STATICCALL never carry a stipend.
    // Non-qualifying by value: a CALL with value == 0 does not receive one either.
    for name in ["delegatecall", "staticcall", "call_no_value_to_code"] {
        assert_eq!(
            delta_rex4_to_rex5(name),
            0,
            "{name}: does not satisfy the scheme-and-value condition, so Rex5 must record the \
             same compute gas as Rex4"
        );
    }
}

/// Spec: [Contract Creation Memory Expansion] — the three-way split of which window records
/// `CREATE2`'s wrapper-side memory-expansion gas (MiniRex–Rex4 second window skipped on inner
/// failure / Rex5 separate eager window / Rex6 folded into the single window).
///
/// The split is observable only on the failure path, where the expansion happens but the inner
/// opcode never completes. On the success path all three arrangements must agree — that agreement
/// is the spec's claim that Rex6 is behavior-preserving there.
#[test]
fn test_create2_memory_expansion_window_differs_only_on_the_failure_path() {
    let readings = |name: &str| {
        let program = corpus_program(name);
        [MegaSpecId::MINI_REX, MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6]
            .map(|spec| transact(spec, (program.build_db)()).compute_gas)
    };

    // Success path: the window arrangement is unobservable, so every spec must agree.
    let [succ_mini, succ_rex4, succ_rex5, succ_rex6] = readings("create2_with_initcode");
    assert_eq!(
        [succ_mini, succ_rex4, succ_rex5],
        [succ_rex6; 3],
        "on the straight-line success path every spec must record the same CREATE2 compute gas \
         (MiniRex={succ_mini} Rex4={succ_rex4} Rex5={succ_rex5} Rex6={succ_rex6})"
    );

    // Failure path: the initcode is oversized, so the memory expansion runs but the inner opcode
    // rejects. Only Rex5's eager recording captures the expansion.
    let [fail_mini, fail_rex4, fail_rex5, fail_rex6] = readings("create2_oversized_initcode");
    assert_eq!(
        fail_mini, fail_rex4,
        "MiniRex and Rex4 both skip the trailing record when the inner opcode fails \
         (MiniRex={fail_mini} Rex4={fail_rex4})"
    );
    assert_eq!(
        fail_rex6, fail_rex4,
        "Rex6 halts before the expansion, so it records the same as the specs that skip the \
         trailing record (Rex4={fail_rex4} Rex6={fail_rex6})"
    );
    assert!(
        fail_rex5 > fail_rex4,
        "Rex5 records the memory-expansion gas eagerly, before the inner opcode rejects, so it \
         must record strictly more than the others (Rex4={fail_rex4} Rex5={fail_rex5})"
    );
}

/// Builds a caller that STATICCALLs `callee`, forwarding all available gas (`GAS`), and returns the
/// callee's reported remaining gas as the transaction output.
fn forwarding_probe() -> (Bytes, Bytes) {
    // Callee: report the gas it was given.
    let callee = BytecodeBuilder::default()
        .append_many([GAS, PUSH0, MSTORE])
        .push_number(32_u64)
        .append(PUSH0)
        .append(RETURN)
        .build();

    // Caller: STATICCALL(gas=GAS, to=callee, in=0/0, out=0/0), then copy and return the result.
    let caller = BytecodeBuilder::default()
        .push_number(0_u64) // retSize — read via RETURNDATACOPY instead
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(CALLEE)
        .append(GAS)
        .append(STATICCALL)
        .append(POP)
        .push_number(32_u64) // size
        .push_number(0_u64) // returndata offset
        .push_number(0_u64) // memory offset
        .append(RETURNDATACOPY)
        .push_number(32_u64)
        .append(PUSH0)
        .append(RETURN)
        .build();

    (caller, callee)
}

/// Spec: [Forwarded Gas Exclusion] — "Under `MiniRex`, `CALLCODE`, `DELEGATECALL`, and `STATICCALL`
/// do not apply the 98/100 forwarding cap ... From Rex onward these three opcodes are subject to
/// the same forwarding cap as `CALL`."
///
/// Without `MegaETH`'s cap, `STATICCALL` forwards the inherited EVM's 63/64 (98.4375%); with it,
/// the forwarded amount is capped at 98/100. Rex is therefore strictly stingier than `MiniRex`, and
/// the gap is large enough at this gas limit to be unambiguous.
///
/// This pins the frozen `MiniRex` quirk: if a refactor ever "fixes" `MiniRex` by routing these
/// opcodes through the cap, replay for MiniRex-era blocks breaks, and this test fails.
#[test]
fn test_minirex_staticcall_is_not_subject_to_the_forwarding_cap() {
    let (caller_code, callee_code) = forwarding_probe();
    let build = || base_db(caller_code.clone()).account_code(CALLEE, callee_code.clone());

    let forwarded = |spec| {
        let o = transact_output(spec, build());
        U256::from_be_slice(&o).to::<u64>()
    };

    let mini_rex = forwarded(MegaSpecId::MINI_REX);
    let rex = forwarded(MegaSpecId::REX);

    assert!(
        mini_rex > rex,
        "MiniRex STATICCALL must forward more than Rex (uncapped 63/64 vs capped 98/100): \
         MiniRex={mini_rex} Rex={rex}"
    );

    // 63/64 = 98.4375%, 98/100 = 98%. Confirm each side lands on its own rule rather than merely
    // differing, using the caller's pre-call remaining gas reconstructed from the forwarded amount.
    let ratio_bp = |fwd: u64, base: u64| fwd * 10_000 / base;
    let base = mini_rex * 64 / 63; // caller's remaining before the MiniRex forward
    assert!(
        (9_800..=9_850).contains(&ratio_bp(mini_rex, base)),
        "MiniRex should forward ~63/64 (9843 bp), got {} bp",
        ratio_bp(mini_rex, base)
    );
    assert!(
        (9_750..=9_805).contains(&ratio_bp(rex, base)),
        "Rex should forward ~98/100 (9800 bp), got {} bp",
        ratio_bp(rex, base)
    );
}

/// The inherited EVM's cold account access cost (EIP-2929).
const COLD_ACCOUNT_ACCESS_COST: u64 = 2_600;
/// The inherited EVM's warm account access cost (EIP-2929 `WARM_STORAGE_READ_COST`).
const WARM_ACCOUNT_ACCESS_COST: u64 = 100;
/// The observable cost of a defeated warm preload: the first touch pays the cold account access
/// cost where the inherited EVM charges the warm cost, so the first call to such an address costs
/// exactly 2,600 − 100 = 2,500 more than the second.
const COLD_IN_PLACE_OF_WARM: i64 = (COLD_ACCOUNT_ACCESS_COST - WARM_ACCOUNT_ACCESS_COST) as i64;

/// A program of `n` identical CALL units targeting `to` with no value, each unit being the seven
/// operand pushes, the CALL, and a POP of the status flag.
fn repeated_call(to: Address, n: usize) -> Bytes {
    let mut b = BytecodeBuilder::default();
    for _ in 0..n {
        b = push_call_operands(b, to, 0, 50_000).append(CALL).append(POP);
    }
    b.stop().build()
}

/// Same as [`repeated_call`] but with STATICCALL units (six operands, no value word).
fn repeated_staticcall(to: Address, n: usize) -> Bytes {
    let mut b = BytecodeBuilder::default();
    for _ in 0..n {
        b = push_valueless_call_operands(b, to, 50_000).append(STATICCALL).append(POP);
    }
    b.stop().build()
}

/// Measures how much more the first call in a transaction costs than the second call to the same
/// target.
///
/// Runs the 0-, 1-, and 2-unit variants of `program` through `run` and differences the chosen
/// `metric` between consecutive variants. The call units are byte-identical, so the per-unit push
/// and POP bookkeeping cancels exactly, leaving `cost(first call) − cost(second call)`. The second
/// call always observes a warm target (the first call loaded it), so the result is
/// [`COLD_IN_PLACE_OF_WARM`] when the first touch was charged cold and `0` when the target's
/// preloaded warmth was honored.
fn first_call_extra_cost(
    run: impl Fn(Bytes) -> Outcome,
    program: impl Fn(usize) -> Bytes,
    metric: impl Fn(&Outcome) -> u64,
) -> i64 {
    let [c0, c1, c2] = [0_usize, 1, 2].map(|n| {
        let outcome = run(program(n));
        assert_eq!(outcome.outcome, "success", "the {n}-call program should succeed");
        metric(&outcome) as i64
    });
    (c1 - c0) - (c2 - c1)
}

/// Spec: [Inherited-Cost Exception: Preload-Warm Addresses] — "When the first access to such an
/// address in a transaction is made by one of the opcodes below, the opcode MUST charge the cold
/// account access cost in place of the warm cost", with `CALL` charged cold since `MiniRex`.
///
/// The inherited EVM treats precompile addresses as warm from the start of every transaction
/// without loading them. From `MiniRex`, the first CALL to a precompile is charged cold anyway:
/// the first-vs-second-call difference is exactly 2,500 on every metering spec. Under Equivalence
/// the difference — measured on `gas_used`, since Equivalence records no compute gas — is zero,
/// pinning that the departure is `MegaETH`'s and not inherited.
#[test]
fn test_first_call_to_a_precompile_is_charged_cold_from_minirex() {
    let program = |n| repeated_call(PRECOMPILE_IDENTITY, n);

    let equivalence = first_call_extra_cost(
        |code| transact(MegaSpecId::EQUIVALENCE, base_db(code)),
        program,
        |o| o.gas_used,
    );
    assert_eq!(
        equivalence, 0,
        "Equivalence: the inherited EVM honors the precompile's preloaded warmth, so the first \
         and second CALL must cost the same"
    );

    for (spec, spec_name) in crate::ALL_SPECS {
        if !spec.is_enabled(MegaSpecId::MINI_REX) {
            continue;
        }
        let extra =
            first_call_extra_cost(|code| transact(spec, base_db(code)), program, |o| o.compute_gas);
        assert_eq!(
            extra, COLD_IN_PLACE_OF_WARM,
            "{spec_name}: the first CALL to a precompile must be charged cold in place of warm"
        );
    }
}

/// Spec: [Inherited-Cost Exception: Preload-Warm Addresses] — the opcode table's second row:
/// `DELEGATECALL` and `STATICCALL` charge preload-warm addresses cold from Rex, not from
/// `MiniRex`.
///
/// Under `MiniRex` these opcodes run without the account-inspecting wrapper, so the precompile's
/// preloaded warmth is honored and the first-vs-second-call difference is zero — a frozen quirk
/// this test pins: wiring the wrapper into `MiniRex` would break replay of MiniRex-era blocks and
/// fail here. From Rex onward the difference is the full cold-for-warm charge.
#[test]
fn test_staticcall_charges_preload_warm_addresses_cold_from_rex_only() {
    let program = |n| repeated_staticcall(PRECOMPILE_IDENTITY, n);
    let extra = |spec| {
        first_call_extra_cost(|code| transact(spec, base_db(code)), program, |o| o.compute_gas)
    };

    assert_eq!(
        extra(MegaSpecId::MINI_REX),
        0,
        "MiniRex: STATICCALL must honor the precompile's preloaded warmth"
    );

    for (spec, spec_name) in crate::ALL_SPECS {
        if !spec.is_enabled(MegaSpecId::REX) {
            continue;
        }
        assert_eq!(
            extra(spec),
            COLD_IN_PLACE_OF_WARM,
            "{spec_name}: the first STATICCALL to a precompile must be charged cold in place of \
             warm"
        );
    }
}

/// Spec: [Inherited-Cost Exception: Preload-Warm Addresses] — the access-list split: an address
/// "listed without storage keys" is preload-warm and its first CALL is charged cold, while an
/// address "listed with storage keys" is *loaded* rather than merely preloaded and keeps the
/// inherited warm pricing.
///
/// The sender pays the EIP-2930 per-address cost for the entry either way; without storage keys
/// the first CALL still pays the cold account access cost on top.
#[test]
fn test_access_list_address_without_storage_keys_is_charged_cold() {
    let program = |n| repeated_call(EXISTING_TARGET, n);
    let without_keys =
        || AccessList(vec![AccessListItem { address: EXISTING_TARGET, storage_keys: vec![] }]);
    let with_key = || {
        AccessList(vec![AccessListItem {
            address: EXISTING_TARGET,
            storage_keys: vec![B256::ZERO],
        }])
    };

    let equivalence = first_call_extra_cost(
        |code| transact_with_access_list(MegaSpecId::EQUIVALENCE, base_db(code), without_keys()),
        program,
        |o| o.gas_used,
    );
    assert_eq!(
        equivalence, 0,
        "Equivalence: the inherited EVM honors the access-list preload, so the first and second \
         CALL must cost the same"
    );

    for (spec, spec_name) in crate::ALL_SPECS {
        if !spec.is_enabled(MegaSpecId::MINI_REX) {
            continue;
        }
        let extra_without_keys = first_call_extra_cost(
            |code| transact_with_access_list(spec, base_db(code), without_keys()),
            program,
            |o| o.compute_gas,
        );
        assert_eq!(
            extra_without_keys, COLD_IN_PLACE_OF_WARM,
            "{spec_name}: the first CALL to an access-list address without storage keys must be \
             charged cold in place of warm"
        );

        let extra_with_key = first_call_extra_cost(
            |code| transact_with_access_list(spec, base_db(code), with_key()),
            program,
            |o| o.compute_gas,
        );
        assert_eq!(
            extra_with_key, 0,
            "{spec_name}: an access-list address with storage keys is loaded, not merely \
             preloaded, so its warmth must be honored"
        );
    }
}

/// Spec: [Inherited-Cost Exception: Preload-Warm Addresses] — the third preload-warm address
/// category: the block beneficiary (warmed by the inherited EIP-3651).
///
/// The harness leaves the block environment at its default, so the beneficiary is the zero
/// address. Under Equivalence the first and second CALL to it cost the same (`gas_used` metric —
/// the inherited coinbase warming). From `MiniRex` the first CALL is charged cold in place of
/// warm on every metering spec.
#[test]
fn test_first_call_to_the_beneficiary_is_charged_cold_from_minirex() {
    /// The default `BlockEnv` beneficiary every `transact` run executes under.
    const BENEFICIARY: Address = Address::ZERO;
    let program = |n| repeated_call(BENEFICIARY, n);

    let equivalence = first_call_extra_cost(
        |code| transact(MegaSpecId::EQUIVALENCE, base_db(code)),
        program,
        |o| o.gas_used,
    );
    assert_eq!(
        equivalence, 0,
        "Equivalence: the inherited EVM warms the beneficiary (EIP-3651), so the first and \
         second CALL must cost the same"
    );

    for (spec, spec_name) in crate::ALL_SPECS {
        if !spec.is_enabled(MegaSpecId::MINI_REX) {
            continue;
        }
        let extra =
            first_call_extra_cost(|code| transact(spec, base_db(code)), program, |o| o.compute_gas);
        assert_eq!(
            extra, COLD_IN_PLACE_OF_WARM,
            "{spec_name}: the first CALL to the beneficiary must be charged cold in place of warm"
        );
    }
}

/// Pins "Code Deposit": the deposit's compute gas is recorded exactly once when the deposit
/// occurs, and nothing is recorded when it does not.
///
/// The two initcodes have identical length and opcode sequence and differ only in the byte they
/// store, so every other cost in the transaction cancels and the difference between the two
/// recorded totals is the code-deposit charge alone. `0xEF` makes EIP-3541 reject the runtime
/// code, so the deposit never happens.
///
/// Two different mechanisms produce this number — `MiniRex` through Rex4 measure it over the
/// frame-action window, Rex5+ pre-charge the canonical amount before the checkpoint commits — so
/// the assertion runs on every tracked spec to keep them agreeing.
#[test]
fn test_code_deposit_recorded_only_when_deposit_occurs() {
    /// `PUSH1 <first>, PUSH0, MSTORE8, PUSH1 32, PUSH0, RETURN` — returns 32 bytes of runtime
    /// code whose first byte is `first`.
    fn initcode(first: u8) -> [u8; 8] {
        [0x60, first, 0x5f, 0x53, 0x60, 0x20, 0x5f, 0xf3]
    }

    fn creator(first: u8) -> MemoryDatabase {
        let code = initcode(first);
        base_db(
            BytecodeBuilder::default()
                .mstore(0, code)
                .push_number(code.len() as u64) // length
                .push_number(0_u64) // offset
                .push_number(0_u64) // value
                .append(CREATE)
                .stop()
                .build(),
        )
    }

    /// 32 bytes of deployed runtime code at the inherited `CODEDEPOSIT` rate of 200 gas per byte.
    const EXPECTED_DEPOSIT_GAS: u64 = 32 * 200;

    for (spec, spec_name) in crate::ALL_SPECS {
        if !spec.is_enabled(MegaSpecId::MINI_REX) {
            continue; // Equivalence records no compute gas at all.
        }
        let deposited = transact(spec, creator(0x00));
        let skipped = transact(spec, creator(0xef));
        // Both transactions succeed: the EIP-3541 rejection fails the CREATE (it pushes zero),
        // not the transaction. A non-success outcome means the fixture itself drifted.
        assert_eq!(deposited.outcome, "success", "{spec_name}: depositing run should succeed");
        assert_eq!(skipped.outcome, "success", "{spec_name}: skipped-deposit run should succeed");
        let (deposited, skipped) = (deposited.compute_gas, skipped.compute_gas);

        let delta = deposited.checked_sub(skipped).unwrap_or_else(|| {
            panic!(
                "{spec_name}: depositing run must record at least as much compute gas as the \
                 skipped run (deposited={deposited} skipped={skipped})"
            )
        });
        assert_eq!(
            delta, EXPECTED_DEPOSIT_GAS,
            "{spec_name}: the only compute gas separating a deposit from an EIP-3541 rejection \
             must be the code-deposit charge (deposited={deposited} skipped={skipped})"
        );
    }
}

/// Pins "Transaction Intrinsic Gas" for a creation transaction, which no other test here sends —
/// every other helper dispatches a call transaction, so the creation surcharge and the EIP-3860
/// per-initcode-word charge are otherwise unreachable.
///
/// The recorded intrinsic is asserted as a closed formula over the initcode length rather than as
/// a handful of magic numbers, so the test states which components are in the amount:
/// base cost, the creation surcharge, the calldata token cost, and the per-initcode-word charge.
/// Lengths straddle a word boundary (64 and 65) so a missing or mis-rounded word charge shows up.
///
/// Because the initcode is all zero bytes it executes as `STOP`, depositing no runtime code, so
/// the amount is the intrinsic alone with no opcode or code-deposit contribution mixed in. The
/// code-deposit charge a creation transaction records when it does deploy runtime code is pinned
/// separately by [`test_creation_transaction_records_code_deposit_compute_gas`].
#[test]
fn test_creation_transaction_intrinsic_compute_gas() {
    /// Inherited base cost of any transaction.
    const TX_BASE_GAS: u64 = 21_000;
    /// Inherited surcharge that only a contract-creation transaction pays.
    const TX_CREATE_SURCHARGE: u64 = 32_000;
    /// Gas per calldata token; a zero byte is one token.
    const STANDARD_TOKEN_COST: u64 = 4;
    /// EIP-3860 charge per 32-byte word of initcode.
    const INITCODE_WORD_COST: u64 = 2;

    for (spec, spec_name) in crate::ALL_SPECS {
        if !spec.is_enabled(MegaSpecId::MINI_REX) {
            continue; // Equivalence records no compute gas at all.
        }
        for len in [0_u64, 32, 64, 65] {
            let words = len.div_ceil(32);
            let expected = TX_BASE_GAS +
                TX_CREATE_SURCHARGE +
                STANDARD_TOKEN_COST * len +
                INITCODE_WORD_COST * words;

            let outcome = crate::transact_create(spec, vec![0_u8; len as usize].into());
            assert_eq!(
                outcome.outcome, "success",
                "{spec_name}: creation transaction with {len} initcode bytes should succeed"
            );
            assert_eq!(
                outcome.compute_gas, expected,
                "{spec_name}: a creation transaction with {len} initcode bytes must record base \
                 cost + creation surcharge + calldata tokens + {words} initcode word(s) as \
                 compute gas, and nothing else"
            );
        }
    }
}

/// Spec: [Contract Creation Code Deposit], the contract-creation-transaction leg — the deposit's
/// `code_length × CODEDEPOSIT` compute gas is recorded for a creation *transaction*, not only for
/// the `CREATE` / `CREATE2` opcodes the snapshot corpus exercises.
///
/// Two creation transactions carry byte-identical initcode except for the RETURN size operand:
/// one deploys 32 bytes of zeroed runtime code, the other 64. Both initcodes first MSTORE a zero
/// word at offset 32, expanding memory to 64 bytes, so neither RETURN expands memory further; and
/// both size operands are non-zero bytes, so the calldata token cost is identical too. The only
/// cost separating the two transactions is 32 extra deposited bytes, so the recorded compute gas
/// must differ by exactly 32 × 200 = 6,400 on every metering spec.
#[test]
fn test_creation_transaction_records_code_deposit_compute_gas() {
    /// The inherited EVM's per-byte code deposit rate.
    const CODEDEPOSIT: u64 = 200;
    /// The number of deployed bytes separating the two transactions.
    const EXTRA_RUNTIME_BYTES: u64 = 32;

    /// `PUSH0; PUSH1 0x20; MSTORE; PUSH1 <len>; PUSH0; RETURN` — stores a zero word at offset 32
    /// (expanding memory to 64 bytes) and returns the first `len` bytes of memory, all zero, as
    /// the deployed runtime code.
    fn initcode(runtime_len: u8) -> Bytes {
        Bytes::from(vec![0x5f, 0x60, 0x20, 0x52, 0x60, runtime_len, 0x5f, 0xf3])
    }

    for (spec, spec_name) in crate::ALL_SPECS {
        if !spec.is_enabled(MegaSpecId::MINI_REX) {
            continue; // Equivalence records no compute gas at all.
        }
        let deploys_32 = crate::transact_create(spec, initcode(32));
        let deploys_64 = crate::transact_create(spec, initcode(64));
        assert_eq!(
            deploys_32.outcome, "success",
            "{spec_name}: the 32-byte deployment should succeed"
        );
        assert_eq!(
            deploys_64.outcome, "success",
            "{spec_name}: the 64-byte deployment should succeed"
        );

        let delta =
            deploys_64.compute_gas.checked_sub(deploys_32.compute_gas).unwrap_or_else(|| {
                panic!(
                    "{spec_name}: deploying more runtime code must not record less compute gas \
                     (32B={} 64B={})",
                    deploys_32.compute_gas, deploys_64.compute_gas
                )
            });
        assert_eq!(
            delta,
            EXTRA_RUNTIME_BYTES * CODEDEPOSIT,
            "{spec_name}: the only compute gas separating the two creation transactions must be \
             the code-deposit charge for the {EXTRA_RUNTIME_BYTES} extra deployed bytes \
             (32B={} 64B={})",
            deploys_32.compute_gas,
            deploys_64.compute_gas
        );
    }
}

/// Spec: [Opcode Metering Classes], the `SelfDestruct` class — "When that check finds more than one
/// dimension over its limit, the reported dimension follows the fixed priority" (data size,
/// key-value updates, compute gas, state growth).
///
/// `SELFDESTRUCT` is the one opcode where two dimensions can cross in the same check: it records
/// the empty beneficiary's data-size / KV / state-growth usage before its inner instruction runs
/// and evaluates all four dimensions only in the trailing all-dimension check, together with its
/// compute gas. Tuning both the data-size and the compute-gas limit to be crossed exactly at that
/// opcode therefore makes the priority observable.
///
/// The limits are calibrated from a control program that is byte-identical except that
/// `SELFDESTRUCT` is replaced by `STOP`: setting a dimension's transaction limit to the control's
/// final usage leaves that dimension zero headroom, so whatever `SELFDESTRUCT` adds crosses it.
/// Two single-dimension runs prove each tuned limit alone is crossed at that opcode and reports
/// its own kind; the combined run then must report data size, not compute gas.
///
/// The exceed is frame-local — the top-level frame's budget is the remaining transaction budget —
/// so per "Exceed Behavior" it surfaces as a revert carrying the ABI-encoded
/// `MegaLimitExceeded(uint8 kind, uint64 limit)` payload, whose `kind` is the observable.
#[test]
fn test_data_size_exceed_is_reported_before_compute_gas_at_selfdestruct() {
    let control_db =
        || base_db(BytecodeBuilder::default().push_address(EMPTY_TARGET).stop().build());
    let selfdestruct_db = || {
        base_db(BytecodeBuilder::default().push_address(EMPTY_TARGET).append(SELFDESTRUCT).build())
    };

    // The beneficiary's pre-inner record exists from Rex5.
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        // Calibration: the control's final usage is the usage standing right before SELFDESTRUCT
        // executes, because the two programs are byte-identical up to that opcode, the control
        // ends in a zero-cost STOP that records nothing further, and the harness charges no fees,
        // so no post-execution fee-recipient accounting is merged into the reported totals either.
        let (control, at_opcode) =
            transact_with_limits(spec, control_db(), EvmTxRuntimeLimits::from_spec(spec));
        assert!(control.is_success(), "{spec:?}: control program should succeed");

        // Under the spec's default limits the SELFDESTRUCT run succeeds and consumes more of both
        // dimensions, so each tuned limit below is crossed exactly at that opcode.
        let (relaxed, sd_usage) =
            transact_with_limits(spec, selfdestruct_db(), EvmTxRuntimeLimits::from_spec(spec));
        assert!(relaxed.is_success(), "{spec:?}: SELFDESTRUCT under default limits should succeed");
        assert!(
            sd_usage.data_size > at_opcode.data_size &&
                sd_usage.compute_gas > at_opcode.compute_gas,
            "{spec:?}: SELFDESTRUCT must add usage in both dimensions (data {} -> {}, compute \
             {} -> {})",
            at_opcode.data_size,
            sd_usage.data_size,
            at_opcode.compute_gas,
            sd_usage.compute_gas
        );

        let reported_kind = |limits: EvmTxRuntimeLimits| {
            let (result, _) = transact_with_limits(spec, selfdestruct_db(), limits);
            let ExecutionResult::Revert { output, .. } = result else {
                panic!("{spec:?}: expected a frame-local limit revert, got {result:?}")
            };
            MegaLimitExceeded::abi_decode(&output)
                .unwrap_or_else(|e| {
                    panic!("{spec:?}: revert payload should decode as MegaLimitExceeded: {e}")
                })
                .kind
        };

        // Each tuned limit alone is crossed at SELFDESTRUCT and reports its own dimension.
        assert_eq!(
            reported_kind(
                EvmTxRuntimeLimits::from_spec(spec).with_tx_data_size_limit(at_opcode.data_size)
            ),
            LimitKind::DataSize.as_u8(),
            "{spec:?}: the tuned data-size limit alone must be crossed at SELFDESTRUCT"
        );
        assert_eq!(
            reported_kind(
                EvmTxRuntimeLimits::from_spec(spec)
                    .with_tx_compute_gas_limit(at_opcode.compute_gas)
            ),
            LimitKind::ComputeGas.as_u8(),
            "{spec:?}: the tuned compute-gas limit alone must be crossed at SELFDESTRUCT"
        );

        // Both dimensions cross in the same all-dimension check: the fixed priority puts data
        // size before compute gas, so data size must be the reported dimension.
        assert_eq!(
            reported_kind(
                EvmTxRuntimeLimits::from_spec(spec)
                    .with_tx_data_size_limit(at_opcode.data_size)
                    .with_tx_compute_gas_limit(at_opcode.compute_gas)
            ),
            LimitKind::DataSize.as_u8(),
            "{spec:?}: when the data-size and compute-gas limits are both crossed at the same \
             opcode, the data-size exceed must be the one reported"
        );
    }
}
