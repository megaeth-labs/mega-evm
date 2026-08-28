//! A frame init that refuses to build a frame still decides the fate of a whole child budget.
//!
//! `frame_init` hands back a result instead of a frame on several shapes, and the result carries
//! the entire child budget as `remaining`. What happens to that budget is decided by the result's
//! classification alone: a success or a revert is erased back into the caller's gas counter, an
//! exceptional halt is not. The child never runs, so the frame-exit settlement that splits an
//! ordinary exceptional halt never sees the halting shapes — REX7 books them here instead.
//!
//! The classification is what separates the rows, not the reason:
//!
//! | shape                             | result           | class  | destroyed |
//! | --------------------------------- | ---------------- | ------ | --------- |
//! | CREATE past the call-stack limit  | `CallTooDeep`    | revert | 0         |
//! | CREATE whose value exceeds balance| `OutOfFunds`     | revert | 0         |
//! | CREATE from a `u64::MAX` nonce    | `Return`         | ok     | 0         |
//! | CREATE onto an occupied address   | `CreateCollision`| halt   | whole     |
//! | CALL into an account with no code | `Stop`           | ok     | 0         |
//! | CALL past the call-stack limit    | `CallTooDeep`    | revert | 0         |
//! | CALL that dispatches a precompile | precompile's own | either | booked at the precompile site |
//!
//! The precompile row is the one that has to be excluded rather than classified. A precompile is
//! dispatched inside the same frame init and comes back as a result too, but it has already booked
//! both halves of its own split — against the forwarded envelope rather than the capped budget the
//! result carries — so booking it again here would report the same gas twice.
//!
//! Pre-REX7 specs have no destroyed lane, so every row books nothing and the receipts are
//! unchanged.
//!
//! The halting row is then followed through the two boundaries that run after the booking: the
//! failed-deposit receipt rewrite, which settles again against a larger envelope, and the
//! `KeylessDeploy` sandbox merge, which carries a nested execution's split into its parent.

use crate::common::{
    default_envs, transact, transact_default, transact_mega_tx, transact_tx, CALLER, CONTRACT,
    ONE_ETH,
};
use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall as _;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    constants::rex::TX_INTRINSIC_STORAGE_GAS,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    bytecode::opcode::{CREATE, CREATE2, POP, STOP},
    context::tx::TxEnvBuilder,
    handler::{EvmTr, ItemOrResult},
    interpreter::{
        interpreter::SharedMemory, interpreter_action::FrameInit, CallInput, CallInputs,
        CallScheme, CallValue, CreateInputs, CreateScheme, FrameInput, InstructionResult,
    },
    primitives::CALL_STACK_LIMIT,
};

/// The budget every synthetic frame init below forwards to the child it asks for.
const FRAME_GAS: u64 = 100_000;

/// An address seeded with code, so a CREATE aimed at it collides and a CALL into it is not the
/// empty-code shape.
const OCCUPIED: Address = address!("0000000000000000000000000000000000310001");

/// An address with no code and no nonce, so a CALL into it returns `Stop` without a frame.
const VACANT: Address = address!("0000000000000000000000000000000000310002");

/// blake2f. Rejects any input whose length is not 213 bytes, before charging anything — a
/// precompile halt with nothing performed.
const BLAKE2F: Address = address!("0000000000000000000000000000000000000009");

/// The two specs every row is run under: the one with the destroyed lane, and the frozen one
/// directly beneath it.
const SPECS: [MegaSpecId; 2] = [MegaSpecId::REX6, MegaSpecId::REX7];

/* ------------------------------------------------------------------------------------------- *
 * The state table, driven at the `frame_init` boundary.
 * ------------------------------------------------------------------------------------------- */

/// A `frame_init` that asks for a CREATE child.
fn create_frame_init(value: U256, depth: usize) -> FrameInit {
    FrameInit {
        depth,
        memory: SharedMemory::new(),
        frame_input: FrameInput::Create(Box::new(CreateInputs::new(
            CALLER,
            CreateScheme::Create,
            value,
            Bytes::new(),
            FRAME_GAS,
            0,
        ))),
    }
}

