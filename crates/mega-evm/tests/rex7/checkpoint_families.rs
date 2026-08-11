//! REX7: one parity case per checkpoint opcode the REX7 table wires.
//!
//! `checkpoint_settlement` covers the checkpoint families through representative members — one
//! LOG, one SLOAD, the CALL family, CREATE / CREATE2, SELFDESTRUCT, a few volatile opcodes. This
//! file closes the set: every opcode the REX7 instruction table replaces with a checkpoint handler
//! gets its own parity case, so a handler wired with the wrong settlement macro — or left out of a
//! future table edit — fails here rather than only in whichever downstream test happened to use it.
//!
//! Each opcode is run twice: once plainly, and once with a detention cap engaged before it so a
//! clamp is outstanding when its prologue runs. Both runs must be indistinguishable from REX6.

use crate::common::{assert_outcomes_identical, transact, CALLEE, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::bytecode::opcode::{
    BALANCE, BASEFEE, BLOBBASEFEE, BLOBHASH, BLOCKHASH, COINBASE, DIFFICULTY, EXTCODECOPY,
    EXTCODEHASH, EXTCODESIZE, GAS, GASLIMIT, LOG0, LOG1, LOG2, LOG3, LOG4, NUMBER, POP,
    SELFBALANCE, SLOAD, STOP, TIMESTAMP,
};

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
        .account_code(CALLEE, BytecodeBuilder::default().append(STOP).build())
}

/// `pairs` PUSH1/POP pairs — plain opcodes that record nothing of their own.
fn plain_filler(builder: BytecodeBuilder, pairs: usize) -> BytecodeBuilder {
    let mut builder = builder;
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder
}

/// Wraps `snippet` in plain segments on both sides, optionally engaging a detention cap first, so
/// the checkpoint under test has an open segment to settle and a clamp to restore.
fn program(snippet: impl Fn(BytecodeBuilder) -> BytecodeBuilder, volatile_prologue: bool) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    if volatile_prologue {
        builder = builder.append(TIMESTAMP).append(POP);
    }
    let builder = snippet(plain_filler(builder, 5));
    plain_filler(builder, 5).append(STOP).build()
}

/// Runs one checkpoint opcode under both specs, plainly and with a clamp outstanding.
fn assert_checkpoint_parity(label: &str, snippet: impl Fn(BytecodeBuilder) -> BytecodeBuilder) {
    for (arm, volatile_prologue, cap) in
        [("plain", false, u64::MAX), ("under a clamp", true, 1_000_000)]
    {
        let code = program(&snippet, volatile_prologue);
        let limits = |spec| {
            let mut limits = EvmTxRuntimeLimits::from_spec(spec);
            if cap != u64::MAX {
                limits.block_env_access_compute_gas_limit = cap;
            }
            limits
        };
        let r6 = transact(MegaSpecId::REX6, base_db(code.clone()), limits(MegaSpecId::REX6));
        let r7 = transact(MegaSpecId::REX7, base_db(code), limits(MegaSpecId::REX7));
        let label = format!("{label} ({arm})");
        assert!(r6.is_success(), "{label}: REX6 must succeed: {:?}", r6.result);
        assert!(r7.is_success(), "{label}: REX7 must succeed: {:?}", r7.result);
        assert_outcomes_identical(&label, &r6, &r7);
    }
}

/// Pushes `n` LOG topics, then the payload length and offset, in the order the opcode pops them.
fn log_operands(builder: BytecodeBuilder, topics: usize) -> BytecodeBuilder {
    let mut builder = builder.mstore(0, [0x44u8; 32]);
    for topic in (0..topics).rev() {
        builder = builder.push_number(0xabc0u64 + topic as u64);
    }
    builder.push_number(32u64).push_number(0u64)
}

/// The block-environment opcodes: each marks volatile access, settles its segment, then installs
/// the detention cap. They take no operands and push one word.
#[test]
fn test_block_env_checkpoints_match_per_opcode() {
    for (label, opcode) in [
        ("COINBASE", COINBASE),
        ("TIMESTAMP", TIMESTAMP),
        ("NUMBER", NUMBER),
        ("DIFFICULTY", DIFFICULTY),
        ("GASLIMIT", GASLIMIT),
        ("BASEFEE", BASEFEE),
        ("BLOBBASEFEE", BLOBBASEFEE),
        ("SELFBALANCE", SELFBALANCE),
    ] {
        assert_checkpoint_parity(label, |builder| builder.append(opcode).append(POP));
    }
}

