//! Shared helpers for the REX7 test suite.

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    ConservationTerms, EvmTxRuntimeLimits, ExternalEnvTypes, InspectorLedger, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionNew as _, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{DUP1, JUMPDEST, JUMPI, POP, STOP, SUB, SWAP1},
    context::{result::ExecutionResult, tx::TxEnvBuilder, TxEnv},
    handler::EvmTr,
    state::EvmState,
    Inspector,
};
use std::{collections::BTreeMap, string::String, vec::Vec};

/// Transaction sender.
pub(crate) const CALLER: Address = address!("0000000000000000000000000000000000300000");
/// Contract invoked by the transaction; its code exercises the opcodes under test.
pub(crate) const CONTRACT: Address = address!("0000000000000000000000000000000000300001");
/// A second contract, used as the target of internal CALL-family frames.
pub(crate) const CALLEE: Address = address!("0000000000000000000000000000000000300002");
/// A spare empty address used as a value-transfer / SELFDESTRUCT target.
pub(crate) const EMPTY_TARGET: Address = address!("0000000000000000000000000000000000300003");

/// One ether, in wei.
pub(crate) const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// The transaction gas limit [`transact`] runs with — high enough that EVM gas is never the
/// binding constraint.
pub(crate) const DEFAULT_TX_GAS_LIMIT: u64 = 100_000_000;

/// The standard fixture: a funded [`CALLER`], `code` at [`CONTRACT`], and a balance there for the
/// value transfers and SELFDESTRUCTs the fixtures make.
pub(crate) fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// `pairs` PUSH/POP pairs: plain opcodes that touch nothing, for padding a segment out to a known
/// compute cost.
pub(crate) fn plain_filler(builder: BytecodeBuilder, pairs: usize) -> BytecodeBuilder {
    let mut builder = builder;
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder
}

/// A countdown loop of plain opcodes with no checkpoint anywhere in the body, after `prefix`, so
/// the run is one settlement segment and the gas clamp is the only thing enforcing the compute
/// limit inside it.
pub(crate) fn countdown_loop_code(prefix: &[u8], iterations: u16) -> Bytes {
    let mut code = prefix.to_vec();
    code.push(0x61); // PUSH2
    code.extend_from_slice(&iterations.to_be_bytes());
    let loop_target = u8::try_from(code.len()).expect("loop target must fit in a PUSH1");
    code.push(JUMPDEST);
    code.extend_from_slice(&[0x60, 0x01]); // PUSH1 1
    code.push(SWAP1);
    code.push(SUB);
    code.push(DUP1);
    code.extend_from_slice(&[0x60, loop_target]); // PUSH1 loop
    code.push(JUMPI);
    code.push(STOP);
    Bytes::from(code)
}

/// A contract that does nothing, for measuring what a transaction costs before its code runs.
pub(crate) fn stop_only() -> Bytes {
    BytecodeBuilder::default().append(STOP).build()
}

/// The spec's default runtime limits with the per-transaction compute budget lowered to `limit`.
pub(crate) fn compute_limit(limit: u64) -> impl Fn(MegaSpecId) -> EvmTxRuntimeLimits {
    move |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(limit)
}

/// [`compute_limit`] under REX7, for the files that never run a second spec.
pub(crate) fn rex7_compute_limit(limit: u64) -> EvmTxRuntimeLimits {
    compute_limit(limit)(MegaSpecId::REX7)
}

/// The spec's default runtime limits with the block-env detention cap lowered to `cap`.
pub(crate) fn detention_cap(cap: u64) -> impl Fn(MegaSpecId) -> EvmTxRuntimeLimits {
    move |spec| {
        let mut limits = EvmTxRuntimeLimits::from_spec(spec);
        limits.block_env_access_compute_gas_limit = cap;
        limits
    }
}

/// A deterministic pre-EIP-155 keyless deployment transaction, RLP-encoded.
///
/// The signature is Nick's Method's: an unrecoverable `r == s` that no key produced, which is what
/// makes the sender deterministic and the deployment address the same on every chain.
pub(crate) fn keyless_tx_bytes(init_code: Bytes, gas_limit: u64) -> Bytes {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
        chain_id: None,
    };
    let word = U256::from_be_bytes(hex!(
        "3333333333333333333333333333333333333333333333333333333333333333"
    ));
    let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    Bytes::from(buf)
}

