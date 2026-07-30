//! Cross-spec compute-gas snapshot.
//!
//! [`docs/spec/evm/compute-gas.md`] defines compute gas as a *derived measurement* of inherited EVM
//! gas: a measurement window opens, the operation runs, and the recorded amount is the EVM gas
//! consumed inside the window minus storage gas and minus gas forwarded to a child frame. Where
//! that window opens and closes is consensus-visible, because the amount recorded when an opcode
//! halts determines which limit is reached first.
//!
//! That makes the recorded amount — not the instruction-table wiring — the thing a refactor of the
//! per-opcode metering layer must preserve. This suite pins it: a corpus covering every metering
//! class, and every non-opcode recording site reachable from a plain transaction, is executed under
//! all nine specs, and the resulting compute-gas readings are compared against a checked-in
//! snapshot.
//!
//! The one recording site the corpus does not reach is `KeylessDeploy` — its dispatch overhead and
//! the Rex5 sandbox-usage merge. Driving it needs a signed RLP payload, and it is already pinned by
//! `tests/rex5/sandbox_accounting.rs`, so it is covered there rather than duplicated here.
//!
//! Within that corpus, any change to a recorded amount on any spec surfaces as a snapshot diff.
//! The corpus runs against minimum-capacity SALT buckets throughout, so recorded amounts that only
//! move once a bucket grows are pinned by the claim tests in `claims.rs` rather than here.
//! Stable specs (Equivalence through Rex5) must never move; a diff there is a replay-breaking
//! regression.
//!
//! Regenerate after an intentional change:
//!
//! ```text
//! UPDATE_COMPUTE_GAS_SNAPSHOT=1 cargo test -p mega-evm --test compute_gas
//! ```

mod claims;

use std::{convert::Infallible, fmt::Write as _, path::Path};

use alloy_eips::eip2930::AccessList;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLCODE, CREATE, CREATE2, DELEGATECALL, LOG0, LOG1, LOG2, LOG3, LOG4, SELFDESTRUCT,
        STATICCALL, TIMESTAMP,
    },
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    handler::EvmTr,
};

/// Transaction sender.
const CALLER: Address = address!("0000000000000000000000000000000000200000");
/// Contract invoked by the transaction; its code exercises the operation under test.
const CONTRACT: Address = address!("0000000000000000000000000000000000200001");
/// An address with no code and no balance — the "empty account" CALL / SELFDESTRUCT target.
const EMPTY_TARGET: Address = address!("0000000000000000000000000000000000200002");
/// An address that already exists (non-zero balance) but has no code.
const EXISTING_TARGET: Address = address!("0000000000000000000000000000000000200003");
/// A contract that simply stops — a CALL target with code, so the callee runs a real frame.
const CALLEE: Address = address!("0000000000000000000000000000000000200004");

/// The identity precompile.
const PRECOMPILE_IDENTITY: Address = address!("0000000000000000000000000000000000000004");
/// The SHA-256 precompile.
const PRECOMPILE_SHA256: Address = address!("0000000000000000000000000000000000000002");
/// The KZG point-evaluation precompile. `MegaETH` replaces it with a fixed-cost variant from
/// `MiniRex` onward, and Rex5 gives it a dedicated compute-gas recording case.
const PRECOMPILE_KZG: Address = address!("000000000000000000000000000000000000000a");

/// Initcode that returns 32 zero bytes as the deployed runtime code: `PUSH1 0x20; PUSH0; RETURN`.
/// Memory in a fresh frame is zero-initialized, so `RETURN(0, 32)` yields 32 bytes of `STOP`.
/// Used to exercise a non-zero code-deposit charge; every other CREATE case deploys empty code.
const INITCODE_RETURNING_32_BYTES: [u8; 4] = [0x60, 0x20, 0x5f, 0xf3];

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// Every spec in the progression, oldest first.
const ALL_SPECS: [(MegaSpecId, &str); 9] = [
    (MegaSpecId::EQUIVALENCE, "Equivalence"),
    (MegaSpecId::MINI_REX, "MiniRex"),
    (MegaSpecId::REX, "Rex"),
    (MegaSpecId::REX1, "Rex1"),
    (MegaSpecId::REX2, "Rex2"),
    (MegaSpecId::REX3, "Rex3"),
    (MegaSpecId::REX4, "Rex4"),
    (MegaSpecId::REX5, "Rex5"),
    (MegaSpecId::REX6, "Rex6"),
];

