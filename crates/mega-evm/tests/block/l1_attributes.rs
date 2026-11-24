//! The block's L1 attributes deposit fits Satin's limits.
//!
//! The deposit is not the protocol's own transaction in the engine's sense: it is held to every
//! per-transaction limit, as any deposit is. A limit it crossed would stop it in every block —
//! every block would stay valid, and the L1 block contract would stop updating. Satin's limits are
//! fixed per spec, so no chain can set one that low; these tests hold the deposit to them.
//!
//! The deposit's data size is its body, its calldata, and one write record for each slot of the
//! L1 block contract it changes. Within one L1 origin it changes the sequence number alone; the
//! deposit that starts an origin changes every value that differs from the last origin's.

use alloy_consensus::{transaction::Recovered, Sealed};
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, bytes, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    test_utils::MemoryDatabase, MegaSpecId, MegaTransactionOutcome, MegaTxEnvelope, ProtocolLimits,
    TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use op_alloy_consensus::TxDeposit;
use op_revm::constants::L1_BLOCK_CONTRACT;
use revm::database::State;

use crate::common::{self, unlimited_ctx, BLOCK_NUMBER};

/// The account the L1 attributes deposit comes from, which only the protocol sends from.
const DEPOSITOR: Address = address!("0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001");

/// The runtime of OP's `L1Block` 1.6.0, the Isthmus version, as `MegaETH` mainnet holds it at
/// the L1 block contract's address: read from the recorded mainnet state of `mega-evme`'s replay
/// fixtures.
const L1_BLOCK_RUNTIME: Bytes = bytes!("0x608060405234801561001057600080fd5b50600436106101825760003560e01c806364ca23ef116100d8578063b80777ea1161008c578063e591b28211610066578063e591b282146103b0578063e81b2c6d146103d2578063f8206140146103db57600080fd5b8063b80777ea14610337578063c598591814610357578063d84447151461037757600080fd5b80638381f58a116100bd5780638381f58a146103115780638b239f73146103255780639e8c49661461032e57600080fd5b806364ca23ef146102e157806368d5dca6146102f557600080fd5b80634397dfef1161013a57806354fd4d501161011457806354fd4d501461025d578063550fcdc91461029f5780635cf24969146102d857600080fd5b80634397dfef146101fc578063440a5e20146102245780634d5d9a2a1461022c57600080fd5b806309bd5a601161016b57806309bd5a60146101a457806316d3bc7f146101c057806321326849146101ed57600080fd5b8063015d8eb914610187578063098999be1461019c575b600080fd5b61019a6101953660046105bc565b6103e4565b005b61019a610523565b6101ad60025481565b6040519081526020015b60405180910390f35b6008546101d49067ffffffffffffffff1681565b60405167ffffffffffffffff90911681526020016101b7565b604051600081526020016101b7565b6040805173eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee815260126020820152016101b7565b61019a61052d565b6008546102489068010000000000000000900463ffffffff1681565b60405163ffffffff90911681526020016101b7565b60408051808201909152600581527f312e362e3000000000000000000000000000000000000000000000000000000060208201525b6040516101b7919061062e565b60408051808201909152600381527f45544800000000000000000000000000000000000000000000000000000000006020820152610292565b6101ad60015481565b6003546101d49067ffffffffffffffff1681565b6003546102489068010000000000000000900463ffffffff1681565b6000546101d49067ffffffffffffffff1681565b6101ad60055481565b6101ad60065481565b6000546101d49068010000000000000000900467ffffffffffffffff1681565b600354610248906c01000000000000000000000000900463ffffffff1681565b60408051808201909152600581527f45746865720000000000000000000000000000000000000000000000000000006020820152610292565b60405173deaddeaddeaddeaddeaddeaddeaddeaddead000181526020016101b7565b6101ad60045481565b6101ad60075481565b3373deaddeaddeaddeaddeaddeaddeaddeaddead00011461048b576040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152603b60248201527f4c31426c6f636b3a206f6e6c7920746865206465706f7369746f72206163636f60448201527f756e742063616e20736574204c3120626c6f636b2076616c7565730000000000606482015260840160405180910390fd5b6000805467ffffffffffffffff98891668010000000000000000027fffffffffffffffffffffffffffffffff00000000000000000000000000000000909116998916999099179890981790975560019490945560029290925560038054919094167fffffffffffffffffffffffffffffffffffffffffffffffff00000000000000009190911617909255600491909155600555600655565b61052b610535565b565b61052b610548565b61053d610548565b60a43560a01c600855565b73deaddeaddeaddeaddeaddeaddeaddeaddead000133811461057257633cc50b456000526004601cfd5b60043560801c60035560143560801c60005560243560015560443560075560643560025560843560045550565b803567ffffffffffffffff811681146105b757600080fd5b919050565b600080600080600080600080610100898b0312156105d957600080fd5b6105e28961059f565b97506105f060208a0161059f565b9650604089013595506060890135945061060c60808a0161059f565b979a969950949793969560a0850135955060c08501359460e001359350915050565b600060208083528351808285015260005b8181101561065b5785810183015185820160400152820161063f565b8181111561066d576000604083870101525b50601f017fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe01692909201604001939250505056fea164736f6c634300080f000a");

/// The calldata of a recorded mainnet L1 attributes deposit: `setL1BlockValuesIsthmus()` and its
/// packed values, 176 bytes. Bytes 12 to 19 are the sequence number.
const L1_ATTRIBUTES_CALLDATA: Bytes = bytes!("0x098999be000005580000000000000000000000eb000000006a276d57000000000181b0bf0000000000000000000000000000000000000000000000000000000006e4978900000000000000000000000000000000000000000000000000227b46f61450dd10d3420bcc480eb9d4f97cdd4a77c165ba667381f60898af8a4ee999cf538e39000000000000000000000000b98c6b1a805b96707a43e1f1acfa163b68098fa6000000000000000000000000");