/// The post-transaction readings compared across specs.
pub(crate) struct Outcome {
    pub(crate) result: ExecutionResult<MegaHaltReason>,
    /// Post-tx compute-gas tracker reading (`get_usage().compute_gas`).
    pub(crate) compute_gas: u64,
    /// Post-tx data-size tracker reading (`get_usage().data_size`).
    pub(crate) data_size: u64,
    /// Post-tx KV-update tracker reading (`get_usage().kv_updates`).
    pub(crate) kv_updates: u64,
    /// Post-tx state-growth tracker reading (`get_usage().state_growth`).
    pub(crate) state_growth: u64,
    /// Receipt `gas_used` (combined compute + storage EVM gas).
    pub(crate) gas_used: u64,
    /// The part of [`compute_gas`](Self::compute_gas) an exceptionally halted frame destroyed
    /// rather than performed (REX7+, else 0).
    pub(crate) destroyed: u64,
    /// Post-tx enforced compute gas — the part of [`compute_gas`](Self::compute_gas) every limit
    /// comparison and the block's admission counter run against.
    enforced_lane: u64,
    /// Receipt envelope before the EIP-3529 refund and the EIP-7623 floor: exactly the number
    /// settlement derives the destroyed total from.
    pub(crate) total_gas_spent: u64,
    /// Post-tx detained compute gas limit — equal to the configured TX limit unless volatile
    /// access lowered it.
    pub(crate) detained_compute_gas_limit: u64,
    /// The conservation law's terms, as the tracker held them when the transaction ended.
    pub(crate) terms: ConservationTerms,
    /// What the measurement shim booked for this transaction, as the outcome reports it.
    pub(crate) inspector_ledger: InspectorLedger,
    /// The state the transaction produced.
    pub(crate) state: EvmState,
}

impl Outcome {
    pub(crate) fn is_success(&self) -> bool {
        self.result.is_success()
    }

    /// The halt reason, or a panic with `label` when the transaction did not halt.
    pub(crate) fn halt_reason(&self, label: &str) -> &MegaHaltReason {
        match &self.result {
            ExecutionResult::Halt { reason, .. } => reason,
            other => panic!("{label}: expected a halt, got {other:?}"),
        }
    }

    /// The part of the reported compute total that a resource limit is evaluated against.
    ///
    /// Read from the tracker's own lane rather than subtracted from the reported total; the two
    /// agree because [`assert_terminal_identity`] checks that they do on every transaction the
    /// helpers in this module run.
    pub(crate) fn enforced(&self) -> u64 {
        self.enforced_lane
    }

    /// `S` — `MegaETH` storage gas plus the sandbox boundary residue. Signed.
    pub(crate) fn non_compute_gas(&self) -> i128 {
        self.terms.non_compute_gas
    }

    /// `K` — the `CALL_STIPEND` total minted into child frames by value-transferring calls.
    pub(crate) fn minted_call_stipend(&self) -> u64 {
        self.terms.minted_call_stipend
    }

    /// The sum of the per-site destroyed bookings — the second opinion the derived
    /// [`destroyed`](Self::destroyed) is cross-checked against, never the reported number.
    pub(crate) fn booked_destroyed(&self) -> u64 {
        self.terms.booked_destroyed_compute_gas
    }

    /// `I` — the net gas an inspector conjured. Zero for every transaction that ran without one,
    /// and for every observation-only inspector.
    pub(crate) fn inspector_conjured_gas(&self) -> i128 {
        self.terms.inspector_conjured_gas
    }

    /// The receipt's raw EIP-3529 refund, before the cap that decides how much of it applies.
    pub(crate) fn refunded(&self) -> u64 {
        self.result.gas().inner_refunded()
    }

    /// The receipt's final EIP-8037 state-gas spend.
    pub(crate) fn state_gas_spent(&self) -> u64 {
        self.result.gas().state_gas_spent_final()
    }

    /// The non-compute part of what this transaction's receipt reports — the `MegaETH` storage
    /// gas and intrinsic share a compute-gas figure does not cover.
    pub(crate) fn storage_overhead(&self) -> u64 {
        self.gas_used - self.compute_gas
    }

