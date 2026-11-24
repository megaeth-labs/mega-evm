//! REX6 frame-local accounting fixes, each with a REX5 freeze guard:
//!
//! - per-log base overhead in `after_log`: an empty `LOG0` is no longer free in the `data_size`
//!   lane (pre-REX6 charges no per-log base);
//! - a compute-limit halt at a CALL/CREATE — whether the frame-local `Revert` or the tx-level
//!   detained `OutOfGas` branch — returns the forwarded gas to the parent instead of swallowing it
//!   into `gas_used` (pre-REX6 leaves it swallowed).

use crate::common::{transact, transact_default, CALLER, CONTRACT};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, LOG0, LOG1, POP, TIMESTAMP},
    context::result::ExecutionResult,
};

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// The per-log base overhead added under REX6: one 32-byte value unit for the log address
/// persisted in every receipt log object regardless of topic count or data length. Mirrors the
/// `mega_evm::limit::data_size::LOG_BASE_SIZE` constant introduced by the fix.
const LOG_BASE_SIZE: u64 = 32;

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

// ---------------------------------------------------------------------------
// Per-log base overhead
// ---------------------------------------------------------------------------

/// `LOG0(0,0)` bytecode: push len=0, push offset=0, LOG0, STOP.
fn log0_empty_code() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64) // len
        .push_number(0u64) // offset
        .append(LOG0)
        .stop()
        .build()
}

#[test]
fn test_rex6_log0_empty_charges_base_overhead_vs_rex5() {
    let code = log0_empty_code();
    let r5 = transact_default(MegaSpecId::REX5, base_db(code.clone()));
    let r6 = transact_default(MegaSpecId::REX6, base_db(code));
    assert!(r5.is_success() && r6.is_success(), "both must succeed");
    // REX6 charges exactly one per-log base overhead more than the frozen REX5 reading.
    assert_eq!(
        r6.data_size,
        r5.data_size + LOG_BASE_SIZE,
        "REX6 LOG0(0,0) must add LOG_BASE_SIZE ({LOG_BASE_SIZE}) over REX5; r5={} r6={}",
        r5.data_size,
        r6.data_size,
    );
}

#[test]
fn test_rex5_log0_empty_is_free_frozen() {
    // Freeze guard: under REX5, LOG0(0,0) contributes ZERO data_size beyond the no-log baseline
    // (preserved byte-for-byte pre-REX6).
    let with_log = transact_default(MegaSpecId::REX5, base_db(log0_empty_code()));
    let no_log =
        transact_default(MegaSpecId::REX5, base_db(BytecodeBuilder::default().stop().build()));
    assert_eq!(
        with_log.data_size, no_log.data_size,
        "REX5 LOG0(0,0) must remain free in data_size (frozen); with_log={} no_log={}",
        with_log.data_size, no_log.data_size,
    );
}

#[test]
fn test_rex6_log1_with_data_base_plus_topics_plus_data() {
    // LOG1 with 32 bytes of data: REX6 = REX5 + LOG_BASE_SIZE (one log → one base overhead); the
    // topic (32) + data (32) component is identical across specs.
    let code = BytecodeBuilder::default()
        .mstore(0, [0x11u8; 32])
        .push_number(0xabcu64) // topic0
        .push_number(32u64) // len
        .push_number(0u64) // offset
        .append(LOG1)
        .stop()
        .build();
    let r5 = transact_default(MegaSpecId::REX5, base_db(code.clone()));
    let r6 = transact_default(MegaSpecId::REX6, base_db(code));
    assert!(r5.is_success() && r6.is_success(), "both must succeed");
    assert_eq!(
        r6.data_size,
        r5.data_size + LOG_BASE_SIZE,
        "REX6 LOG1 must add exactly one LOG_BASE_SIZE over REX5; r5={} r6={}",
        r5.data_size,
        r6.data_size,
    );
}

// ---------------------------------------------------------------------------
// Forwarded gas returned on a frame-local compute-limit abort at a CALL
// ---------------------------------------------------------------------------

