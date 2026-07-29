//! Direct assertions for the normative claims in `docs/spec/evm/compute-gas.md` that the
//! cross-spec snapshot cannot pin on its own.
//!
//! The snapshot in [`super`] records *numbers*; it detects that a value moved but does not state
//! why the value is what it is. These tests assert the rules behind the numbers, so a refactor
//! that happens to preserve a snapshot value while breaking the underlying rule still fails.
//!
//! Each test names the spec section it pins.

use std::convert::Infallible;

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    IMegaLimitControl, MegaSpecId, SaltEnv, TestExternalEnvs, LIMIT_CONTROL_ADDRESS,
    MIN_BUCKET_SIZE,
};
use revm::bytecode::opcode::{
    CALL, GAS, MSTORE, POP, PUSH0, RETURN, RETURNDATACOPY, SELFDESTRUCT, STATICCALL,
};

use crate::{
    base_db, transact, transact_output, transact_with_envs, CALLEE, CALLER, CONTRACT, EMPTY_TARGET,
    EXISTING_TARGET, ONE_ETH,
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
/// The constant is a **`MegaETH` override**, not the inherited EVM value. This test pins the number
/// itself rather than merely its presence: on the KZG non-out-of-gas error path, Rex4 records the
/// spent amount (zero, because the precompile never charged) while Rex5 records the fixed cost.
/// The difference between the two specs is therefore exactly the constant.
///
/// Had this test existed earlier, the spec page could not have shipped the inherited 50,000 in
/// place of `MegaETH`'s 100,000.
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