/// The operand-taking volatile checkpoints.
#[test]
fn test_operand_taking_volatile_checkpoints_match_per_opcode() {
    assert_checkpoint_parity("BLOCKHASH", |builder| {
        builder.push_number(0u64).append(BLOCKHASH).append(POP)
    });
    assert_checkpoint_parity("BLOBHASH", |builder| {
        builder.push_number(0u64).append(BLOBHASH).append(POP)
    });
    assert_checkpoint_parity("BALANCE", |builder| {
        builder.push_address(CALLEE).append(BALANCE).append(POP)
    });
    assert_checkpoint_parity("EXTCODESIZE", |builder| {
        builder.push_address(CALLEE).append(EXTCODESIZE).append(POP)
    });
    assert_checkpoint_parity("EXTCODEHASH", |builder| {
        builder.push_address(CALLEE).append(EXTCODEHASH).append(POP)
    });
    assert_checkpoint_parity("EXTCODECOPY", |builder| {
        builder
            .push_number(32u64) // length
            .push_number(0u64) // offset
            .push_number(0u64) // destOffset
            .push_address(CALLEE)
            .append(EXTCODECOPY)
    });
    assert_checkpoint_parity("SLOAD", |builder| {
        builder.push_u256(U256::from(3)).append(SLOAD).append(POP)
    });
}

/// `GAS` is a checkpoint only because of the clamp: it has to restore the hidden gas before revm's
/// instruction reads the counter.
#[test]
fn test_gas_checkpoint_matches_per_opcode() {
    assert_checkpoint_parity("GAS", |builder| builder.append(GAS).append(POP));
}

/// Every LOG arity: the storage-gas surcharge scales with the topic count, and each arity is a
/// separate table entry.
#[test]
fn test_every_log_arity_matches_per_opcode() {
    for (label, opcode, topics) in [
        ("LOG0", LOG0, 0),
        ("LOG1", LOG1, 1),
        ("LOG2", LOG2, 2),
        ("LOG3", LOG3, 3),
        ("LOG4", LOG4, 4),
    ] {
        assert_checkpoint_parity(label, move |builder| {
            log_operands(builder, topics).append(opcode)
        });
    }
}

/// SSTORE across the three write shapes its storage-gas charge distinguishes: a first write to a
/// fresh slot, an overwrite of that slot, and a write back to zero.
#[test]
fn test_sstore_write_shapes_match_per_opcode() {
    assert_checkpoint_parity("SSTORE zero -> non-zero", |builder| {
        builder.sstore(U256::from(0x50), U256::from(0x11))
    });
    assert_checkpoint_parity("SSTORE non-zero -> non-zero", |builder| {
        builder
            .sstore(U256::from(0x50), U256::from(0x11))
            .sstore(U256::from(0x50), U256::from(0x22))
    });
    assert_checkpoint_parity("SSTORE non-zero -> zero", |builder| {
        builder.sstore(U256::from(0x50), U256::from(0x11)).sstore(U256::from(0x50), U256::ZERO)
    });
    assert_checkpoint_parity("SSTORE then SLOAD of the same slot", |builder| {
        builder
            .sstore(U256::from(0x50), U256::from(0x11))
            .push_u256(U256::from(0x50))
            .append(SLOAD)
            .append(POP)
    });
    assert_checkpoint_parity("two SSTOREs with a plain gap", |builder| {
        let builder = builder.sstore(U256::from(0x50), U256::from(0x11));
        plain_filler(builder, 10).sstore(U256::from(0x51), U256::from(0x22))
    });
}

/// Back-to-back checkpoints with no plain opcode between them: the settlement window has to open
/// and close on a zero-length segment without billing anything twice.
#[test]
fn test_adjacent_checkpoints_match_per_opcode() {
    assert_checkpoint_parity("TIMESTAMP NUMBER COINBASE", |builder| {
        builder
            .append(TIMESTAMP)
            .append(POP)
            .append(NUMBER)
            .append(POP)
            .append(COINBASE)
            .append(POP)
    });
    assert_checkpoint_parity("GAS GAS", |builder| {
        builder.append(GAS).append(POP).append(GAS).append(POP)
    });
    assert_checkpoint_parity("SSTORE SSTORE", |builder| {
        builder.sstore(U256::from(0x60), U256::from(1)).sstore(U256::from(0x61), U256::from(2))
    });
}
