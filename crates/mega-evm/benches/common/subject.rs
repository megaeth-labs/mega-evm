//! Benchmark subjects: one per EVM stack being compared.
//!
//! A [`Subject`] turns a backend-agnostic [`Workload`] into a concrete run on
//! its own stack. The shared loop/assert/measure skeleton lives once in
//! [`run_workload`]; each subject only supplies how to *build* its EVM and how
//! to *execute* one tx, so the success assertion can never drift between stacks.
//!
//! What makes the vanilla rows comparable — the target hardfork and the
//! operator-fee zero-out — is defined once in the "Comparability baseline"
//! section below, not repeated per stack.

use alloy_primitives::{Bytes, U256};
use core::convert::Infallible;
use criterion::black_box;
use mega_evm::{
    revm::inspector::NoOpInspector, test_utils::MemoryDatabase, DeclaredObserver, EmptyExternalEnv,
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, TestExternalEnvs, TrustedObserver,
};
use op_revm::{
    DefaultOp as _, OpBuilder as _, OpContext as OpContextPinned, OpSpecId as OpSpecIdPinned,
    OpTransaction as OpTransactionPinned,
};
use revm::{
    context::{tx::TxEnvBuilder, TxEnv},
    database::EmptyDB as EmptyDBPinned,
    primitives::hardfork::SpecId as SpecIdPinned,
    Context as ContextPinned, ExecuteEvm, InspectEvm, Inspector, MainBuilder as _,
    MainContext as _,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::{cell::RefCell, rc::Rc};

use super::workload::{Account, TxSpec, Workload};

//
// ============================================================================
// Comparability baseline — single source of truth for what makes the vanilla
// rows comparable. Change a value here and every baseline moves together; CI's
// baseline-gap table assumes the rows all sit on one fork.
// ============================================================================
//

/// Target hardfork for the vanilla `revm` row. Cancun keeps every baseline on
/// one fork and predates EIP-7825's `tx_gas_limit_cap` (2^24), so the
/// multi-gigagas `gas_limit` workloads are not truncated
/// (`MainContext::mainnet()` would otherwise default to Osaka and trip the
/// cap).
const REVM_FORK: SpecIdPinned = SpecIdPinned::CANCUN;

/// Target hardfork for the op row. Holocene maps to eth Cancun, matching the
/// `revm` row above. Needed because `DefaultOp::op()` hard-codes `BEDROCK`
/// (eth Merge) regardless of the enum default — without this the op row would
/// sit on a different fork and the op-vs-revm gap would reflect a hardfork
/// difference rather than a version one.
const OP_FORK: OpSpecIdPinned = OpSpecIdPinned::HOLOCENE;

/// Zero the operator fee so the op and mega rows are comparable to the revm
/// rows, which carry no such fee. A macro rather than a fn keeps it
/// independent of the concrete `chain` type — only the field names matter.
macro_rules! zero_operator_fee {
    ($chain:expr) => {{
        $chain.operator_fee_scalar = Some(U256::ZERO);
        $chain.operator_fee_constant = Some(U256::ZERO);
    }};
}

//
// ============================================================================
// Subject trait + shared run skeleton.
// ============================================================================
//

/// One row of a criterion group — a specific EVM stack at a specific config.
pub trait Subject {
    /// Row name as it appears in the criterion group (e.g. `revm_pinned`).
    fn name(&self) -> &str;
    /// Build a fresh DB + EVM from the workload, run every tx on that one
    /// reused instance, and (unless the workload opts out) assert each
    /// succeeds. Implementations delegate to [`run_workload`].
    fn run(&self, workload: &Workload);
}

/// The one place the per-tx loop, success assertion, and `black_box` live.
///
/// `build` constructs the (stack-specific) EVM once; `exec` runs a single tx
/// and returns its `is_success()`. Keeping the skeleton here means the success
/// check can never drift or be forgotten on one stack — the bug that the old
/// per-stack `run` bodies were prone to. The generic `E` is inferred from
/// `build`, so no stack has to spell out its verbose revm `Evm<…>` type.
///
/// On failure the panic names the row and tx index; the concrete result is not
/// surfaced because `exec` has already abstracted away the stack's distinct
/// `ExecutionResult` type (it `black_box`es the result for the optimizer).
fn run_workload<E>(
    name: &str,
    workload: &Workload,
    build: impl FnOnce() -> E,
    exec: impl Fn(&mut E, &TxSpec) -> bool,
) {
    let mut evm = build();
    for (i, tx) in workload.txs.iter().enumerate() {
        let success = exec(&mut evm, tx);
        if workload.assert_success {
            assert!(success, "{name} tx #{i} should succeed");
        }
    }
}

//
// ============================================================================
// Database builders — written once, fed to every subject of each stack.
// ============================================================================
//

/// Seed a pinned-revm `MemoryDatabase` from the workload accounts. Shared by
/// the `*_pinned` baselines and every `mega_*` subject.
fn build_pinned_db(accounts: &[Account]) -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    for account in accounts {
        db = db.account_balance(account.address, account.balance);
        if let Some(code) = &account.code {
            db = db.account_code(account.address, code.clone());
        }
        for (slot, value) in &account.storage {
            db = db.account_storage(account.address, *slot, *value);
        }
    }
    db
}