    /// Reads a storage slot out of the produced state, defaulting to zero when the transaction
    /// never touched it.
    pub(crate) fn storage_value(&self, address: Address, slot: U256) -> U256 {
        self.state
            .get(&address)
            .and_then(|account| account.storage.get(&slot))
            .map(|value| value.present_value())
            .unwrap_or_default()
    }
}

/// The transaction every helper here runs unless a test supplies its own: a plain call from
/// [`CALLER`] into [`CONTRACT`].
pub(crate) fn call_contract_tx(gas_limit: u64) -> MegaTransaction {
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(gas_limit).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Zeroes the operator fee, which otherwise adds an L1 charge to every receipt the suite reads.
pub(crate) fn zero_operator_fee<EXT: ExternalEnvTypes>(
    mut context: MegaContext<&mut MemoryDatabase, EXT>,
) -> MegaContext<&mut MemoryDatabase, EXT> {
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context
}

/// The context every helper here runs on: `spec`, `limits`, no operator fee, no external
/// environment.
pub(crate) fn context(
    db: &mut MemoryDatabase,
    spec: MegaSpecId,
    limits: EvmTxRuntimeLimits,
) -> MegaContext<&mut MemoryDatabase, mega_evm::EmptyExternalEnv> {
    zero_operator_fee(MegaContext::new(db, spec).with_tx_runtime_limits(limits))
}

/// Runs `tx` on `evm` and assembles the [`Outcome`], checking the terminal identity before handing
/// it back.
///
/// Every helper in this module funnels through here, and so does every test that builds its own
/// EVM — so every REX7 transaction the suite runs, not just the ones written to look at gas, is a
/// check that the tracker lanes reconcile with the receipt the transaction produced.
pub(crate) fn drive<'db, INSP, EXT>(
    spec: MegaSpecId,
    evm: &mut MegaEvm<&'db mut MemoryDatabase, INSP, EXT>,
    tx: MegaTransaction,
) -> Outcome
where
    INSP: Inspector<MegaContext<&'db mut MemoryDatabase, EXT>>,
    EXT: ExternalEnvTypes,
{
    try_drive(spec, evm, tx)
        .unwrap_or_else(|refusal| panic!("tx should not surface EVMError, got {}", refusal.error))
}

/// [`drive`] for a run the shim may refuse.
///
/// A refusal produces no receipt at all, so there is no [`Outcome`] to read and the two numbers a
/// [`Refusal`] carries are the whole of what such a run leaves behind.
pub(crate) fn try_drive<'db, INSP, EXT>(
    spec: MegaSpecId,
    evm: &mut MegaEvm<&'db mut MemoryDatabase, INSP, EXT>,
    tx: MegaTransaction,
) -> Result<Outcome, Refusal>
where
    INSP: Inspector<MegaContext<&'db mut MemoryDatabase, EXT>>,
    EXT: ExternalEnvTypes,
{
    let executed = evm.execute_transaction(tx);
    let (detained_compute_gas_limit, terms, tracker_ledger) = {
        let additional_limit = EvmTr::ctx_ref(evm).additional_limit.borrow();
        (
            additional_limit.detained_compute_gas_limit(),
            additional_limit.conservation_terms(),
            additional_limit.inspector_ledger(),
        )
    };
    let outcome = match executed {
        Ok(outcome) => outcome,
        Err(e) => {
            return Err(Refusal {
                error: std::format!("{e:?}"),
                rejected_rewrites: tracker_ledger.rejected_rewrites,
            })
        }
    };
    assert_eq!(
        outcome.inspector_ledger, tracker_ledger,
        "the outcome must report the ledger the shim booked, unchanged",
    );
    let gas_used = outcome.result_and_state.result.tx_gas_used();
    let total_gas_spent = outcome.result_and_state.result.gas().total_gas_spent();
    let outcome = Outcome {
        result: outcome.result_and_state.result,
        compute_gas: outcome.compute_gas_used,
        data_size: outcome.data_size,
        kv_updates: outcome.kv_updates,
        state_growth: outcome.state_growth_used,
        gas_used,
        destroyed: outcome.compute_gas_destroyed,
        enforced_lane: outcome.compute_gas_enforced,
        total_gas_spent,
        detained_compute_gas_limit,
        terms,
        inspector_ledger: outcome.inspector_ledger,
        state: outcome.result_and_state.state,
    };
    assert_terminal_identity(spec, &outcome);
    Ok(outcome)
}