/// A `frame_init` that asks for a CALL child.
fn call_frame_init(target: Address, input: Bytes, depth: usize) -> FrameInit {
    FrameInit {
        depth,
        memory: SharedMemory::new(),
        frame_input: FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(input),
            return_memory_offset: 0..0,
            gas_limit: FRAME_GAS,
            bytecode_address: target,
            target_address: target,
            caller: CALLER,
            // Apparent rather than Transfer: a zero-value transfer would still touch the target
            // account, and these synthetic frame inits run against a journal that has loaded
            // nothing. The rows under test are all decided before any value would move.
            value: CallValue::Apparent(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            reservoir: 0,
            known_bytecode: Default::default(),
            charged_new_account_state_gas: false,
        })),
    }
}

/// What one `frame_init` row produced: the classification it returned, the budget the result still
/// carries, and the destroyed total the tracker booked for it.
struct Row {
    instruction_result: InstructionResult,
    remaining: u64,
    booked_destroyed: u64,
}

/// Drives `frame_init` once against a fresh EVM and reads back the row.
fn run_frame_init(spec: MegaSpecId, mut db: MemoryDatabase, frame_init: FrameInit) -> Row {
    let context = MegaContext::new(&mut db, spec);
    let mut evm = MegaEvm::new(context);
    let result = EvmTr::frame_init(&mut evm, frame_init).expect("frame_init must not error");
    let ItemOrResult::Result(frame_result) = result else {
        panic!("{spec:?}: this shape must reject the frame, not build one");
    };
    let booked_destroyed = evm.ctx_ref().additional_limit.borrow().conservation_terms_for_test().2;
    Row {
        instruction_result: frame_result.instruction_result(),
        remaining: frame_result.gas().remaining(),
        booked_destroyed,
    }
}

/// The database every row starts from: a funded caller, an occupied address, and blake2f reachable
/// as a precompile.
fn row_db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(ONE_ETH))
        .account_code(OCCUPIED, BytecodeBuilder::default().append(STOP).build())
}

/// Every `frame_init` rejection classified as a success or a revert hands its budget back, so it
/// must book nothing — on both specs.
#[test]
fn test_returned_frame_init_rejections_book_nothing() {
    let cases: Vec<(&str, MemoryDatabase, FrameInit, InstructionResult)> = vec![
        (
            "CREATE past the call-stack limit",
            row_db(),
            create_frame_init(U256::ZERO, CALL_STACK_LIMIT as usize + 1),
            InstructionResult::CallTooDeep,
        ),
        (
            "CREATE whose value exceeds the caller's balance",
            row_db(),
            create_frame_init(U256::from(2 * ONE_ETH), 1),
            InstructionResult::OutOfFunds,
        ),
        (
            "CREATE from a caller whose nonce cannot be bumped",
            row_db().account_nonce(CALLER, u64::MAX),
            create_frame_init(U256::ZERO, 1),
            InstructionResult::Return,
        ),
        (
            "CALL into an account with no code",
            row_db(),
            call_frame_init(VACANT, Bytes::new(), 1),
            InstructionResult::Stop,
        ),
        (
            "CALL past the call-stack limit",
            row_db(),
            // The REX5 depth guard covers `Call` / `StaticCall`; a `CallCode` reaches revm's own
            // depth check, which is the arm under test here.
            FrameInit {
                depth: CALL_STACK_LIMIT as usize + 1,
                memory: SharedMemory::new(),
                frame_input: match call_frame_init(
                    OCCUPIED,
                    Bytes::new(),
                    CALL_STACK_LIMIT as usize + 1,
                )
                .frame_input
                {
                    FrameInput::Call(mut inputs) => {
                        inputs.scheme = CallScheme::CallCode;
                        FrameInput::Call(inputs)
                    }
                    other => other,
                },
            },
            InstructionResult::CallTooDeep,
        ),
    ];

    for (label, db, frame_init, expected) in cases {
        for spec in SPECS {
            let row = run_frame_init(spec, db.clone(), clone_frame_init(&frame_init));
            assert_eq!(
                row.instruction_result, expected,
                "{label} ({spec:?}): unexpected classification",
            );
            assert!(
                row.instruction_result.is_ok_or_revert(),
                "{label} ({spec:?}): this row is only meaningful while the shape stays \
                 non-halting",
            );
            assert_eq!(
                row.remaining, FRAME_GAS,
                "{label} ({spec:?}): the whole budget must be handed back to the caller",
            );
            assert_eq!(
                row.booked_destroyed, 0,
                "{label} ({spec:?}): gas that returns to the caller is not destroyed",
            );
        }
    }
}