/// A single corpus entry: a label and the world state it runs against.
pub(crate) struct Program {
    name: &'static str,
    /// Rebuilt per spec so each run starts from identical state.
    build_db: fn() -> MemoryDatabase,
}

/// Post-transaction readings recorded in the snapshot.
struct Outcome {
    compute_gas: u64,
    gas_used: u64,
    outcome: String,
}

/// Runs one transaction calling [`CONTRACT`] under `spec` with the spec's default runtime limits.
fn transact(spec: MegaSpecId, mut db: MemoryDatabase) -> Outcome {
    let mut context =
        MegaContext::new(&mut db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let compute_gas = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    let gas_used = result.result.gas_used();

    let outcome = match &result.result {
        ExecutionResult::Success { .. } => "success".to_string(),
        ExecutionResult::Revert { .. } => "revert".to_string(),
        ExecutionResult::Halt { reason, .. } => format!("halt {reason:?}"),
    };

    Outcome { compute_gas, gas_used, outcome }
}

/// Runs the same transaction as [`transact`] but against a caller-supplied external environment,
/// so a test can raise a SALT bucket above `MIN_BUCKET_SIZE` and make dynamic storage gas non-zero.
fn transact_with_envs(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    envs: TestExternalEnvs<Infallible>,
) -> Outcome {
    let mut context = MegaContext::new(&mut db, spec)
        .with_external_envs(envs.into())
        .with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let compute_gas = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    let gas_used = result.result.gas_used();

    let outcome = match &result.result {
        ExecutionResult::Success { .. } => "success".to_string(),
        ExecutionResult::Revert { .. } => "revert".to_string(),
        ExecutionResult::Halt { reason, .. } => format!("halt {reason:?}"),
    };

    Outcome { compute_gas, gas_used, outcome }
}

/// Runs the same transaction as [`transact`] but attaches an EIP-2930 access list, so a claim
/// test can observe how the transaction-level warm preload interacts with the first CALL-family
/// touch of a listed address.
fn transact_with_access_list(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    access_list: AccessList,
) -> Outcome {
    let mut context =
        MegaContext::new(&mut db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CONTRACT)
        .access_list(access_list)
        .gas_limit(100_000_000)
        .build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let compute_gas = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    let gas_used = result.result.gas_used();

    let outcome = match &result.result {
        ExecutionResult::Success { .. } => "success".to_string(),
        ExecutionResult::Revert { .. } => "revert".to_string(),
        ExecutionResult::Halt { reason, .. } => format!("halt {reason:?}"),
    };

    Outcome { compute_gas, gas_used, outcome }
}

/// Runs the same transaction as [`transact`] but under caller-supplied runtime limits, returning
/// the full execution result — so revert payloads and halt reasons stay observable — together
/// with the post-transaction tracker usage across all four resource dimensions.
fn transact_with_limits(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
) -> (ExecutionResult<MegaHaltReason>, LimitUsage) {
    let mut context = MegaContext::new(&mut db, spec).with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (result.result, usage)
}

/// Runs the same transaction as [`transact`] and returns the transaction's output bytes.
/// Used by the claim tests that read a value the contract computed (e.g. forwarded gas).
fn transact_output(spec: MegaSpecId, mut db: MemoryDatabase) -> Bytes {
    let mut context =
        MegaContext::new(&mut db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    result.result.output().cloned().unwrap_or_default()
}

/// Runs a **contract-creation** transaction carrying `initcode`, rather than the call transaction
/// every other helper here sends.
///
/// The intrinsic-gas rules for a creation transaction differ from those for a call: it carries the
/// inherited creation surcharge and the EIP-3860 per-initcode-word charge on top of the base cost.
/// Those two additions are unreachable through a call transaction, so they need their own entry
/// point to be observable at all.
fn transact_create(spec: MegaSpecId, initcode: Bytes) -> Outcome {
    let mut db = base_db(Bytes::new());
    let mut context =
        MegaContext::new(&mut db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .create()
        .data(initcode)
        .gas_limit(100_000_000)
        .build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let compute_gas = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    let gas_used = result.result.gas_used();

    let outcome = match &result.result {
        ExecutionResult::Success { .. } => "success".to_string(),
        ExecutionResult::Revert { .. } => "revert".to_string(),
        ExecutionResult::Halt { reason, .. } => format!("halt {reason:?}"),
    };

    Outcome { compute_gas, gas_used, outcome }
}

/// Base world: funded caller, the contract under test with `code`, and the CALL fixtures.
fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
        .account_balance(EXISTING_TARGET, U256::from(ONE_ETH))
        .account_code(CALLEE, BytecodeBuilder::default().stop().build())
}

/// Emits the seven CALL operands (retSize, retOffset, argsSize, argsOffset, value, to, gas),
/// leaving `gas` on top as CALL expects.
fn push_call_operands(b: BytecodeBuilder, to: Address, value: u64, gas: u64) -> BytecodeBuilder {
    b.push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(value)
        .push_address(to)
        .push_number(gas)
}

/// Emits the six DELEGATECALL / STATICCALL operands (no value word).
fn push_valueless_call_operands(b: BytecodeBuilder, to: Address, gas: u64) -> BytecodeBuilder {
    b.push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_address(to)
        .push_number(gas)
}

/// The corpus. One entry per metering class and per non-opcode recording site in the spec.
pub(crate) fn corpus() -> Vec<Program> {
    vec![
        // ---- Non-opcode recording site: transaction intrinsic gas ----
        Program {
            // No code at CONTRACT: the only compute gas recorded is the transaction intrinsic.
            name: "intrinsic_only",
            build_db: || {
                MemoryDatabase::default().account_balance(CALLER, U256::from(10 * ONE_ETH))
            },
        },
        // ---- Plain class ----
        Program {
            name: "plain_arithmetic",
            build_db: || {
                let mut b = BytecodeBuilder::default();
                for i in 0..32u64 {
                    b = b.push_number(i).push_number(i + 1).append(0x01).append(0x50); // ADD, POP
                }
                base_db(b.stop().build())
            },
        },
        Program {
            name: "plain_memory_expansion",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .mstore(0, [0x11u8; 32])
                        .mstore(4096, [0x22u8; 32])
                        .stop()
                        .build(),
                )
            },
        },
        // ---- Volatile class ----
        Program {
            name: "volatile_timestamp",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .append(TIMESTAMP)
                        .append(0x50) // POP
                        .stop()
                        .build(),
                )
            },
        },
        // ---- Storage class ----
        Program {
            name: "sstore_zero_to_nonzero",
            build_db: || {
                base_db(
                    BytecodeBuilder::default().sstore(U256::from(7), U256::from(99)).stop().build(),
                )
            },
        },
        Program {
            name: "sstore_nonzero_to_nonzero",
            build_db: || {
                base_db(
                    BytecodeBuilder::default().sstore(U256::from(7), U256::from(99)).stop().build(),
                )
                .account_storage(CONTRACT, U256::from(7), U256::from(1))
            },
        },
        Program {
            name: "sstore_nonzero_to_zero",
            build_db: || {
                base_db(BytecodeBuilder::default().sstore(U256::from(7), U256::ZERO).stop().build())
                    .account_storage(CONTRACT, U256::from(7), U256::from(1))
            },
        },
        Program {
            name: "log0_empty",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .push_number(0u64) // len
                        .push_number(0u64) // offset
                        .append(LOG0)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "log1_32bytes",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .mstore(0, [0x11u8; 32])
                        .push_number(0xabcu64) // topic0
                        .push_number(32u64) // len
                        .push_number(0u64) // offset
                        .append(LOG1)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "log4_128bytes",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .mstore(0, [0x11u8; 32])
                        .mstore(32, [0x22u8; 32])
                        .mstore(64, [0x33u8; 32])
                        .mstore(96, [0x44u8; 32])
                        .push_number(4u64)
                        .push_number(3u64)
                        .push_number(2u64)
                        .push_number(1u64)
                        .push_number(128u64) // len
                        .push_number(0u64) // offset
                        .append(LOG4)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "log2_log3_sequence",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .mstore(0, [0x55u8; 32])
                        .push_number(2u64)
                        .push_number(1u64)
                        .push_number(32u64)
                        .push_number(0u64)
                        .append(LOG2)
                        .push_number(3u64)
                        .push_number(2u64)
                        .push_number(1u64)
                        .push_number(32u64)
                        .push_number(0u64)
                        .append(LOG3)
                        .stop()
                        .build(),
                )
            },
        },
        // ---- Call class ----
        Program {
            name: "call_no_value_to_code",
            build_db: || {
                let b = push_call_operands(BytecodeBuilder::default(), CALLEE, 0, 1_000_000);
                base_db(b.append(CALL).stop().build())
            },
        },
        Program {
            name: "call_value_to_empty",
            build_db: || {
                let b = push_call_operands(BytecodeBuilder::default(), EMPTY_TARGET, 1, 1_000_000);
                base_db(b.append(CALL).stop().build())
            },
        },
        Program {
            name: "call_value_to_existing",
            build_db: || {
                let b =
                    push_call_operands(BytecodeBuilder::default(), EXISTING_TARGET, 1, 1_000_000);
                base_db(b.append(CALL).stop().build())
            },
        },
        Program {
            name: "callcode_value",
            build_db: || {
                let b = push_call_operands(BytecodeBuilder::default(), CALLEE, 1, 1_000_000);
                base_db(b.append(CALLCODE).stop().build())
            },
        },
        Program {
            name: "delegatecall",
            build_db: || {
                let b = push_valueless_call_operands(BytecodeBuilder::default(), CALLEE, 1_000_000);
                base_db(b.append(DELEGATECALL).stop().build())
            },
        },
        Program {
            name: "staticcall",
            build_db: || {
                let b = push_valueless_call_operands(BytecodeBuilder::default(), CALLEE, 1_000_000);
                base_db(b.append(STATICCALL).stop().build())
            },
        },
        Program {
            name: "nested_calls_depth3",
            build_db: || {
                // CONTRACT calls CALLEE_A, which calls CALLEE_B, which stops.
                // Exercises the forwarded-gas exclusion across three frames plus, on Rex4+, the
                // per-frame budget forwarding.
                let inner = push_valueless_call_operands(
                    BytecodeBuilder::default(),
                    EXISTING_TARGET,
                    100_000,
                )
                .append(STATICCALL)
                .stop()
                .build();
                let outer =
                    push_valueless_call_operands(BytecodeBuilder::default(), CALLEE, 1_000_000)
                        .append(STATICCALL)
                        .stop()
                        .build();
                base_db(outer).account_code(CALLEE, inner)
            },
        },
        // ---- Create class ----
        Program {
            name: "create_empty_initcode",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .push_number(0u64) // length
                        .push_number(0u64) // offset
                        .push_number(0u64) // value
                        .append(CREATE)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "create2_empty_initcode",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .push_number(0u64) // salt
                        .push_number(0u64) // length
                        .push_number(0u64) // offset
                        .push_number(0u64) // value
                        .append(CREATE2)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "create2_with_initcode",
            build_db: || {
                // 32 bytes of initcode forces the wrapper-side memory expansion whose recording
                // window moves across MiniRex / Rex5 / Rex6.
                base_db(
                    BytecodeBuilder::default()
                        .mstore(0, [0x00u8; 32])
                        .push_number(0u64) // salt
                        .push_number(32u64) // length
                        .push_number(0u64) // offset
                        .push_number(0u64) // value
                        .append(CREATE2)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "create_deploying_runtime_code",
            build_db: || {
                // The only corpus case that deploys non-empty runtime code, so the only one that
                // exercises the code-deposit recording site (`code_length × CODEDEPOSIT`). Every
                // other CREATE case returns empty code, making that charge zero.
                base_db(
                    BytecodeBuilder::default()
                        .mstore(0, INITCODE_RETURNING_32_BYTES)
                        .push_number(INITCODE_RETURNING_32_BYTES.len() as u64) // length
                        .push_number(0u64) // offset
                        .push_number(0u64) // value
                        .append(CREATE)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "create2_far_memory_offset",
            build_db: || {
                // A large offset makes the memory-expansion gas dominant, so a shift in which
                // window records it is unmistakable in the snapshot.
                base_db(
                    BytecodeBuilder::default()
                        .push_number(0u64) // salt
                        .push_number(32u64) // length
                        .push_number(65_536u64) // offset
                        .push_number(0u64) // value
                        .append(CREATE2)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "create2_oversized_initcode",
            build_db: || {
                // Initcode longer than the max initcode size. Rex5 expands memory and records that
                // gas eagerly, before the inner opcode rejects the size. MiniRex-Rex4 expand too
                // but skip their trailing record once the inner opcode fails, and Rex6 halts on the
                // size check before expanding at all. This is the failure path on which the CREATE2
                // window divergence is observable.
                base_db(
                    BytecodeBuilder::default()
                        .push_number(0u64) // salt
                        .push_number(600_000u64) // length — above MAX_INITCODE_SIZE
                        .push_number(0u64) // offset
                        .push_number(0u64) // value
                        .append(CREATE2)
                        .stop()
                        .build(),
                )
            },
        },
        // ---- SelfDestruct class ----
        Program {
            name: "selfdestruct_to_empty",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .push_address(EMPTY_TARGET)
                        .append(SELFDESTRUCT)
                        .build(),
                )
            },
        },
        Program {
            name: "selfdestruct_to_existing",
            build_db: || {
                base_db(
                    BytecodeBuilder::default()
                        .push_address(EXISTING_TARGET)
                        .append(SELFDESTRUCT)
                        .build(),
                )
            },
        },
        Program {
            name: "selfdestruct_to_self",
            build_db: || {
                base_db(
                    BytecodeBuilder::default().push_address(CONTRACT).append(SELFDESTRUCT).build(),
                )
            },
        },
        // ---- Non-opcode recording site: precompiles ----
        Program {
            name: "precompile_identity",
            build_db: || {
                let b = BytecodeBuilder::default().mstore(0, [0x77u8; 32]);
                base_db(
                    b.push_number(32u64) // retSize
                        .push_number(64u64) // retOffset
                        .push_number(32u64) // argsSize
                        .push_number(0u64) // argsOffset
                        .push_number(0u64) // value
                        .push_address(PRECOMPILE_IDENTITY)
                        .push_number(100_000u64)
                        .append(CALL)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "precompile_sha256",
            build_db: || {
                let b = BytecodeBuilder::default().mstore(0, [0x77u8; 32]);
                base_db(
                    b.push_number(32u64) // retSize
                        .push_number(64u64) // retOffset
                        .push_number(32u64) // argsSize
                        .push_number(0u64) // argsOffset
                        .push_number(0u64) // value
                        .push_address(PRECOMPILE_SHA256)
                        .push_number(100_000u64)
                        .append(CALL)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "precompile_kzg_invalid_input",
            build_db: || {
                // KZG point evaluation with a 32-byte argument (it requires exactly 192),
                // forwarding well above the fixed cost. The precompile reaches its
                // verification step and returns a non-out-of-gas error — the Rex5 KZG branch, which
                // records the fixed `KZG_POINT_EVALUATION_GAS_COST` rather than the spent amount
                // (zero).
                //
                // This is the corpus case that makes MegaETH's KZG override visible: the recorded
                // value must be the MegaETH 100,000, not the inherited EVM 50,000.
                let b = BytecodeBuilder::default().mstore(0, [0x01u8; 32]);
                base_db(
                    b.push_number(0u64) // retSize
                        .push_number(0u64) // retOffset
                        .push_number(32u64) // argsSize — wrong length on purpose
                        .push_number(0u64) // argsOffset
                        .push_number(0u64) // value
                        .push_address(PRECOMPILE_KZG)
                        .push_number(200_000u64) // comfortably above the fixed cost
                        .append(CALL)
                        .stop()
                        .build(),
                )
            },
        },
        Program {
            name: "precompile_underfunded",
            build_db: || {
                // Forwarding less than the precompile's minimum cost: the error path where the
                // recorded amount is the effective gas limit, not the spent amount.
                let b = BytecodeBuilder::default().mstore(0, [0x77u8; 32]);
                base_db(
                    b.push_number(32u64)
                        .push_number(64u64)
                        .push_number(32u64)
                        .push_number(0u64)
                        .push_number(0u64)
                        .push_address(PRECOMPILE_SHA256)
                        .push_number(1u64) // far below the SHA-256 minimum
                        .append(CALL)
                        .stop()
                        .build(),
                )
            },
        },
        // ---- Revert behavior: compute gas must survive a reverted frame ----
        Program {
            name: "reverting_subcall",
            build_db: || {
                // The callee burns gas and reverts. Compute gas is non-revertible, so the caller's
                // total must still include the callee's consumption.
                let mut inner = BytecodeBuilder::default();
                for i in 0..64u64 {
                    inner = inner.push_number(i).push_number(i + 1).append(0x01).append(0x50);
                }
                let inner = inner.revert().build();
                let outer =
                    push_valueless_call_operands(BytecodeBuilder::default(), CALLEE, 1_000_000)
                        .append(STATICCALL)
                        .stop()
                        .build();
                base_db(outer).account_code(CALLEE, inner)
            },
        },
    ]
}

/// Renders the full corpus × spec matrix as the snapshot text.
fn render_snapshot() -> String {
    let mut out = String::new();
    out.push_str("# Cross-spec compute-gas snapshot.\n");
    out.push_str("#\n");
    out.push_str("# Generated by `crates/mega-evm/tests/compute_gas`. Do not edit by hand.\n");
    out.push_str(
        "# Regenerate: UPDATE_COMPUTE_GAS_SNAPSHOT=1 cargo test -p mega-evm --test compute_gas\n",
    );
    out.push_str("#\n");
    out.push_str(
        "# `compute_gas` is the post-transaction compute-gas tracker reading; `gas_used` is the\n",
    );
    out.push_str(
        "# receipt value, which is neither counter nor their sum: refunds lower it, gas consumed\n",
    );
    out.push_str(
        "# outside a completed window raises it, and the calldata floor can raise it further.\n",
    );
    out.push_str("#\n");
    out.push_str(
        "# Stable specs (Equivalence through Rex5) must never change: a diff there is a\n",
    );
    out.push_str("# replay-breaking regression.\n");
    out.push_str("#\n");
    out.push_str(&format!(
        "# {:<30}{:<13}{:>13}{:>13}  {}\n",
        "program", "spec", "compute_gas", "gas_used", "outcome"
    ));

    for program in corpus() {
        out.push('\n');
        for (spec, spec_name) in ALL_SPECS {
            let o = transact(spec, (program.build_db)());
            let _ = writeln!(
                out,
                "{:<32}{:<13}{:>13}{:>13}  {}",
                program.name, spec_name, o.compute_gas, o.gas_used, o.outcome
            );
        }
    }
    out
}

fn snapshot_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compute_gas/snapshot.txt")
}

#[test]
fn test_compute_gas_snapshot_matches() {
    let actual = render_snapshot();
    let path = snapshot_path();

    if std::env::var_os("UPDATE_COMPUTE_GAS_SNAPSHOT").is_some() {
        std::fs::write(&path, &actual).expect("failed to write snapshot");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot at {}: {e}\n\
             Generate it with: UPDATE_COMPUTE_GAS_SNAPSHOT=1 cargo test -p mega-evm --test compute_gas",
            path.display()
        )
    });

    if actual != expected {
        // Report the first differing line to keep the failure readable.
        let diff = expected
            .lines()
            .zip(actual.lines())
            .enumerate()
            .find(|(_, (e, a))| e != a)
            .map(|(i, (e, a))| format!("line {}:\n  expected: {e}\n  actual:   {a}", i + 1))
            .unwrap_or_else(|| "snapshots differ in length".to_string());
        panic!(
            "compute-gas snapshot mismatch.\n{diff}\n\n\
             If this change is intentional, review it against docs/spec/evm/compute-gas.md and \
             regenerate with:\n  UPDATE_COMPUTE_GAS_SNAPSHOT=1 cargo test -p mega-evm --test compute_gas"
        );
    }
}

/// Equivalence does not meter compute gas at all — no instruction wrappers, no precompile
/// recording. This is stated normatively in the spec's Overview; pin it directly so a wrapper
/// accidentally applied to the Equivalence table fails here rather than only shifting a number.
#[test]
fn test_equivalence_records_no_compute_gas() {
    for program in corpus() {
        let o = transact(MegaSpecId::EQUIVALENCE, (program.build_db)());
        assert_eq!(
            o.compute_gas, 0,
            "{}: Equivalence must not record compute gas, got {}",
            program.name, o.compute_gas
        );
    }
}

/// Compute gas is the sole non-revertible resource dimension: a child frame that reverts still
/// contributes its consumption to the transaction total.
#[test]
fn test_compute_gas_survives_reverted_frame() {
    let program = corpus()
        .into_iter()
        .find(|p| p.name == "reverting_subcall")
        .expect("corpus must contain reverting_subcall");

    // A caller-only baseline: same outer code, but the callee returns immediately.
    let baseline_db = || {
        let outer = push_valueless_call_operands(BytecodeBuilder::default(), CALLEE, 1_000_000)
            .append(STATICCALL)
            .stop()
            .build();
        base_db(outer).account_code(CALLEE, BytecodeBuilder::default().stop().build())
    };

    for (spec, spec_name) in ALL_SPECS {
        if spec == MegaSpecId::EQUIVALENCE {
            continue; // no metering
        }
        let with_revert = transact(spec, (program.build_db)());
        let baseline = transact(spec, baseline_db());
        assert!(
            with_revert.compute_gas > baseline.compute_gas,
            "{spec_name}: a reverted subcall's compute gas must still be counted \
             (reverting={} baseline={})",
            with_revert.compute_gas,
            baseline.compute_gas
        );
    }
}