/// Runs a single transaction that calls [`CONTRACT`] under `spec` with the given DB and runtime
/// limits, returning the execution result plus the post-tx tracker readings and `gas_used`.
pub(crate) fn transact(
    spec: MegaSpecId,
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
) -> Outcome {
    transact_with_gas_limit(spec, db, limits, DEFAULT_TX_GAS_LIMIT)
}

/// [`transact`] with an explicit transaction gas limit, for cases that need EVM gas itself to run
/// out.
pub(crate) fn transact_with_gas_limit(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    gas_limit: u64,
) -> Outcome {
    let mut evm = MegaEvm::new(context(&mut db, spec, limits));
    drive(spec, &mut evm, call_contract_tx(gas_limit))
}

/// Runs [`transact`] with the spec's default runtime limits.
pub(crate) fn transact_default(spec: MegaSpecId, db: MemoryDatabase) -> Outcome {
    transact(spec, db, EvmTxRuntimeLimits::from_spec(spec))
}

/// [`transact`] with an inspector attached, borrowed so the caller can read it back afterwards.
///
/// Runs the same fixture through the inspected frame loops. The identity every other helper here
/// checks holds on this path too, with the inspector's own term in it — which is the point: a
/// rewriting inspector must leave the transaction's numbers accountable, not merely plausible.
pub(crate) fn transact_inspected<I>(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    inspector: &mut I,
) -> Outcome
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, mega_evm::EmptyExternalEnv>>,
{
    let mut evm = MegaEvm::new(context(&mut db, spec, limits)).with_inspector(inspector);
    drive(spec, &mut evm, call_contract_tx(DEFAULT_TX_GAS_LIMIT))
}

/// What a transaction the shim refused reports: the error it surfaced and the refusals counted.
///
/// A refused rewrite produces no receipt at all, so there is no [`Outcome`] to read — these two
/// numbers are the whole of what such a run leaves behind.
pub(crate) struct Refusal {
    /// The `EVMError` the refusal surfaced, rendered.
    pub(crate) error: String,
    /// How many rewrites the shim refused over the transaction.
    pub(crate) rejected_rewrites: u32,
}

/// [`transact_inspected`] for a run the shim is expected to refuse.
///
/// Panics when the transaction produced a receipt, so a fixture that stops reaching the refused
/// shape fails rather than passing as a run that was never refused.
pub(crate) fn transact_inspected_refused<I>(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    inspector: &mut I,
) -> Refusal
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, mega_evm::EmptyExternalEnv>>,
{
    let mut evm = MegaEvm::new(context(&mut db, spec, limits)).with_inspector(inspector);
    match try_drive(spec, &mut evm, call_contract_tx(DEFAULT_TX_GAS_LIMIT)) {
        Ok(outcome) => {
            panic!("the run was expected to be refused, but produced {:?}", outcome.result)
        }
        Err(refusal) => refusal,
    }
}

/// The external environment [`transact_tx`] runs with when a test does not need SALT buckets or
/// oracle storage of its own. Equivalent to the empty environment the other helpers use: every
/// bucket reports the minimum capacity and the oracle has no data.
pub(crate) fn default_envs() -> TestExternalEnvs {
    TestExternalEnvs::new()
}

/// The general entry point: an explicit transaction and an explicit external environment.
///
/// The other helpers in this module fix the transaction to a plain call into [`CONTRACT`]; the
/// shapes that need a different one — EIP-7702 authorizations, system-originated callers, direct
/// calls into a system contract — build their own `TxEnv` and come through here. `envs` is borrowed
/// so a test can read back what execution recorded into it (oracle hints, for instance).
pub(crate) fn transact_tx(
    spec: MegaSpecId,
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: TxEnv,
    envs: &TestExternalEnvs,
) -> Outcome {
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    transact_mega_tx(spec, db, limits, tx, envs)
}

/// [`transact_tx`] for the shapes that need the `MegaETH` transaction itself, not just its
/// `TxEnv` — a deposit's `source_hash` and `mint` live on the outer type.
pub(crate) fn transact_mega_tx(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: MegaTransaction,
    envs: &TestExternalEnvs,
) -> Outcome {
    let context = zero_operator_fee(
        MegaContext::new(&mut db, spec)
            .with_external_envs(envs.into())
            .with_tx_runtime_limits(limits),
    );
    let mut evm = MegaEvm::new(context);
    drive(spec, &mut evm, tx)
}

