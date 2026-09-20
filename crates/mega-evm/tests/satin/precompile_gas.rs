//! What a call to the repriced KZG precompile costs, measured through the gas a transaction used.
//!
//! A successful evaluation costs a flat 100,000 whatever the caller forwarded. A call that
//! reaches verification and fails there — a wrong proof, a versioned hash that does not match the
//! commitment, an input of the wrong length — burns everything the caller forwarded, as a failing
//! precompile does. A caller that forwards less than the price is out of gas before verification
//! runs.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    kzg_point_evaluation::{ADDRESS as KZG, GAS_COST as KZG_GAS_COST},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaEvm,
};
use revm::{
    bytecode::opcode::{MSTORE, PUSH0, RETURN, STATICCALL},
    precompile::kzg_point_evaluation::kzg_to_versioned_hash,
    primitives::hex,
};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000a00000");
const CONTRACT: Address = address!("0000000000000000000000000000000000a00001");

const GAS_LIMIT: u64 = 10_000_000;

/// The c-kzg test vector `verify_kzg_proof_case_correct_proof_4_4`.
const COMMITMENT: [u8; 48] = hex!(
    "8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca2\
     5f26936857bc3a7c2539ea8ec3a952b7"
);
const Z: [u8; 32] = hex!("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000");
const Y: [u8; 32] = hex!("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9");
const PROOF: [u8; 48] = hex!(
    "a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc216074\
     4faf0070725e00b60ad9a026a15b1a8c"
);

/// The precompile's input: `versioned_hash ++ z ++ y ++ commitment ++ proof`.
fn kzg_input() -> Vec<u8> {
    let mut input = kzg_to_versioned_hash(&COMMITMENT).to_vec();
    input.extend_from_slice(&Z);
    input.extend_from_slice(&Y);
    input.extend_from_slice(&COMMITMENT);
    input.extend_from_slice(&PROOF);
    input
}

/// The same input with the last byte of the proof flipped: well formed, but the proof does not
/// verify.
fn invalid_proof() -> Vec<u8> {
    let mut input = kzg_input();
    let last = input.len() - 1;
    input[last] ^= 0x01;
    input
}

/// The same input with a versioned hash the commitment does not hash to.
fn mismatched_version() -> Vec<u8> {
    let mut input = kzg_input();
    input[0] = 0x02;
    input
}

/// A contract that `STATICCALL`s the KZG precompile with `forwarded` gas and `input`, and returns
/// the call's success flag.
fn caller_of(input: &[u8], forwarded: u64, args_len: u64) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, input)
        .push_number(0u64) // retLength
        .push_number(0u64) // retOffset
        .push_number(args_len)
        .push_number(0u64) // argsOffset
        .push_address(KZG)
        .push_number(forwarded)
        .append(STATICCALL)
        .append_many([PUSH0, MSTORE])
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build()
}

/// The gas the transaction used, and whether the `STATICCALL` reported success.
struct Call {
    gas_used: u64,
    succeeded: bool,
}

fn run(input: &[u8], forwarded: u64, args_len: u64) -> Call {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(CONTRACT, caller_of(input, forwarded, args_len));
    let mut evm = MegaEvm::new(context(db));
    let result = evm
        .transact_raw(call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT))
        .expect("the transaction is valid")
        .result;
    assert!(result.is_success(), "{result:?}");
    let output = result.output().expect("the caller returns the flag").clone();
    Call { gas_used: result.gas().tx_gas_used(), succeeded: output.iter().any(|byte| *byte != 0) }
}

/// A successful evaluation costs the same whatever the caller forwarded: the price is flat.
#[test]
fn test_a_valid_kzg_evaluation_costs_a_flat_price() {
    let at = |forwarded| run(&kzg_input(), forwarded, 192);
    let base = at(KZG_GAS_COST);
    assert!(base.succeeded);
    for forwarded in [KZG_GAS_COST + 1, 2 * KZG_GAS_COST, 10 * KZG_GAS_COST] {
        let call = at(forwarded);
        assert!(call.succeeded, "at {forwarded} forwarded");
        assert_eq!(call.gas_used, base.gas_used, "at {forwarded} forwarded");
    }
}

/// The flat price is 100,000: a verification that fails keeps the whole forwarded amount, and the
/// difference from a success is what the success did not spend.
#[test]
fn test_the_flat_price_is_one_hundred_thousand() {
    for forwarded in [2 * KZG_GAS_COST, 5 * KZG_GAS_COST] {
        let success = run(&kzg_input(), forwarded, 192);
        let failure = run(&invalid_proof(), forwarded, 192);
        assert!(success.succeeded);
        assert!(!failure.succeeded, "a wrong proof does not verify");
        assert_eq!(
            failure.gas_used - success.gas_used,
            forwarded - KZG_GAS_COST,
            "at {forwarded} forwarded"
        );
    }
    assert_eq!(KZG_GAS_COST, 100_000);
}

/// Each way verification can fail burns the forwarded gas the same way: a wrong proof, a
/// versioned hash that does not match, and an input of the wrong length.
#[test]
fn test_every_verification_failure_burns_the_forwarded_gas() {
    let forwarded = 5 * KZG_GAS_COST;
    let success = run(&kzg_input(), forwarded, 192);
    for (name, input, args_len) in [
        ("a wrong proof", invalid_proof(), 192),
        ("a mismatched versioned hash", mismatched_version(), 192),
        ("an input one byte short", kzg_input(), 191),
    ] {
        let failure = run(&input, forwarded, args_len);
        assert!(!failure.succeeded, "{name}");
        assert_eq!(
            failure.gas_used - success.gas_used,
            forwarded - KZG_GAS_COST,
            "{name} burns what was forwarded"
        );
    }
}

/// A caller that forwards less than the price is out of gas before verification runs, so the
/// price is a floor on what a call must carry, not only on what it pays.
#[test]
fn test_forwarding_less_than_the_price_is_out_of_gas() {
    let short = run(&kzg_input(), KZG_GAS_COST - 1, 192);
    assert!(!short.succeeded);
    // Upstream's own price would have been enough; the `MegaETH` price is not.
    let at_upstreams_price = run(&kzg_input(), 50_000, 192);
    assert!(!at_upstreams_price.succeeded);
}