/// A CREATE onto an occupied address is the one `frame_init` rejection whose budget the caller
/// never sees again, so REX7 books the whole thing as destroyed and REX6 books nothing.
#[test]
fn test_create_collision_books_the_whole_swallowed_budget() {
    for spec in SPECS {
        let created = CALLER.create(0);
        let db = row_db().account_code(created, BytecodeBuilder::default().append(STOP).build());
        let row = run_frame_init(spec, db, create_frame_init(U256::ZERO, 1));

        assert_eq!(
            row.instruction_result,
            InstructionResult::CreateCollision,
            "{spec:?}: the shape under test must be a collision",
        );
        assert!(
            !row.instruction_result.is_ok_or_revert(),
            "{spec:?}: a collision is an exceptional halt, which is why the budget is lost",
        );
        assert_eq!(
            row.remaining, FRAME_GAS,
            "{spec:?}: the result carries the whole child budget it is about to swallow",
        );
        let expected = if spec.is_enabled(MegaSpecId::REX7) { FRAME_GAS } else { 0 };
        assert_eq!(
            row.booked_destroyed, expected,
            "{spec:?}: the swallowed budget must be booked exactly once on the destroyed lane",
        );
    }
}

/// A precompile comes back through the same arm, but it books its own split at the recording site.
/// Booking again here would double it, so the total must stay one forwarded envelope.
#[test]
fn test_precompile_result_is_not_booked_a_second_time() {
    // 32 bytes: not blake2f's 213, so it is rejected before any work and halts.
    let malformed = Bytes::from(vec![0xAAu8; 32]);
    for spec in SPECS {
        let row = run_frame_init(spec, row_db(), call_frame_init(BLAKE2F, malformed.clone(), 1));

        assert_eq!(
            row.instruction_result,
            InstructionResult::PrecompileError,
            "{spec:?}: the probe must reach the precompile and halt inside it",
        );
        let expected = if spec.is_enabled(MegaSpecId::REX7) { FRAME_GAS } else { 0 };
        assert_eq!(
            row.booked_destroyed,
            expected,
            "{spec:?}: the precompile's own recording site books the forwarded envelope once; \
             a second booking at the frame-init arm would report {} here",
            2 * FRAME_GAS,
        );
    }
}

/// `FrameInit` is not `Clone`, and each row is run once per spec.
fn clone_frame_init(frame_init: &FrameInit) -> FrameInit {
    FrameInit {
        depth: frame_init.depth,
        memory: SharedMemory::new(),
        frame_input: frame_init.frame_input.clone(),
    }
}

/* ------------------------------------------------------------------------------------------- *
 * The same rejections reached through real transactions.
 * ------------------------------------------------------------------------------------------- */

/// The transaction gas limit the end-to-end collision cases run with.
const TX_GAS_LIMIT: u64 = 1_000_000;

/// Standard EVM intrinsic gas for a creation transaction with empty init code: 21,000 plus the
/// 32,000 creation surcharge. Empty init code adds neither calldata nor EIP-3860 word cost.
const CREATE_INTRINSIC_COMPUTE: u64 = 53_000;

/// A creation transaction from [`CALLER`] with empty init code, aimed at whatever
/// `CALLER.create(0)` resolves to.
fn colliding_create_tx(gas_limit: u64) -> revm::context::TxEnv {
    TxEnvBuilder::default()
        .caller(CALLER)
        .kind(TxKind::Create)
        .gas_limit(gas_limit)
        .gas_price(0)
        .data(Bytes::new())
        .build_fill()
}

