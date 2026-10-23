//! A block through `MegaBlockExecutor`: what block execution costs per transaction on top of
//! what the transaction itself costs, in a chain's steady state.
//!
//! Every block runs as a node runs one:
//!
//! - on a state that holds the seven predeploys as a chain that ran Rex6 holds them, with what the
//!   previous block's pre-block changes left: no block but Satin's activation block reads the
//!   predeploys, and none deploys them;
//! - with the EIP-2935 and EIP-4788 contracts' own code, so the pre-block calls record the parent
//!   hash and the beacon root;
//! - under the spec's limits, `ProtocolLimits::for_spec`: the production data-size limits and gas
//!   detention's caps, with no building policy on top (`BlockLimits::default()`);
//! - from a fresh `State` over that database, as a node starts each block.
//!
//! Three workloads of `N` transactions each, whose per-transaction work beyond the transaction
//! itself is the executor's own: the admission checks, the size estimates, the limits, the
//! counters, the receipt and the commit.
//!
//! - `transfers`: value transfers to accounts with no code — the cheapest transaction there is, so
//!   the executor's own cost is most of what is measured.
//! - `storage_writes`: calls to a contract that writes the slot the transaction names in its
//!   calldata, so every transaction changes storage and carries a write record through the
//!   counters, with a log-free receipt.
//! - `erc20_transfers`: ERC-20 `transfer`s, each from a sender of its own to a recipient of its own
//!   that already holds tokens, on the token `contracts/Token.sol` builds: a block of the commonest
//!   contract call there is. Each reads and writes two balances and emits one `Transfer` log, so it
//!   carries two write records and a log through the counters.
//!
//! The measurement runs `apply_pre_execution_changes`, the transactions and `finish` — the whole
//! block a node executes — and hands the state back, so dropping it is not measured. One more arm,
//! `steady_pre_block`, measures `apply_pre_execution_changes` alone on the steady state: the
//! per-block cost of the pre-block system calls and reads.
#![allow(missing_docs)]

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
};
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, hex, keccak256, Address, Bytes, Signature, TxKind, B256, U256};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::{
    system::{MEGA_SYSTEM_ADDRESS, SEQUENCER_REGISTRY_ADDRESS},
    test_utils::{BytecodeBuilder, CommittedStates, MemoryDatabase},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory, MegaHardforkConfig,
    MegaSpecId, MegaTxEnvelope,
};
use op_revm::constants::L1_BLOCK_CONTRACT;
use revm::{
    bytecode::opcode::{CALLDATALOAD, SSTORE},
    context::{BlockEnv, CfgEnv},
    database::State,
    state::EvmState,
    DatabaseCommit,
};
use std::hint::black_box;

/// Transactions in the block.
const N: u64 = 64;

/// The chain the block belongs to.
const CHAIN_ID: u64 = 4_326;

/// The block the bench executes.
const BLOCK_NUMBER: u64 = 1_000;

/// The timestamp of the block the bench executes.
const BLOCK_TIMESTAMP: u64 = 1_800_000_000;

/// A non-zero parent hash and beacon root. A zero stored over an empty slot is not a write, so
/// the pre-block calls would leave no storage behind.
const PARENT_HASH: B256 = B256::repeat_byte(0xab);
const PARENT_BEACON_ROOT: B256 = B256::repeat_byte(0xcd);

const SENDER: Address = address!("0x2000000000000000000000000000000000000002");
const RECIPIENT: Address = address!("0x3000000000000000000000000000000000000003");
const WRITER: Address = address!("0x1000000000000000000000000000000000000001");
const TOKEN: Address = address!("0x4000000000000000000000000000000000000004");

