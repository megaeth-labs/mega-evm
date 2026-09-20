//! Shared setup of the block-executor tests.
//!
//! A block on a chain whose schedule activates Satin at genesis, over an in-memory database with
//! an empty L1 block contract — so the L1 fees, the operator fee and the data-availability
//! footprint scalar all read as zero unless a test writes them.

use alloy_consensus::{transaction::Recovered, Sealed, Signed, TxLegacy};
use alloy_evm::{EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, EmptyExternalEnv, MegaBlockExecutionCtx,
    MegaBlockExecutor, MegaBlockExecutorFactory, MegaEvm, MegaEvmFactory, MegaHardforkConfig,
    MegaSpecId, MegaTxEnvelope,
};
use op_alloy_consensus::TxDeposit;
use revm::{
    context::{BlockEnv, CfgEnv},
    database::State,
    inspector::NoOpInspector,
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

/// A schedule that activates Satin at genesis.
pub(crate) fn chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default().with_all_activated()
}

/// The context of a block held to `limits`.
///
/// Cancun is active, so the block carries a parent beacon block root; without one the EIP-4788
/// pre-block call refuses the block.
pub(crate) fn block_ctx(limits: BlockLimits) -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(B256::ZERO, Some(B256::ZERO), Bytes::new(), limits)
}

/// The context of a block with no limits at all.
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
pub(crate) fn factory() -> MegaBlockExecutorFactory<
    OpAlloyReceiptBuilder,
    MegaHardforkConfig,
    MegaEvmFactory<EmptyExternalEnv>,
> {
    MegaBlockExecutorFactory::new(
        OpAlloyReceiptBuilder::default(),
        chain_spec(),
        MegaEvmFactory::new(),
    )
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

/// An executor over `state`, for a block on the chain `spec` describes.
pub(crate) fn executor_with_spec(
    state: &mut State<MemoryDatabase>,
    ctx: MegaBlockExecutionCtx,
    spec: MegaHardforkConfig,
) -> TestExecutor<'_> {
    build(state, ctx, evm_env(), spec)
}

fn build(
    state: &mut State<MemoryDatabase>,
    ctx: MegaBlockExecutionCtx,
    env: EvmEnv<MegaSpecId>,
    spec: MegaHardforkConfig,
) -> TestExecutor<'_> {
    let evm = MegaEvmFactory::new()
        .create_evm(state, env)
        .with_tx_runtime_limits(ctx.block_limits.to_evm_tx_runtime_limits());
    MegaBlockExecutor::new(evm, ctx, spec, OpAlloyReceiptBuilder::default())
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
    let deposit = TxDeposit {
        source_hash: B256::ZERO,
        from: CALLER,
        to: TxKind::Call(CONTRACT),
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