/// Translate a [`TxSpec`] into a pinned-revm `TxEnv`, used by the `revm_pinned`,
/// `op_revm_pinned`, and `mega_*` subjects (mega-evm re-exports the same pinned
/// revm crate).
fn pinned_tx_env(tx: &TxSpec) -> TxEnv {
    TxEnvBuilder::new()
        .caller(tx.caller)
        .call(tx.target)
        .gas_limit(tx.gas_limit)
        .value(tx.value)
        .data(tx.data.clone())
        .build_fill()
}

//
// ============================================================================
// Mega subject.
// ============================================================================
//

/// `MegaEvm` at a single spec. One instance per benchmarked spec.
pub struct Mega {
    pub name: &'static str,
    pub spec: MegaSpecId,
}

impl Subject for Mega {
    fn name(&self) -> &str {
        self.name
    }

    fn run(&self, workload: &Workload) {
        let spec = self.spec;
        run_workload(
            self.name,
            workload,
            || {
                let mut context = MegaContext::new(build_pinned_db(&workload.accounts), spec);
                context.modify_chain(|chain| zero_operator_fee!(chain));
                MegaEvm::<_, NoOpInspector, EmptyExternalEnv>::new(context)
            },
            |evm, tx| {
                // Wrap into a `MegaTransaction` with an empty envelope, matching
                // what the production tx-pool would attach.
                let mut mega_tx = MegaTransaction(OpTransactionPinned::new(pinned_tx_env(tx)));
                mega_tx.enveloped_tx = Some(Bytes::new());
                let r = evm.transact(mega_tx).expect("mega transact");
                let success = r.result.is_success();
                black_box(r);
                success
            },
        );
    }
}

//
// ============================================================================
// Mega inspected-path subject.
// ============================================================================
//

/// Which inspector an inspected-path row attaches.
///
/// `NoOp` is the floor: the inspect loop and the measurement shim run, but the
/// inner inspector is empty, so the row is the cost of snapshotting.
/// `GethTracer` is `revm-inspectors`' `debug_traceTransaction` default — the
/// production tracer this crate already admits on the block path. `all()` is
/// not used: it clones memory on every opcode and is not an RPC default.
///
/// The two `*Trusted` kinds attach the same two inspectors through
/// [`MegaEvm::with_trusted_inspector`], so each pair differs only in whether
/// the shim measures. The gap between a pair is the measurement's whole cost,
/// and the trusted row is what the inspected path costs without it.
#[derive(Clone, Copy)]
pub enum InspectKind {
    NoOp,
    NoOpTrusted,
    GethTracer,
    GethTracerTrusted,
}

/// `MegaEvm` on the inspected frame loop, with the measurement shim live.
///
/// The plain [`Mega`] row calls `ExecuteEvm::transact`, which never enters an
/// inspector callback. This subject calls [`InspectEvm::inspect_tx`] after
/// [`MegaEvm::with_inspector`], which is the path RPC tracers take and the only
/// path the shim runs on.
pub struct MegaInspected {
    pub name: &'static str,
    pub spec: MegaSpecId,
    pub kind: InspectKind,
}

impl Subject for MegaInspected {
    fn name(&self) -> &str {
        self.name
    }

    fn run(&self, workload: &Workload) {
        match self.kind {
            InspectKind::NoOp => {
                run_inspected(self.name, self.spec, workload, || NoOpInspector);
            }
            InspectKind::NoOpTrusted => {
                run_inspected_trusted(self.name, self.spec, workload, || NoOpInspector);
            }
            InspectKind::GethTracer => {
                run_inspected(self.name, self.spec, workload, || {
                    TracingInspector::new(TracingInspectorConfig::default_geth())
                });
            }
            InspectKind::GethTracerTrusted => {
                run_inspected_trusted(self.name, self.spec, workload, || {
                    DeclaredObserver(TracingInspector::new(TracingInspectorConfig::default_geth()))
                });
            }
        }
    }
}

fn run_inspected<I, Make>(name: &str, spec: MegaSpecId, workload: &Workload, make_inspector: Make)
where
    I: Inspector<MegaContext<MemoryDatabase, EmptyExternalEnv>>,
    Make: FnOnce() -> I,
{
    run_workload(
        name,
        workload,
        || MegaEvm::new(inspected_context(spec, workload)).with_inspector(make_inspector()),
        inspect_one_tx,
    );
}

/// [`run_inspected`] through [`MegaEvm::with_trusted_inspector`].
///
/// Everything else is identical, which is the point: the pair of rows differs
/// only in whether the shim measures.
fn run_inspected_trusted<I, Make>(
    name: &str,
    spec: MegaSpecId,
    workload: &Workload,
    make_inspector: Make,
) where
    I: Inspector<MegaContext<MemoryDatabase, EmptyExternalEnv>> + TrustedObserver,
    Make: FnOnce() -> I,
{
    run_workload(
        name,
        workload,
        || MegaEvm::new(inspected_context(spec, workload)).with_trusted_inspector(make_inspector()),
        inspect_one_tx,
    );
}