/// A CALL target that has code (a bare STOP) so revm builds a child `NewFrame` and forwards gas
/// to it — the precondition for the gas swallow. Distinct from `EMPTY_TARGET` (no code), which
/// resolves immediately with no pending frame.
const CALL_TARGET: alloy_primitives::Address =
    alloy_primitives::address!("0000000000000000000000000000000000200003");

/// DB where [`CONTRACT`] performs a value-free CALL to a code-bearing target, forwarding (almost)
/// all remaining gas. The CALL is the first compute-significant opcode.
fn compute_abort_db() -> MemoryDatabase {
    let call_code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALL_TARGET) // to
        .push_number(100_000_000u64) // gas (revm caps to 98/100 of remaining)
        .append(CALL)
        .stop()
        .build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, call_code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
        .account_code(CALL_TARGET, BytecodeBuilder::default().stop().build())
}

/// `no_limits()` except a tight compute-gas budget that the CALL's own compute overhead (cold
/// account access) overruns — firing a frame-local compute-limit halt while the child `NewFrame`
/// is pending and its forwarded gas already deducted.
fn tight_compute_limits() -> EvmTxRuntimeLimits {
    let mut limits = EvmTxRuntimeLimits::no_limits();
    limits.tx_compute_gas_limit = 22_000;
    limits
}

#[test]
fn test_rex6_compute_abort_at_call_returns_forwarded_gas() {
    let r5 = transact(MegaSpecId::REX5, compute_abort_db(), tight_compute_limits());
    let r6 = transact(MegaSpecId::REX6, compute_abort_db(), tight_compute_limits());
    // Both halt (frame-local compute-limit Revert) at the CALL.
    assert!(!r5.is_success(), "REX5 must halt on the compute limit; got {:?}", r5.result);
    assert!(!r6.is_success(), "REX6 must halt on the compute limit; got {:?}", r6.result);
    // REX5 swallows the forwarded gas (~98% of the 100M tx limit) → gas_used inflated.
    assert!(
        r5.gas_used > 50_000_000,
        "REX5 must swallow the forwarded gas (frozen bug); gas_used={}",
        r5.gas_used,
    );
    // REX6 erases the forwarded gas back to the parent before halting → gas_used stays small.
    assert!(
        r6.gas_used < 1_000_000,
        "REX6 must return the forwarded gas (not swallow it); gas_used={}",
        r6.gas_used,
    );
}

/// DB where [`CONTRACT`] performs a `CREATE` with a one-byte init code, forwarding gas to the init
/// frame. The `CREATE` is the first compute-significant opcode, so its compute overhead overruns
/// the tight budget while the child `NewFrame(Create)` is pending and its forwarded gas deducted.
fn compute_abort_create_db() -> MemoryDatabase {
    // memory[0] = 0x00 (STOP) → a one-byte init code that deploys empty runtime.
    let create_code = BytecodeBuilder::default()
        .mstore(0, [0x00u8; 32])
        .push_number(1u64) // init code size
        .push_number(0u64) // init code offset
        .push_number(0u64) // value
        .append(CREATE)
        .stop()
        .build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, create_code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

#[test]
fn test_rex6_compute_abort_at_create_returns_forwarded_gas() {
    let r5 = transact(MegaSpecId::REX5, compute_abort_create_db(), tight_compute_limits());
    let r6 = transact(MegaSpecId::REX6, compute_abort_create_db(), tight_compute_limits());
    // Both halt (frame-local compute-limit Revert) at the CREATE.
    assert!(!r5.is_success(), "REX5 must halt on the compute limit; got {:?}", r5.result);
    assert!(!r6.is_success(), "REX6 must halt on the compute limit; got {:?}", r6.result);
    // REX5 swallows the gas forwarded to the discarded init frame → gas_used inflated.
    assert!(
        r5.gas_used > 50_000_000,
        "REX5 must swallow the forwarded gas (frozen bug); gas_used={}",
        r5.gas_used,
    );
    // REX6 erases the forwarded gas back to the parent before halting → gas_used stays small.
    assert!(
        r6.gas_used < 1_000_000,
        "REX6 must return the forwarded gas (not swallow it); gas_used={}",
        r6.gas_used,
    );
}

/// DB where [`CONTRACT`] accesses volatile block env (`TIMESTAMP`) to arm a relative compute-gas
/// detention cap, then performs a gas-forwarding CALL to a code-bearing target. With a tight
/// detention cap and a large per-frame budget, the CALL's own compute overshoots the *tx-level*
/// (detained) compute limit while the frame-local budget still has room — so the compute halt is
/// the tx-level `OutOfGas` branch, not the frame-local `Revert` branch exercised above.
fn tx_level_detained_abort_db() -> MemoryDatabase {
    let code = BytecodeBuilder::default()
        .append(TIMESTAMP) // volatile access → arms the relative detention cap
        .append(POP)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALL_TARGET) // to
        .push_number(100_000_000u64) // gas (revm caps to 98/100 of remaining)
        .append(CALL)
        .stop()
        .build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
        .account_code(CALL_TARGET, BytecodeBuilder::default().stop().build())
}