/// The runtime code of `contracts/Token.sol`, compiled with solc 0.8.30:
/// `solc --optimize --optimize-runs 200 --metadata-hash none --bin-runtime Token.sol`.
const TOKEN_CODE: [u8; 492] = hex!("608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806370a0823114610038578063a9059cbb1461006a575b5f5ffd5b610057610046366004610178565b5f6020819052908152604090205481565b6040519081526020015b60405180910390f35b61007d610078366004610198565b61008d565b6040519015158152602001610061565b335f90815260208190526040812054828110156100da5760405162461bcd60e51b815260206004820152600760248201526662616c616e636560c81b604482015260640160405180910390fd5b335f9081526020819052604080822085840390556001600160a01b03861682528120805485929061010c9084906101c0565b90915550506040518381526001600160a01b0385169033907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9060200160405180910390a360019150505b92915050565b80356001600160a01b0381168114610173575f5ffd5b919050565b5f60208284031215610188575f5ffd5b6101918261015d565b9392505050565b5f5f604083850312156101a9575f5ffd5b6101b28361015d565b946020939093013593505050565b8082018082111561015757634e487b7160e01b5f52601160045260245ffdfea164736f6c634300081e000a");

/// The first of the `N` accounts the ERC-20 transfers are sent from, and the first of the `N`
/// accounts they pay, each one per transaction.
const TOKEN_SENDER_BASE: u64 = 0x60_0000;
const TOKEN_HOLDER_BASE: u64 = 0x70_0000;

/// Code that writes 1 to the slot its first calldata word names, so every call that names a slot
/// of its own changes storage and leaves a write record behind.
fn writer_code() -> Bytes {
    BytecodeBuilder::default()
        .push_number(1_u64)
        .push_number(0_u64)
        .append(CALLDATALOAD)
        .append(SSTORE)
        .stop()
        .build()
}

/// The slot the transaction with this nonce writes: one per transaction, so none of them writes
/// a value the slot already holds.
fn slot_calldata(nonce: u64) -> Bytes {
    Bytes::from(U256::from(nonce + 1).to_be_bytes::<32>())
}

/// The `index`-th account of the run of accounts that starts at `base`.
fn nth_account(base: u64, index: u64) -> Address {
    Address::from_word(B256::from(U256::from(base + index)))
}

/// The slot of the token's `balanceOf` mapping, its first, that holds `owner`'s balance.
fn token_balance_slot(owner: Address) -> U256 {
    let mut word = [0_u8; 64];
    word[12..32].copy_from_slice(owner.as_slice());
    U256::from_be_bytes(keccak256(word).0)
}

/// The calldata of `transfer(to, value)`.
fn token_transfer_calldata(to: Address, value: u64) -> Bytes {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&hex!("a9059cbb"));
    data.extend_from_slice(&to.into_word().0);
    data.extend_from_slice(&U256::from(value).to_be_bytes::<32>());
    Bytes::from(data)
}

/// The accounts the workloads use, the EIP-2935 and EIP-4788 contracts' code, and the seven
/// predeploys as a chain that ran Rex6 holds them. Every ERC-20 sender holds ether and tokens,
/// and every ERC-20 recipient holds tokens already, so no transfer adds a slot.
fn database() -> MemoryDatabase {
    let mut db = MemoryDatabase::default().rex6_predeploys(MEGA_SYSTEM_ADDRESS);
    db.set_account_code(WRITER, writer_code());
    db.set_account_code(RECIPIENT, Bytes::new());
    db.set_account_balance(SENDER, U256::from(1_000_000_000_000_000_u64));
    db.set_account_code(TOKEN, Bytes::from_static(&TOKEN_CODE));
    for index in 0..N {
        let sender = nth_account(TOKEN_SENDER_BASE, index);
        db.set_account_balance(sender, U256::from(1_000_000_000_000_000_u64));
        db.set_account_storage(TOKEN, token_balance_slot(sender), U256::from(1_000_000_u64));
        let holder = nth_account(TOKEN_HOLDER_BASE, index);
        db.set_account_storage(TOKEN, token_balance_slot(holder), U256::from(1_u64));
    }
    db.set_account_code(HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone());
    db.set_account_code(BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone());
    db
}