/// The context both inspected variants build their EVM over.
fn inspected_context(
    spec: MegaSpecId,
    workload: &Workload,
) -> MegaContext<MemoryDatabase, EmptyExternalEnv> {
    let mut context = MegaContext::new(build_pinned_db(&workload.accounts), spec);
    context.modify_chain(|chain| zero_operator_fee!(chain));
    context
}

/// Runs one transaction on the inspected loop, for either variant.
fn inspect_one_tx<I>(evm: &mut MegaEvm<MemoryDatabase, I, EmptyExternalEnv>, tx: &TxSpec) -> bool
where
    I: Inspector<MegaContext<MemoryDatabase, EmptyExternalEnv>>,
{
    let mut mega_tx = MegaTransaction(OpTransactionPinned::new(pinned_tx_env(tx)));
    mega_tx.enveloped_tx = Some(Bytes::new());
    // `ExecuteEvm::transact` ignores `inspect` and stays on the plain
    // loop; `inspect_tx` is the inspected loop the shim actually sits on.
    let r = InspectEvm::inspect_tx(evm, mega_tx).expect("mega inspect");
    let success = r.result.is_success();
    black_box(r);
    success
}

//
// ============================================================================
// MegaWithEnv subject.
// ============================================================================
//

/// `MegaEvm` at a single spec over a configurable `TestExternalEnvs` (crowded
/// SALT buckets and/or oracle storage). Used by the SALT and oracle benches.
/// The `env` is cloned per build so each iteration starts from the same config.
pub struct MegaWithEnv {
    pub name: &'static str,
    pub spec: MegaSpecId,
    pub env: TestExternalEnvs<Infallible>,
}

impl Subject for MegaWithEnv {
    fn name(&self) -> &str {
        self.name
    }

    fn run(&self, workload: &Workload) {
        let spec = self.spec;
        let env = self.env.clone();
        run_workload(
            self.name,
            workload,
            || {
                let db = build_pinned_db(&workload.accounts);
                let salt = Rc::new(env.clone());
                let oracle = Rc::new(RefCell::new(env.clone()));
                let mut context = MegaContext::new_with_ext_envs(db, spec, salt, oracle);
                context.modify_chain(|chain| zero_operator_fee!(chain));
                MegaEvm::<_, NoOpInspector, TestExternalEnvs<Infallible>>::new(context)
            },
            |evm, tx| {
                let mut mega_tx = MegaTransaction(OpTransactionPinned::new(pinned_tx_env(tx)));
                mega_tx.enveloped_tx = Some(Bytes::new());
                let r = evm.transact(mega_tx).expect("mega transact");
                let success = r.result.is_success();
                black_box(r);
                success
            },
        );
    }
}

//
// ============================================================================
// Baseline subjects. Fork pins and the operator-fee zero-out come from the
// Comparability baseline section above.
// ============================================================================
//

/// Vanilla `revm` at the version mega-evm currently pins (fork: [`REVM_FORK`]).
pub struct RevmPinned;

impl Subject for RevmPinned {
    fn name(&self) -> &str {
        "revm_pinned"
    }

    fn run(&self, workload: &Workload) {
        run_workload(
            self.name(),
            workload,
            || {
                ContextPinned::mainnet()
                    // Sets the gas params along with the spec: revm 40 keeps
                    // per-spec `GasParams` in `CfgEnv`, so assigning `cfg.spec`
                    // alone would leave the default (Osaka) params in place.
                    .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(REVM_FORK))
                    .with_db(build_pinned_db(&workload.accounts))
                    .build_mainnet()
            },
            |evm, tx| {
                let r = evm.transact(pinned_tx_env(tx)).expect("revm_pinned transact");
                let success = r.result.is_success();
                black_box(r);
                success
            },
        );
    }
}

/// `op-revm` at the version mega-evm currently pins (fork: [`OP_FORK`],
/// operator fee = 0).
pub struct OpRevmPinned;

impl Subject for OpRevmPinned {
    fn name(&self) -> &str {
        "op_revm_pinned"
    }

    fn run(&self, workload: &Workload) {
        run_workload(
            self.name(),
            workload,
            || {
                let mut ctx = <OpContextPinned<EmptyDBPinned>>::op()
                    .with_db(build_pinned_db(&workload.accounts));
                // Sets the gas params along with the spec: `DefaultOp::op()`
                // seeds `CfgEnv` with BEDROCK gas params, so assigning
                // `cfg.spec` alone would leave those in place.
                ctx.modify_cfg(|cfg| cfg.set_spec_and_mainnet_gas_params(OP_FORK));
                ctx.modify_chain(|chain| zero_operator_fee!(chain));
                ctx.build_op()
            },
            |evm, tx| {
                let mut op_tx = OpTransactionPinned::new(pinned_tx_env(tx));
                op_tx.enveloped_tx = Some(Bytes::new());
                let r = evm.transact(op_tx).expect("op_revm_pinned transact");
                let success = r.result.is_success();
                black_box(r);
                success
            },
        );
    }
}