/// The storage slots `L1Block` 1.6.0 holds, 0 to 8: the most records a deposit can keep.
const L1_BLOCK_SLOTS: u64 = 9;

/// The deposit's calldata with `delta` added to the sequence number's last byte.
fn calldata(delta: u8) -> Bytes {
    let mut calldata = L1_ATTRIBUTES_CALLDATA.to_vec();
    calldata[19] = calldata[19].wrapping_add(delta);
    calldata.into()
}

/// The L1 attributes deposit carrying `calldata`, on a gas limit that writes every slot of the
/// contract with 1,000,000 of regular gas to spare at the byte prices in effect.
fn l1_attributes_deposit(calldata: Bytes) -> Recovered<MegaTxEnvelope> {
    let deposit = TxDeposit {
        source_hash: B256::repeat_byte(0x1a),
        from: DEPOSITOR,
        to: TxKind::Call(L1_BLOCK_CONTRACT),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000 + L1_BLOCK_SLOTS * common::slot_state_gas(),
        is_system_transaction: false,
        input: calldata,
    };
    Recovered::new_unchecked(
        MegaTxEnvelope::Deposit(Sealed::new_unchecked(deposit, B256::repeat_byte(0x1b))),
        DEPOSITOR,
    )
}

/// What the deposit carrying `calldata(delta)` produced, run as the first transaction of block
/// `number` on `state` and committed: under the spec's limits, or `limits` in their place.
fn run_in_block(
    state: &mut State<MemoryDatabase>,
    number: u64,
    delta: u8,
    limits: Option<ProtocolLimits>,
) -> MegaTransactionOutcome {
    let mut env = common::evm_env();
    env.block_env.number = U256::from(number);
    let mut executor = common::executor_with_env(state, unlimited_ctx(), env);
    assert_eq!(executor.protocol_limits(), &ProtocolLimits::for_spec(MegaSpecId::SATIN));
    if let Some(limits) = limits {
        executor = executor.with_protocol_limits(limits);
    }
    executor.apply_pre_execution_changes().expect("the block starts");
    let outcome = executor
        .run_transaction(&l1_attributes_deposit(calldata(delta)))
        .expect("the deposit runs");
    let inner = outcome.inner.clone();
    executor.commit_transaction_outcome(outcome).expect("the deposit is included");
    inner
}

/// What the deposit kept under the spec's limits — its data size and write records — which it
/// must run to a success that no limit stopped.
fn kept_under_the_spec(state: &mut State<MemoryDatabase>, number: u64, delta: u8) -> (u64, u64) {
    let outcome = run_in_block(state, number, delta, None);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None, "no limit stops the deposit");
    (outcome.usage.data_size, outcome.usage.write_records)
}

/// A deposit within one L1 origin changes the sequence number alone and keeps 526 bytes and one
/// write record, the figures measured on recorded mainnet blocks: its 310-byte body, its 176 bytes
/// of calldata and the record. Satin's transaction data-size limit holds it with room to spare. A
/// limit one byte short of it, which no chain can set, would stop it in every block.
///
/// Expected values: independent (measured on recorded blocks of one L1 origin).
#[test]
fn test_an_l1_attributes_deposit_within_an_origin_fits_the_satin_limits() {
    let mut db = common::database();
    db.set_account_code(L1_BLOCK_CONTRACT, L1_BLOCK_RUNTIME);
    let mut state = State::builder().with_database(db).build();
    kept_under_the_spec(&mut state, BLOCK_NUMBER, 0);

    let (data_size, write_records) = kept_under_the_spec(&mut state, BLOCK_NUMBER + 1, 1);
    assert_eq!((data_size, write_records), (526, 1));
    assert_eq!(data_size, TX_BODY_SIZE + 176 + WRITE_RECORD_SIZE);
    let limits = ProtocolLimits::for_spec(MegaSpecId::SATIN);
    assert!(data_size <= limits.tx_runtime_limits.tx_data_size_limit);
    assert!(data_size <= limits.block_txs_data_limit);

    let short = limits
        .with_tx_runtime_limits(limits.tx_runtime_limits.with_tx_data_size_limit(data_size - 1));
    let stopped = run_in_block(&mut state, BLOCK_NUMBER + 2, 2, Some(short));
    assert!(stopped.limit_exceeded.is_some(), "{:?}", stopped.result);
}

/// The deposit that starts an L1 origin, on a contract that holds nothing yet, changes every slot
/// it sets: one record each, at most one per slot of the contract. That is still far inside
/// Satin's limits.
///
/// Expected values: independent (the contract's storage layout bounds the records).
#[test]
fn test_an_l1_attributes_deposit_that_starts_an_origin_fits_the_satin_limits() {
    let mut db = common::database();
    db.set_account_code(L1_BLOCK_CONTRACT, L1_BLOCK_RUNTIME);
    let mut state = State::builder().with_database(db).build();

    let (data_size, write_records) = kept_under_the_spec(&mut state, BLOCK_NUMBER, 0);
    assert!((1..=L1_BLOCK_SLOTS).contains(&write_records), "{write_records}");
    assert_eq!(data_size, TX_BODY_SIZE + 176 + write_records * WRITE_RECORD_SIZE);
    let most = TX_BODY_SIZE + 176 + L1_BLOCK_SLOTS * WRITE_RECORD_SIZE;
    assert_eq!(most, 846);
    let limits = ProtocolLimits::for_spec(MegaSpecId::SATIN);
    assert!(most <= limits.tx_runtime_limits.tx_data_size_limit);
    assert!(most <= limits.block_txs_data_limit);
}
