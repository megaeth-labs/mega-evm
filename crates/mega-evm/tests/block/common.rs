//! Shared setup of the block-executor tests.
//!
//! A block on a chain whose schedule activates Satin at genesis, over an in-memory database holding
//! the seven predeploys as a chain that ran Rex6 holds them, and no L1 block contract — so the L1
//! fees, the operator fee and the data-availability footprint scalar all read as zero unless a test
//! writes them. Satin activates at genesis, so no block of these tests is its activation block.

use alloy_consensus::{transaction::Recovered, Sealed, Signed, TxLegacy};
use alloy_eips::{eip2935::HISTORY_STORAGE_ADDRESS, eip4788::BEACON_ROOTS_ADDRESS};
use alloy_evm::{EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        system_contract_specs, IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
    },
    test_utils::{BytecodeBuilder, MemoryDatabase, SequencerRegistryRoles},
    BlockLimits, EmptyExternalEnv, EvmTxRuntimeLimits, MegaBlockExecutionCtx, MegaBlockExecutor,
    MegaBlockExecutorFactory, MegaEvm, MegaEvmFactory, MegaHardforkConfig, MegaSpecId,
    MegaTxEnvelope, ProtocolLimits,
};
use op_alloy_consensus::TxDeposit;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::cfg::GasId,
    database::State,
    inspector::NoOpInspector,
    state::EvmState,
};

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

/// The database a block runs on: the seven predeploys as a chain that ran Rex6 holds them, the
/// registry holding [`registry_roles`]; the caller is funded and the callee has empty code.
pub(crate) fn database() -> MemoryDatabase {
    let mut db = MemoryDatabase::default().rex6_predeploys(registry_roles());
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

/// The sequencer the tests' registry names, distinct from the system address and admin.
pub(crate) const SEQUENCER: Address = address!("0x2222222222222222222222222222222222222222");

/// The admin the tests' registry names, distinct from the system address and sequencer.
pub(crate) const ADMIN: Address = address!("0x3333333333333333333333333333333333333333");

/// An address as the word the registry stores it as.
pub(crate) fn word(address: Address) -> U256 {
    U256::from_be_bytes(address.into_word().0)
}

/// Code that writes 1 into the slot named by the first byte of its calldata.
pub(crate) fn writer() -> Bytes {
    use revm::bytecode::opcode::{CALLDATALOAD, PUSH0, SHR, SSTORE, STOP};
    BytecodeBuilder::default()
        .push_number(1_u8)
        .append_many([PUSH0, CALLDATALOAD])
        .push_number(248_u8)
        .append_many([SHR, SSTORE, STOP])
        .build()
}

/// The roles the registry of [`database`] holds: the default system address, and a sequencer and
/// an admin of their own.
pub(crate) fn registry_roles() -> SequencerRegistryRoles {
    SequencerRegistryRoles {
        system_address: MEGA_SYSTEM_ADDRESS,
        sequencer: SEQUENCER,
        admin: ADMIN,
        min_rotation_delay: 100,
    }
}

/// A schedule that activates Satin at genesis.
///
/// Its blocks are held to the spec's limits ([`ProtocolLimits::for_spec`]), which no block this
/// size reaches but gas detention's caps; a test that holds a block or a transaction to another
/// limit replaces them ([`executor_with_limits`]).
pub(crate) fn chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default().with_all_activated()
}

/// The loosest limits a block could be held to while a read of volatile data still detains:
/// every limit unlimited, and gas detention's caps one below the most compute a transaction can
/// spend, which no transaction of a block this size reaches. For a test that holds a block to
/// one limit and no other.
pub(crate) const fn loosest() -> ProtocolLimits {
    ProtocolLimits::no_limits().with_tx_runtime_limits(loosest_tx())
}