/// [`transact`] with every SALT bucket reporting `bucket_capacity`.
///
/// The SALT-scaled storage-gas charges (`SSTORE` set, new account, contract creation) are
/// `base × (capacity / MIN_BUCKET_SIZE − 1)`, so only a capacity above
/// [`mega_evm::MIN_BUCKET_SIZE`] makes them non-zero and exercises the paths that have to
/// exclude them from the compute-gas window.
pub(crate) fn transact_with_bucket_capacity(
    spec: MegaSpecId,
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    bucket_capacity: u64,
) -> Outcome {
    let envs = TestExternalEnvs::default().with_default_bucket_capacity(bucket_capacity);
    transact_tx(
        spec,
        db,
        limits,
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(DEFAULT_TX_GAS_LIMIT)
            .build_fill(),
        &envs,
    )
}

/// The identity every REX7 transaction that produces a receipt must satisfy, connecting what the
/// trackers hold to the number the receipt reports.
///
/// # The identity
///
/// For one transaction, write
///
/// ```text
/// C = compute_gas         reported compute total
/// E = enforced_lane       the part limits and block admission compare against
/// D = destroyed           the part that is reported and accounted but never enforced
/// N = non_compute_gas     MegaETH storage gas plus the sandbox boundary residue (signed)
/// M = minted_call_stipend CALL_STIPEND minted into child frames and never debited from a caller
/// I = inspector_conjured_gas gas an inspector wrote into the execution that nothing debited
/// S = total_gas_spent     the receipt envelope, before the refund and the floor
/// R = the receipt's raw refund
/// F = the receipt's EIP-7623 floor gas
/// ```
///
/// then
///
/// ```text
/// (1)  C = E + D
/// (2)  E + N + D − M − I = S
/// (3)  I = the ledger's own net
/// (4)  receipt gas_used = max(S − R, F)
/// ```
///
/// (1) is the split of the reported total. (2) is `ConservationTerms::envelope_for`, the law
/// solved for the envelope; substituting (1) gives the equivalent receipt-facing form
/// `C + N − M − I = S`. (3) pins the law's inspector term to the ledger it is read from, so a
/// lane the shim books but the law never sees cannot pass. (4) is how a receipt's gas number is
/// built from its envelope.
///
/// `I` is zero for every transaction that runs without an inspector and for every
/// observation-only one, so (2) is the plain two-term identity on all but the handful of runs
/// that attach a rewriting inspector — which is exactly where it earns its keep.
///
/// # Why (2) needs no refund or floor correction
///
/// The EIP-3529 refund and the EIP-7623 floor both move the number the receipt reports without
/// anyone having burnt the difference. Both are applied strictly after the envelope is final, and
/// both are carried on the result as their own fields rather than folded into the envelope, so
/// anchoring on `S` — the same value settlement reads — keeps them out of the identity entirely.
/// Substituting (2) into (4) gives the receipt-level form, which is what a reader normally wants:
///
/// ```text
/// receipt gas_used = max(C + N − M − R, F)
/// ```
///
/// # What it catches
///
/// (2) fails whenever a transaction's envelope moves without a `MegaETH` site accounting for it —
/// a settlement that never ran, a result rewritten after settlement, an upstream subsidy nobody
/// records. (1) fails when the reported split disagrees with the per-site bookings, which is what
/// the block's admission counter reads. Pre-REX7 specs have neither a destroyed lane nor a
/// non-compute lane, so (1) and (2) are REX7-only by construction; (3) is not, because the shim
/// is not spec-gated.
fn assert_terminal_identity(spec: MegaSpecId, outcome: &Outcome) {
    assert_eq!(
        outcome.terms.inspector_conjured_gas,
        outcome.inspector_ledger.conjured_gas(),
        "the law's `I` term is the ledger's net, and nothing else",
    );
    if !spec.is_enabled(MegaSpecId::REX7) {
        return;
    }
    assert_eq!(
        outcome.compute_gas,
        outcome.enforced_lane + outcome.destroyed,
        "reported compute gas must split into enforced + destroyed; \
         compute={} enforced={} destroyed={} result={:?}",
        outcome.compute_gas,
        outcome.enforced_lane,
        outcome.destroyed,
        outcome.result,
    );
    assert_eq!(
        outcome.terms.envelope_for(outcome.destroyed),
        i128::from(outcome.total_gas_spent),
        "the tracker lanes must account for the whole receipt envelope; \
         reported compute={} destroyed={} envelope={} (receipt gas_used={}) result={:?} ({})",
        outcome.compute_gas,
        outcome.destroyed,
        outcome.total_gas_spent,
        outcome.gas_used,
        outcome.result,
        outcome.terms,
    );
}