/// A funded caller whose first creation address is already occupied.
fn colliding_db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(ONE_ETH))
        .account_code(CALLER.create(0), BytecodeBuilder::default().append(STOP).build())
}

/// A transaction that is nothing but a colliding creation destroys everything past its intrinsic
/// cost, and the receipt is unchanged from the frozen spec's.
#[test]
fn test_top_level_create_collision_destroys_the_rest_of_the_envelope() {
    let run = |spec| {
        transact_tx(
            spec,
            colliding_db(),
            EvmTxRuntimeLimits::from_spec(spec),
            colliding_create_tx(TX_GAS_LIMIT),
            &default_envs(),
        )
    };
    let r6 = run(MegaSpecId::REX6);
    let r7 = run(MegaSpecId::REX7);

    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "the collision halt itself must be unchanged",
    );
    assert_eq!(r6.gas_used, TX_GAS_LIMIT, "an exceptional halt spends the whole envelope");
    assert_eq!(r7.gas_used, r6.gas_used, "receipt gas_used must be unchanged");

    // Everything the transaction spent is either compute or the flat intrinsic storage gas: the
    // collision creates no account, so nothing else is charged.
    assert_eq!(
        r7.compute_gas,
        TX_GAS_LIMIT - TX_INTRINSIC_STORAGE_GAS,
        "REX7 must report the whole envelope less its intrinsic storage gas as compute",
    );
    assert_eq!(
        r7.enforced(),
        CREATE_INTRINSIC_COMPUTE,
        "only the intrinsic compute was ever performed, so only it may enforce",
    );
    assert_eq!(
        r7.destroyed,
        TX_GAS_LIMIT - TX_INTRINSIC_STORAGE_GAS - CREATE_INTRINSIC_COMPUTE,
        "the rest of the envelope is what the refused frame swallowed",
    );
    assert_eq!(
        r7.booked_destroyed, r7.destroyed,
        "the per-site booking and the conservation law must agree",
    );

    assert_eq!(
        r6.compute_gas, CREATE_INTRINSIC_COMPUTE,
        "REX6 attributes nothing to a frame that never ran",
    );
    assert_eq!(r6.destroyed, 0, "REX6 has no destroyed lane");
    assert_eq!(
        r7.enforced(),
        r6.compute_gas,
        "the enforcing lane is byte-identical across the two specs",
    );
}

/// Two CREATE2s with the same salt and the same (empty) init code: the first deploys, the second
/// collides with it.
fn colliding_create2_code() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64) // salt
        .push_number(0u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE2)
        .append(POP)
        .push_number(0u64) // salt
        .push_number(0u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE2)
        .append(POP)
        .append(STOP)
        .build()
}

/// The reported repro: two CREATE2s with the same salt and the same init code, the second of which
/// collides. The caller survives it, so this also pins that a swallowed inner budget is booked
/// without the surrounding frame noticing.
#[test]
fn test_inner_create2_collision_destroys_the_forwarded_budget() {
    let code = colliding_create2_code();
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code);

    let r6 = transact_default(MegaSpecId::REX6, db.clone());
    let r7 = transact_default(MegaSpecId::REX7, db);

    assert!(r7.is_success(), "the caller absorbs the failed CREATE2 and stops: {:?}", r7.result);
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "the caller's own result must be unchanged",
    );
    assert_eq!(r6.gas_used, r7.gas_used, "receipt gas_used must be unchanged");

    assert!(
        r7.destroyed > 0,
        "the colliding CREATE2's forwarded budget is swallowed and must be booked",
    );
    assert_eq!(
        r7.booked_destroyed, r7.destroyed,
        "the per-site booking and the conservation law must agree",
    );
    assert_eq!(
        r7.enforced(),
        r6.compute_gas,
        "the enforcing lane is byte-identical across the two specs",
    );
    assert_eq!(
        r7.compute_gas,
        r6.compute_gas + r7.destroyed,
        "REX7 reports exactly what REX6 reported plus the swallowed budget",
    );
}