/// A huge per-frame compute budget (so the frame-local limit never binds) paired with a tight
/// detention cap (so after the volatile access the tx-level detained limit is what the CALL trips).
fn tx_level_detention_limits() -> EvmTxRuntimeLimits {
    let mut limits = EvmTxRuntimeLimits::no_limits();
    limits.tx_compute_gas_limit = 100_000_000;
    limits.block_env_access_compute_gas_limit = 1_000;
    limits
}

/// The forwarded gas is returned to the parent on a *tx-level* (detained) compute-limit halt at a
/// CALL, not only on the frame-local one: a child `NewFrame` whose gas revm already deducted is
/// discarded when the detained `OutOfGas` fires, so without the erase that gas is swallowed into
/// `gas_used`. REX6 returns it; pre-REX6 leaves it swallowed.
#[test]
fn test_rex6_tx_level_compute_abort_at_call_returns_forwarded_gas() {
    let r5 = transact(MegaSpecId::REX5, tx_level_detained_abort_db(), tx_level_detention_limits());
    let r6 = transact(MegaSpecId::REX6, tx_level_detained_abort_db(), tx_level_detention_limits());
    // Both halt at the tx-level detained limit (a Halt, distinct from the frame-local Revert).
    assert!(
        matches!(r5.result, ExecutionResult::Halt { .. }),
        "REX5 must hit the tx-level detained OutOfGas (Halt), not a frame-local Revert; got {:?}",
        r5.result,
    );
    assert!(
        matches!(r6.result, ExecutionResult::Halt { .. }),
        "REX6 must hit the tx-level detained OutOfGas (Halt), not a frame-local Revert; got {:?}",
        r6.result,
    );
    // REX5 swallows the gas forwarded to the discarded child → gas_used inflated.
    assert!(
        r5.gas_used > 50_000_000,
        "REX5 must swallow the forwarded gas (frozen bug); gas_used={}",
        r5.gas_used,
    );
    // REX6 erases the forwarded gas back to the parent before the detained halt → gas_used small.
    assert!(
        r6.gas_used < 1_000_000,
        "REX6 must return the forwarded gas on a tx-level halt too; gas_used={}",
        r6.gas_used,
    );
}

#[test]
fn test_rex6_two_logs_charge_two_base_overheads() {
    // Two LOG0(0,0) → two per-log base overheads under REX6.
    let code = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(0u64)
        .append(LOG0)
        .push_number(0u64)
        .push_number(0u64)
        .append(LOG0)
        .stop()
        .build();
    let r5 = transact_default(MegaSpecId::REX5, base_db(code.clone()));
    let r6 = transact_default(MegaSpecId::REX6, base_db(code));
    assert!(r5.is_success() && r6.is_success(), "both must succeed");
    assert_eq!(
        r6.data_size,
        r5.data_size + 2 * LOG_BASE_SIZE,
        "REX6 two LOG0 must add 2*LOG_BASE_SIZE over REX5; r5={} r6={}",
        r5.data_size,
        r6.data_size,
    );
}