/// The part of an account a transaction's state actually asserts.
///
/// Raw [`EvmState`] cannot be compared directly: `Account::transaction_id` and each storage slot's
/// `is_cold` are journal bookkeeping with no consensus meaning, and two runs that produce identical
/// state can still differ there. This keeps the account info, the deployed code, the status flags
/// that decide how the account is applied, and every slot's original/present pair.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AccountView {
    balance: U256,
    nonce: u64,
    code_hash: B256,
    code: Bytes,
    touched: bool,
    created: bool,
    selfdestructed: bool,
    loaded_as_not_existing: bool,
    storage: BTreeMap<U256, (U256, U256)>,
}

/// Normalises an [`EvmState`] into a stable, order-independent view.
pub(crate) fn state_view(state: &EvmState) -> BTreeMap<Address, AccountView> {
    state
        .iter()
        .map(|(address, account)| {
            let view = AccountView {
                balance: account.info.balance,
                nonce: account.info.nonce,
                code_hash: account.info.code_hash,
                code: account
                    .info
                    .code
                    .as_ref()
                    .map(|code| code.original_bytes())
                    .unwrap_or_default(),
                touched: account.is_touched(),
                created: account.is_created(),
                selfdestructed: account.is_selfdestructed(),
                loaded_as_not_existing: account.is_loaded_as_not_existing(),
                storage: account
                    .storage
                    .iter()
                    .map(|(slot, value)| (*slot, (value.original_value, value.present_value)))
                    .collect(),
            };
            (*address, view)
        })
        .collect()
}

/// Asserts that two outcomes are indistinguishable: same execution result, same four-dimension
/// usage, same receipt `gas_used`, the same detained compute-gas limit, and the same state.
///
/// This is the precision invariant in assertion form — what a transaction that stays inside every
/// per-tx limit must produce under both accounting models.
pub(crate) fn assert_outcomes_identical(label: &str, r6: &Outcome, r7: &Outcome) {
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "{label}: execution result must be identical; REX6={:?} REX7={:?}",
        r6.result,
        r7.result
    );
    assert_eq!(
        r6.compute_gas, r7.compute_gas,
        "{label}: compute gas must be identical; REX6={} REX7={}",
        r6.compute_gas, r7.compute_gas
    );
    assert_eq!(
        r6.gas_used, r7.gas_used,
        "{label}: receipt gas_used must be identical; REX6={} REX7={}",
        r6.gas_used, r7.gas_used
    );
    assert_eq!(
        (r6.data_size, r6.kv_updates, r6.state_growth),
        (r7.data_size, r7.kv_updates, r7.state_growth),
        "{label}: the non-compute dimensions must be identical",
    );
    assert_eq!(
        r6.detained_compute_gas_limit, r7.detained_compute_gas_limit,
        "{label}: the detained compute-gas limit must be identical; REX6={} REX7={}",
        r6.detained_compute_gas_limit, r7.detained_compute_gas_limit
    );
    let (s6, s7) = (state_view(&r6.state), state_view(&r7.state));
    if s6 != s7 {
        // Report the first address the two disagree on; dumping both whole states buries it.
        let mut addresses: Vec<&Address> = s6.keys().chain(s7.keys()).collect();
        addresses.sort_unstable();
        addresses.dedup();
        let culprit = addresses
            .into_iter()
            .find(|address| s6.get(*address) != s7.get(*address))
            .expect("the maps differ, so some address must");
        panic!(
            "{label}: the produced state must be identical; {culprit} is\n  REX6: {:?}\n  REX7: {:?}",
            s6.get(culprit),
            s7.get(culprit),
        );
    }
}