/// A CREATE whose value exceeds the caller's balance is a revert: its budget comes back, so
/// nothing is destroyed and the two specs report the same compute total.
#[test]
fn test_inner_create_out_of_funds_destroys_nothing() {
    let code = BytecodeBuilder::default()
        .push_number(0u64) // size
        .push_number(0u64) // offset
        .push_number(2 * ONE_ETH as u64) // value, above the contract's balance
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build();
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH));

    let r6 = transact_default(MegaSpecId::REX6, db.clone());
    let r7 = transact_default(MegaSpecId::REX7, db);

    assert!(r7.is_success(), "the caller absorbs the failed CREATE: {:?}", r7.result);
    assert_eq!(r6.gas_used, r7.gas_used, "receipt gas_used must be unchanged");
    assert_eq!(r7.destroyed, 0, "an OutOfFunds create hands its budget back");
    assert_eq!(r7.booked_destroyed, 0, "and so books nothing");
    assert_eq!(
        r7.compute_gas, r6.compute_gas,
        "with nothing destroyed the two specs report the same compute total",
    );
}

/// A CREATE from an account whose nonce cannot be bumped reports success and hands its budget
/// back, so it books nothing either.
#[test]
fn test_inner_create_nonce_overflow_destroys_nothing() {
    let code = BytecodeBuilder::default()
        .push_number(0u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build();
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_nonce(CONTRACT, u64::MAX);

    let r6 = transact_default(MegaSpecId::REX6, db.clone());
    let r7 = transact_default(MegaSpecId::REX7, db);

    assert!(r7.is_success(), "the caller survives the refused CREATE: {:?}", r7.result);
    assert_eq!(r6.gas_used, r7.gas_used, "receipt gas_used must be unchanged");
    assert_eq!(r7.destroyed, 0, "a nonce-overflow create hands its budget back");
    assert_eq!(r7.booked_destroyed, 0, "and so books nothing");
    assert_eq!(
        r7.compute_gas, r6.compute_gas,
        "with nothing destroyed the two specs report the same compute total",
    );
}

/// A precompile that halts is booked once, by its own recording site. Running it alongside the
/// frame-init arm must not double the destroyed total.
#[test]
fn test_precompile_halt_stays_booked_once_end_to_end() {
    let malformed = vec![0xAAu8; 32];
    let forwarded: u64 = 200_000;
    let code = BytecodeBuilder::default()
        .mstore(0, &malformed)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(malformed.len() as u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(BLAKE2F)
        .push_number(forwarded)
        .append(revm::bytecode::opcode::CALL)
        .append(POP)
        .append(STOP)
        .build();
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code);

    let r7 = transact(MegaSpecId::REX7, db, EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7));

    assert!(r7.is_success(), "the caller absorbs the precompile failure: {:?}", r7.result);
    assert_eq!(
        r7.destroyed,
        forwarded,
        "the forwarded envelope is destroyed exactly once; a second booking at the frame-init \
         arm would report about {} here",
        2 * forwarded,
    );
    assert_eq!(
        r7.booked_destroyed, r7.destroyed,
        "the per-site booking and the conservation law must agree",
    );
}

/* ------------------------------------------------------------------------------------------- *
 * The rewritten-envelope boundary.
 * ------------------------------------------------------------------------------------------- */

/// Sender of the deposit transaction.
const DEPOSIT_CALLER: Address = address!("0000000000000000000000000000000000310003");