/// The per-transaction half of [`loosest`].
pub(crate) const fn loosest_tx() -> EvmTxRuntimeLimits {
    let cap = mega_evm::constants::MAX_TX_COMPUTE_GAS - 1;
    EvmTxRuntimeLimits::no_limits()
        .with_block_env_access_compute_gas_limit(cap)
        .with_oracle_access_compute_gas_limit(cap)
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

/// An executor over `state`, for a block held to `limits` in place of the spec's, packed under no
/// building policy.
pub(crate) fn executor_with_limits(
    state: &mut State<MemoryDatabase>,
    limits: ProtocolLimits,
) -> TestExecutor<'_> {
    build(state, unlimited_ctx(), evm_env(), chain_spec()).with_protocol_limits(limits)
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

/// The one account each of `states` names, in commit order: what each pre-block step committed
/// is about. A pre-block call names its target, the activation block's check the predeploy it
/// read, the read of the registry's pending changes and the call that applies them the registry,
/// and the read of the L1 block info the L1 block contract.
///
/// # Panics
///
/// On a state that names more than one account, as a pre-block call whose code reaches another
/// contract does.
pub(crate) fn named_accounts(states: &[EvmState]) -> Vec<Address> {
    states
        .iter()
        .map(|state| {
            assert_eq!(state.len(), 1, "a pre-block step names one account: {state:?}");
            *state.keys().next().expect("one account")
        })
        .collect()
}

/// The committed state of the pre-block call to `target` among `states`: the first that names it.
///
/// # Panics
///
/// When no state names `target`.
pub(crate) fn call_state(states: &[EvmState], target: Address) -> &EvmState {
    states
        .iter()
        .find(|state| state.contains_key(&target))
        .unwrap_or_else(|| panic!("no committed state names {target}"))
}

/// The contracts the EIP-2935 and EIP-4788 calls committed in `states` targeted, in commit order:
/// a call's state names its target.
pub(crate) fn eip_calls(states: &[EvmState]) -> Vec<Address> {
    states
        .iter()
        .filter_map(|state| {
            [HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS]
                .into_iter()
                .find(|target| state.contains_key(target))
        })
        .collect()
}

/// Whether `state` is the activation block's check of a predeploy: one of the seven accounts
/// alone, read only, with no slot. The read of the registry's pending changes names the registry
/// alone too, with the slots it read.
pub(crate) fn is_predeploy_check(state: &EvmState) -> bool {
    state.len() == 1 &&
        system_contract_specs().iter().any(|spec| {
            state.get(&spec.address).is_some_and(|account| {
                !account.is_touched() && !account.is_created() && account.storage.is_empty()
            })
        })
}

/// Whether one of `states` is the `applyPendingChanges()` call: a state that touched the
/// registry. The read of its pending changes leaves the registry untouched.
pub(crate) fn applies_pending_changes(states: &[EvmState]) -> bool {
    states.iter().any(|state| {
        state.get(&SEQUENCER_REGISTRY_ADDRESS).is_some_and(|registry| registry.is_touched())
    })
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

/// A legacy transaction from `sender`.
///
/// The hash is derived from the sender and the nonce. Nothing in these tests pins it: a receipt
/// carries none, and a block that executes two transactions from one sender at one nonce tells
/// them apart by the sender's account, which is what the stale-outcome check reads.
pub(crate) fn legacy_from(
    sender: Address,
    nonce: u64,
    kind: TxKind,
    value: U256,
    input: Bytes,
    gas_limit: u64,
    gas_price: u128,
) -> Recovered<MegaTxEnvelope> {
    let tx =
        TxLegacy { chain_id: Some(CHAIN_ID), nonce, gas_price, gas_limit, to: kind, value, input };
    let hash = B256::from(alloy_primitives::keccak256(
        [sender.as_slice(), &nonce.to_be_bytes(), &[0u8]].concat(),
    ));
    Recovered::new_unchecked(
        MegaTxEnvelope::Legacy(Signed::new_unchecked(tx, Signature::test_signature(), hash)),
        sender,
    )
}

/// A legacy transaction from [`CALLER`].
pub(crate) fn tx(nonce: u64, to: Address, input: Bytes, gas_limit: u64) -> MegaTxEnvelope {
    legacy_from(CALLER, nonce, TxKind::Call(to), U256::ZERO, input, gas_limit, 1_000_000)
        .into_inner()
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
/// then, and is skipped; each test that skips one leaves a note (`note_price_guard`),
/// which the byte-price grid counts.
pub(crate) fn state_is_free() -> bool {
    let params = mega_evm::satin_gas_params();
    if !mega_evm::STATE_GAS_REPRICED.iter().all(|&(id, _)| params.get(id()) == 0) {
        return false;
    }
    mega_evm::test_utils::note_price_guard("MEGA_SATIN_CPSB prices a state byte at nothing");
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
