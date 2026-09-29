//! Shared setup of the block-executor tests.
//!
//! A block on a chain whose schedule activates Satin at genesis, over an in-memory database with
//! an empty L1 block contract — so the L1 fees, the operator fee and the data-availability
//! footprint scalar all read as zero unless a test writes them.

use alloy_consensus::{transaction::Recovered, Sealed, Signed, TxLegacy};
use alloy_evm::{EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, SequencerRegistryConfig, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::MemoryDatabase,
    BlockLimits, EmptyExternalEnv, EvmTxRuntimeLimits, MegaBlockExecutionCtx, MegaBlockExecutor,
    MegaBlockExecutorFactory, MegaEvm, MegaEvmFactory, MegaHardforkConfig, MegaSpecId,
    MegaTxEnvelope, PreBlockStateSource, ProtocolLimits,
};
use op_alloy_consensus::TxDeposit;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::cfg::GasId,
    database::State,
    inspector::NoOpInspector,
    state::EvmState,
};
use std::sync::{Arc, Mutex};

/// The sender every test transaction comes from.
pub(crate) const CALLER: Address = address!("0x2000000000000000000000000000000000000002");

/// The account every test transaction calls.
pub(crate) const CONTRACT: Address = address!("0x1000000000000000000000000000000000000001");

/// The block the tests execute.
pub(crate) const BLOCK_NUMBER: u64 = 1_000;

/// The timestamp of the block the tests execute.
pub(crate) const BLOCK_TIMESTAMP: u64 = 1_800_000_000;

/// The gas limit of the block the tests execute.
pub(crate) const BLOCK_GAS_LIMIT: u64 = 30_000_000;

/// The chain id the test transactions carry.
pub(crate) const CHAIN_ID: u64 = 4_326;

/// The database a block runs on: the caller is funded and the callee has empty code.
pub(crate) fn database() -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, Bytes::new());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000_u64));
    db
}

/// The state a block runs on.
pub(crate) fn state() -> State<MemoryDatabase> {
    State::builder().with_database(database()).build()
}

/// The environment of the block the tests execute.
pub(crate) fn evm_env() -> EvmEnv<MegaSpecId> {
    let mut cfg_env = CfgEnv::new_with_spec(MegaSpecId::SATIN);
    cfg_env.chain_id = CHAIN_ID;
    let block_env = BlockEnv {
        number: U256::from(BLOCK_NUMBER),
        timestamp: U256::from(BLOCK_TIMESTAMP),
        gas_limit: BLOCK_GAS_LIMIT,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

/// The sequencer the tests seed into the registry, distinct from the system address and admin.
pub(crate) const SEQUENCER: Address = address!("0x2222222222222222222222222222222222222222");

/// The admin the tests seed into the registry, distinct from the system address and sequencer.
pub(crate) const ADMIN: Address = address!("0x3333333333333333333333333333333333333333");

/// Registry params the block tests attach to a Satin-at-genesis schedule.
pub(crate) fn registry_config() -> SequencerRegistryConfig {
    SequencerRegistryConfig {
        initial_system_address: MEGA_SYSTEM_ADDRESS,
        initial_sequencer: SEQUENCER,
        initial_admin: ADMIN,
        initial_from_block: 1,
        min_rotation_delay: 100,
    }
}

/// A schedule that activates Satin at genesis, can seed the `SequencerRegistry`, and holds its
/// blocks to the loosest limits a chain may carry.
///
/// Its limits are [`ProtocolLimits::loosest`]: most block tests exercise one mechanism and take
/// the limits out, and a test that holds a block or a transaction to a limit attaches its own
/// ([`chain_spec_with`]). Every limit is unlimited but gas detention's caps, which no transaction
/// of a block this size reaches, so a transaction that reads volatile data is detained and never
/// stopped.
pub(crate) fn chain_spec() -> MegaHardforkConfig {
    chain_spec_with(ProtocolLimits::loosest())
}

/// [`chain_spec`], holding its blocks to `limits`, which must be limits a chain may carry: the
/// block executor refuses a block under any other.
pub(crate) fn chain_spec_with(limits: ProtocolLimits) -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with_all_activated()
        .with_params(registry_config())
        .with_params(limits)
}

/// The per-transaction half of [`ProtocolLimits::loosest`], for a test that holds a block's
/// transactions to one limit and no other.
pub(crate) const fn loosest_tx() -> EvmTxRuntimeLimits {
    ProtocolLimits::loosest().tx_runtime_limits
}

/// The context of a block the builder packs under `policy`.
///
/// Cancun is active, so the block carries a parent beacon block root; without one the EIP-4788
/// pre-block call refuses the block.
pub(crate) fn block_ctx(policy: BlockLimits) -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(B256::ZERO, Some(B256::ZERO), Bytes::new(), policy)
}