/// The environment of block `number`, one second after the block before it.
fn evm_env_at(number: u64) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = CfgEnv::new_with_spec(MegaSpecId::SATIN);
    cfg_env.chain_id = CHAIN_ID;
    EvmEnv {
        cfg_env,
        block_env: BlockEnv {
            number: U256::from(number),
            timestamp: U256::from(BLOCK_TIMESTAMP + number - BLOCK_NUMBER),
            gas_limit: 1_000_000_000,
            ..Default::default()
        },
    }
}

/// The environment of the block the bench executes.
fn evm_env() -> EvmEnv<MegaSpecId> {
    evm_env_at(BLOCK_NUMBER)
}

/// A block packed under no building policy, as a validator executes one: the chain's limits
/// alone hold it.
fn block_ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(
        PARENT_HASH,
        Some(PARENT_BEACON_ROOT),
        Bytes::new(),
        BlockLimits::default(),
    )
}

/// A legacy call from `sender` at `nonce` to `to`, carrying `value` and `input`.
fn transaction(
    sender: Address,
    nonce: u64,
    to: Address,
    value: u64,
    gas_limit: u64,
    input: Bytes,
) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: 0,
        gas_limit,
        to: TxKind::Call(to),
        value: U256::from(value),
        input,
    };
    let signed = Signed::new_unchecked(tx, Signature::test_signature(), B256::repeat_byte(1));
    Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), sender)
}

/// `N` transactions from `SENDER` to `to`, one per nonce, each carrying the input `input` builds
/// for it.
fn transactions(
    to: Address,
    value: u64,
    gas_limit: u64,
    input: impl Fn(u64) -> Bytes,
) -> Vec<Recovered<MegaTxEnvelope>> {
    (0..N).map(|nonce| transaction(SENDER, nonce, to, value, gas_limit, input(nonce))).collect()
}

/// `N` ERC-20 transfers of one token, the `i`-th from the `i`-th sender to the `i`-th holder.
fn token_transfers() -> Vec<Recovered<MegaTxEnvelope>> {
    (0..N)
        .map(|index| {
            let sender = nth_account(TOKEN_SENDER_BASE, index);
            let input = token_transfer_calldata(nth_account(TOKEN_HOLDER_BASE, index), 1);
            transaction(sender, 0, TOKEN, 0, 300_000, input)
        })
        .collect()
}

fn chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default().with_all_activated()
}

/// Runs the pre-block changes of block `number` on a state over `db` and returns what they
/// committed to it, in order, as a commit hook on the state receives it; `db` itself is not
/// changed.
fn pre_block_states(db: MemoryDatabase, number: u64) -> Vec<EvmState> {
    let spec = chain_spec();
    let mut state = State::builder().with_database(db).build();
    let committed = CommittedStates::install(&mut state);
    let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env_at(number));
    let mut executor =
        MegaBlockExecutor::new(evm, block_ctx(), &spec, OpAlloyReceiptBuilder::default());
    executor.apply_pre_execution_changes().expect("the pre-block changes apply");
    drop(executor);
    committed.states()
}

/// The database the chain's steady state starts a block from: [`database`] with what the
/// previous block's pre-block changes left in it, committed in the order the executor commits
/// them.
fn steady_database() -> MemoryDatabase {
    let mut db = database();
    for state in pre_block_states(db.clone(), BLOCK_NUMBER - 1) {
        db.commit(state);
    }
    db
}