/// A colliding creation, sent as an OP deposit.
///
/// A failed deposit's receipt is rebuilt to report the whole gas limit after every settlement has
/// run, and the boundary that rebuilds it books the difference as destroyed. This transaction
/// books at both places, so it is where a double count between them would show up: the total must
/// still be the whole envelope less the work the transaction actually performed.
#[test]
fn test_failed_deposit_whose_create_collides_books_the_envelope_once() {
    let gas_limit = TX_GAS_LIMIT;
    let db = MemoryDatabase::default()
        .account_balance(DEPOSIT_CALLER, U256::from(ONE_ETH))
        .account_code(DEPOSIT_CALLER.create(0), BytecodeBuilder::default().append(STOP).build());
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default()
            .caller(DEPOSIT_CALLER)
            .kind(TxKind::Create)
            .gas_limit(gas_limit)
            .gas_price(0)
            .data(Bytes::new())
            .build_fill(),
    );
    tx.deposit.source_hash = B256::repeat_byte(0x42);
    tx.enveloped_tx = Some(Bytes::new());

    let r7 = transact_mega_tx(
        MegaSpecId::REX7,
        db,
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7),
        tx,
        &TestExternalEnvs::default(),
    );

    let rendered = format!("{:?}", r7.halt_reason("deposit"));
    assert!(
        rendered.contains("FailedDeposit"),
        "a failed deposit must be reported as FailedDeposit, got {rendered}",
    );
    assert_eq!(r7.gas_used, gas_limit, "a failed deposit's receipt reports the whole gas limit");
    assert_eq!(
        r7.enforced(),
        CREATE_INTRINSIC_COMPUTE,
        "only the intrinsic compute was performed, and the rewrite must not change that",
    );
    assert_eq!(
        r7.destroyed,
        gas_limit - TX_INTRINSIC_STORAGE_GAS - CREATE_INTRINSIC_COMPUTE,
        "the rewritten envelope, less what was performed, is destroyed exactly once",
    );
    assert_eq!(
        r7.booked_destroyed, r7.destroyed,
        "the per-site bookings and the conservation law must agree after the rewrite too",
    );
}

/* ------------------------------------------------------------------------------------------- *
 * The nested-execution boundary.
 * ------------------------------------------------------------------------------------------- */

/// The inner keyless transaction's gas limit — enough for its constructor to run both creations.
const KEYLESS_INNER_GAS: u64 = 400_000;

/// A deterministic pre-EIP-155 creation transaction, wrapped in a `keylessDeploy` call.
///
/// Its constructor runs [`colliding_create2_code`], so the collision happens inside the
/// `KeylessDeploy` sandbox rather than in the outer transaction's own frames.
fn keyless_deploy_calldata() -> Bytes {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: KEYLESS_INNER_GAS,
        to: TxKind::Create,
        value: U256::ZERO,
        input: colliding_create2_code(),
        chain_id: None,
    };
    let word = U256::from_be_bytes(hex!(
        "3333333333333333333333333333333333333333333333333333333333333333"
    ));
    let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    Bytes::from(
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: Bytes::from(buf),
            gasLimitOverride: U256::from(KEYLESS_INNER_GAS),
        }
        .abi_encode(),
    )
}

/// A keyless deployment whose constructor collides with itself books the swallowed budget in the
/// sandbox's own tracker, and the merge has to carry it into the outer transaction: the outer
/// transaction reports it and still enforces only the work performed.
#[test]
fn test_keyless_sandbox_create_collision_crosses_the_merge_boundary() {
    let run = |spec| {
        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000 * ONE_ETH));
        let tx = TxEnvBuilder::default()
            .caller(CALLER)
            .call(KEYLESS_DEPLOY_ADDRESS)
            .gas_limit(2_000_000u64)
            .chain_id(Some(1))
            .data(keyless_deploy_calldata())
            .build_fill();
        transact_tx(spec, db, EvmTxRuntimeLimits::from_spec(spec), tx, &default_envs())
    };
    let r6 = run(MegaSpecId::REX6);
    let r7 = run(MegaSpecId::REX7);

    assert!(r7.is_success(), "the sandbox deployment must still succeed: {:?}", r7.result);
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "the outer transaction's own result must be unchanged",
    );
    assert_eq!(r6.gas_used, r7.gas_used, "receipt gas_used must be unchanged");
    assert!(
        r7.destroyed > 0,
        "the sandbox frame the collision refused swallowed its budget, and the merge must carry \
         that across",
    );
    assert_eq!(
        r7.booked_destroyed, r7.destroyed,
        "the per-site booking and the conservation law must agree across the merge",
    );
    assert_eq!(
        r7.enforced(),
        r6.compute_gas,
        "the enforcing lane is byte-identical across the two specs",
    );
    assert_eq!(
        r7.compute_gas,
        r6.compute_gas + r7.destroyed,
        "the outer transaction reports exactly what REX6 reported plus the swallowed budget",
    );
}