/// The context of a block packed under no building policy, as a validator executes one.
pub(crate) fn unlimited_ctx() -> MegaBlockExecutionCtx {
    block_ctx(BlockLimits::no_limits())
}

/// The EVM the tests run a block on.
pub(crate) type TestEvm<'a> =
    MegaEvm<&'a mut State<MemoryDatabase>, NoOpInspector, EmptyExternalEnv>;

/// The executor the tests drive.
pub(crate) type TestExecutor<'a> =
    MegaBlockExecutor<TestEvm<'a>, OpAlloyReceiptBuilder, MegaHardforkConfig>;

/// The factory the tests build executors from.
pub(crate) type TestFactory = MegaBlockExecutorFactory<
    OpAlloyReceiptBuilder,
    MegaHardforkConfig,
    MegaEvmFactory<EmptyExternalEnv>,
>;

/// The factory the tests build executors from.
pub(crate) fn factory() -> TestFactory {
    factory_on(chain_spec())
}

/// A factory of executors for blocks on the chain `spec` describes.
pub(crate) fn factory_on(spec: MegaHardforkConfig) -> TestFactory {
    MegaBlockExecutorFactory::new(OpAlloyReceiptBuilder::default(), spec, MegaEvmFactory::new())
}

/// An executor over `state`, for a block held to `ctx`.
pub(crate) fn executor(
    state: &mut State<MemoryDatabase>,
    ctx: MegaBlockExecutionCtx,
) -> TestExecutor<'_> {
    executor_with_env(state, ctx, evm_env())
}

/// An executor over `state`, for a block held to `ctx` in the environment `env`.
pub(crate) fn executor_with_env(
    state: &mut State<MemoryDatabase>,
    ctx: MegaBlockExecutionCtx,
    env: EvmEnv<MegaSpecId>,
) -> TestExecutor<'_> {
    build(state, ctx, env, chain_spec())
}

/// An executor over `state`, for a block the chain holds to `limits`, packed under no building
/// policy.
pub(crate) fn executor_with_limits(
    state: &mut State<MemoryDatabase>,
    limits: ProtocolLimits,
) -> TestExecutor<'_> {
    build(state, unlimited_ctx(), evm_env(), chain_spec_with(limits))
}

/// An executor over `state`, for a block on the chain `spec` describes.
pub(crate) fn executor_with_spec(
    state: &mut State<MemoryDatabase>,
    ctx: MegaBlockExecutionCtx,
    spec: MegaHardforkConfig,
) -> TestExecutor<'_> {
    build(state, ctx, evm_env(), spec)
}

/// An executor over `state`, for a block held to `ctx` in the environment `env`, on the chain
/// `spec` describes.
pub(crate) fn executor_with_env_and_spec(
    state: &mut State<MemoryDatabase>,
    ctx: MegaBlockExecutionCtx,
    env: EvmEnv<MegaSpecId>,
    spec: MegaHardforkConfig,
) -> TestExecutor<'_> {
    build(state, ctx, env, spec)
}

fn build(
    state: &mut State<MemoryDatabase>,
    ctx: MegaBlockExecutionCtx,
    env: EvmEnv<MegaSpecId>,
    spec: MegaHardforkConfig,
) -> TestExecutor<'_> {
    let evm = MegaEvmFactory::new().create_evm(state, env);
    MegaBlockExecutor::new(evm, ctx, spec, OpAlloyReceiptBuilder::default())
}

/// Shared log of pre-block states an executor observer records.
pub(crate) type PreBlockLog = Arc<Mutex<Vec<(PreBlockStateSource, EvmState)>>>;

/// Installs a recording observer on `executor` and returns the log it writes.
pub(crate) fn record_pre_block(executor: &mut TestExecutor<'_>) -> PreBlockLog {
    let log = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&log);
    executor.set_pre_block_observer(Some(Box::new(
        move |source: PreBlockStateSource, state: &EvmState| {
            captured.lock().expect("pre-block observer").push((source, state.clone()));
        },
    )));
    log
}

/// The states the log holds, in execution order.
pub(crate) fn pre_block_states(log: &PreBlockLog) -> Vec<(PreBlockStateSource, EvmState)> {
    log.lock().expect("pre-block observer").clone()
}

/// The history gas the body of a legacy transaction carrying `calldata_len` bytes of calldata
/// pays, at the byte prices in effect.
pub(crate) fn body_history(calldata_len: u64) -> u64 {
    mega_evm::history_gas(mega_evm::tx_body_history_bytes(calldata_len, 0, 0, 0))
        .expect("the body has a price")
}