/// Asserts that the pre-block changes of the bench's block on `db` are a steady block's: the two
/// calls, which write their contracts, then the reads of the registry's pending changes and of the
/// L1 block info, and no read of a predeploy, which only Satin's activation block makes. Each step
/// names one account.
fn assert_steady_pre_block(db: &MemoryDatabase) {
    let states = pre_block_states(db.clone(), BLOCK_NUMBER);
    let named: Vec<_> = states
        .iter()
        .map(|state| {
            assert_eq!(state.len(), 1, "a pre-block step names one account: {state:?}");
            *state.keys().next().expect("one account")
        })
        .collect();
    assert_eq!(
        named,
        [
            HISTORY_STORAGE_ADDRESS,
            BEACON_ROOTS_ADDRESS,
            SEQUENCER_REGISTRY_ADDRESS,
            L1_BLOCK_CONTRACT
        ],
        "a steady block reads no predeploy"
    );
    // Outside every measured section: both calls wrote the contract the bench seeded.
    for (call, address) in states.iter().zip([HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS]) {
        let writes = call[&address].storage.values().filter(|slot| slot.is_changed()).count();
        assert!(writes > 0, "the call to {address} wrote its contract's storage");
    }
}

fn bench_block(c: &mut Criterion) {
    let spec = chain_spec();
    let steady = steady_database();
    // Checked once before measurement, so a block that is not a steady one is not measured as one.
    assert_steady_pre_block(&steady);

    // Each workload with the write records each of its transactions keeps.
    let workloads = [
        // The budgets clear the state gas the Satin gas table charges: a transfer to an account
        // that does not exist yet pays for the new account, a storage write pays for the slot.
        ("transfers", transactions(RECIPIENT, 1, 300_000, |_| Bytes::new()), 1),
        ("storage_writes", transactions(WRITER, 0, 300_000, slot_calldata), 1),
        // The sender's balance and the recipient's.
        ("erc20_transfers", token_transfers(), 2),
    ];

    let mut group = c.benchmark_group("block");
    for (name, txs, records) in &workloads {
        // The workload runs once outside the measurement, so a block that does not execute — or
        // one whose transactions keep nothing — is caught here rather than being measured.
        {
            let mut state = State::builder().with_database(steady.clone()).build();
            let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env());
            let mut executor =
                MegaBlockExecutor::new(evm, block_ctx(), &spec, OpAlloyReceiptBuilder::default());
            executor.apply_pre_execution_changes().expect("the block starts");
            for tx in txs {
                let outcome =
                    executor.execute_transaction_without_commit(tx).expect("the transaction runs");
                assert!(outcome.result.is_success(), "{:?}", outcome.result);
                assert_eq!(outcome.limit_exceeded, None, "no limit stops the workload");
                assert_eq!(
                    outcome.usage.write_records, *records,
                    "every transaction of the workload keeps its write records"
                );
                executor.commit_transaction(outcome);
            }
            let (_, result) = executor.finish_with_counters().expect("the block finishes");
            assert_eq!(result.receipts().len(), N as usize);
            assert!(result.gas.execution > 0);
            assert_eq!(result.usage.write_records, N * records);
        }

        group.bench_function(*name, |b| {
            b.iter_batched(
                || State::builder().with_database(steady.clone()).build(),
                |mut state| {
                    let result = {
                        let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env());
                        let mut executor = MegaBlockExecutor::new(
                            evm,
                            block_ctx(),
                            &spec,
                            OpAlloyReceiptBuilder::default(),
                        );
                        executor.apply_pre_execution_changes().unwrap();
                        for tx in txs {
                            black_box(executor.execute_transaction(tx).unwrap());
                        }
                        executor.finish_with_counters().unwrap().1
                    };
                    (state, black_box(result))
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.bench_function("steady_pre_block", |b| {
        b.iter_batched(
            || State::builder().with_database(steady.clone()).build(),
            |mut state| {
                let used = {
                    let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env());
                    let mut executor = MegaBlockExecutor::new(
                        evm,
                        block_ctx(),
                        &spec,
                        OpAlloyReceiptBuilder::default(),
                    );
                    executor.apply_pre_execution_changes().unwrap();
                    executor.limiter().block_gas_used
                };
                (state, black_box(used))
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_block);
criterion_main!(benches);