/// A gas limit for a call to the empty [`CONTRACT`]: 100,000 of regular gas on top of the
/// history its body pays, so the call has the same room whatever a history byte costs.
pub(crate) fn empty_call_gas() -> u64 {
    100_000 + body_history(0)
}

/// A legacy transaction from [`CALLER`].
pub(crate) fn tx(nonce: u64, to: Address, input: Bytes, gas_limit: u64) -> MegaTxEnvelope {
    let tx_legacy = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input,
    };
    MegaTxEnvelope::Legacy(Signed::new_unchecked(
        tx_legacy,
        Signature::test_signature(),
        B256::repeat_byte(nonce as u8 + 1),
    ))
}

/// A legacy transaction from [`CALLER`], with its sender recovered.
pub(crate) fn user_tx(nonce: u64, gas_limit: u64) -> Recovered<MegaTxEnvelope> {
    recovered(tx(nonce, CONTRACT, Bytes::new(), gas_limit))
}

/// The state gas one fresh storage slot costs at the byte prices in effect, in the minimum
/// bucket the tests' environment prices every slot in.
pub(crate) fn slot_state_gas() -> u64 {
    mega_evm::satin_gas_params().get(GasId::sstore_set_state_gas())
}

/// Whether a state byte costs nothing at the prices in effect: every state-gas entry of the
/// schedule is zero.
///
/// Only a measurement build arranges that, with `MEGA_SATIN_CPSB` at 0 or at a price every entry
/// rounds to nothing. A case whose scenario is state gas to cross a limit with has nothing to run
/// then, and is skipped; the notice goes to stderr once, since the harness keeps a passing test's
/// output to itself.
pub(crate) fn state_is_free() -> bool {
    use std::io::Write;

    let params = mega_evm::satin_gas_params();
    if !mega_evm::STATE_GAS_REPRICED.iter().all(|&(id, _)| params.get(id()) == 0) {
        return false;
    }
    static NOTICE: std::sync::Once = std::sync::Once::new();
    NOTICE.call_once(|| {
        let _ = writeln!(
            std::io::stderr(),
            "note: skipping the cases that need state gas, because MEGA_SATIN_CPSB prices a state \
             byte at nothing"
        );
    });
    true
}

/// The state gas one new account costs at the byte prices in effect.
pub(crate) fn new_account_state_gas() -> u64 {
    mega_evm::satin_gas_params().get(GasId::new_account_state_gas())
}

/// A Mega System Transaction: a legacy call from the system address the registry names to the
/// Oracle's `getSlot(0)`.
///
/// Its gas is 1,000,000 on top of the account the engine creates for its caller, which the
/// tests' state does not hold, so it runs the same whatever a state byte costs.
pub(crate) fn system_tx() -> Recovered<MegaTxEnvelope> {
    let input = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    Recovered::new_unchecked(
        tx(0, ORACLE_CONTRACT_ADDRESS, input.into(), 1_000_000 + new_account_state_gas()),
        MEGA_SYSTEM_ADDRESS,
    )
}

/// A legacy transaction carrying `input`.
pub(crate) fn user_tx_with_input(
    nonce: u64,
    input: Bytes,
    gas_limit: u64,
) -> Recovered<MegaTxEnvelope> {
    recovered(tx(nonce, CONTRACT, input, gas_limit))
}

/// A deposit transaction carrying `input`.
pub(crate) fn deposit_tx(input: Bytes, gas_limit: u64) -> Recovered<MegaTxEnvelope> {
    deposit_tx_to(CONTRACT, input, gas_limit)
}

/// A deposit transaction to `to`, carrying `input`.
pub(crate) fn deposit_tx_to(
    to: Address,
    input: Bytes,
    gas_limit: u64,
) -> Recovered<MegaTxEnvelope> {
    let deposit = TxDeposit {
        source_hash: B256::ZERO,
        from: CALLER,
        to: TxKind::Call(to),
        mint: 0,
        value: U256::ZERO,
        gas_limit,
        is_system_transaction: false,
        input,
    };
    recovered(MegaTxEnvelope::Deposit(Sealed::new_unchecked(deposit, B256::repeat_byte(0xde))))
}

/// A transaction with its sender recovered. Deposits carry no recoverable signature, so every
/// test transaction names [`CALLER`] as its sender.
pub(crate) fn recovered(tx: MegaTxEnvelope) -> Recovered<MegaTxEnvelope> {
    Recovered::new_unchecked(tx, CALLER)
}

/// `len` bytes that barely compress, so a transaction carrying them has a large
/// data-availability size. Deterministic, so every run estimates the same size.
pub(crate) fn incompressible(len: usize) -> Bytes {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    Bytes::from(
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect::<Vec<u8>>(),
    )
}
