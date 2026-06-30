//! Shared bench harness comparing mega-evm against vanilla revm and op-revm.
//!
//! A bench declares a backend-agnostic [`Workload`] (accounts + transactions)
//! and registers it across a fixed set of [`Subject`](subject::Subject) rows:
//! the four vanilla baselines (`revm_pinned`, `revm_latest`, `op_revm_pinned`,
//! `op_revm_latest`) and the mega specs. Every row runs the same scenario, so a
//! single criterion group yields a comparable gap table.
//!
//! Bench files pull this in with a plain `mod common;` (resolved via the
//! standard `common/mod.rs` sibling-folder lookup). Each criterion bench target
//! compiles as its own binary, so this module is compiled once per target.

#![allow(dead_code)] // each bench target uses a subset of the harness
#![allow(unreachable_pub)] // included as a shared bench module, so `pub` items appear unreachable in lint terms

pub mod subject;
pub mod workload;

use core::convert::Infallible;

use mega_evm::{MegaSpecId, TestExternalEnvs};
pub use subject::MegaWithEnv;
use subject::{Mega, OpRevmLatest, OpRevmPinned, RevmLatest, RevmPinned, Subject};
pub use workload::{Account, TxSpec, Workload};

/// Mega specs registered by [`register_all`] and [`register_mega`]. Shared so
/// every bench file emits the same mega rows. Benches needing a different set
/// (single spec, SELFDESTRUCT-relevant specs, …) pass their own list to
/// [`register_mega_specs`].
pub const SPEC_IDS: &[(&str, MegaSpecId)] = &[
    ("equivalence", MegaSpecId::EQUIVALENCE),
    ("mini_rex", MegaSpecId::MINI_REX),
    ("rex4", MegaSpecId::REX4),
    ("rex5", MegaSpecId::REX5),
];

type Group<'a> = criterion::BenchmarkGroup<'a, criterion::measurement::WallTime>;

fn baseline_subjects() -> Vec<Box<dyn Subject>> {
    vec![Box::new(RevmPinned), Box::new(RevmLatest), Box::new(OpRevmPinned), Box::new(OpRevmLatest)]
}

fn mega_subjects(specs: &[(&'static str, MegaSpecId)]) -> Vec<Box<dyn Subject>> {
    specs.iter().map(|&(name, spec)| Box::new(Mega { name, spec }) as Box<dyn Subject>).collect()
}

/// Run each subject as one row of `group`, named `<subject>` or, when `variant`
/// is non-empty, `<subject>/<variant>` (e.g. `revm_pinned/log0_32b`) so every
/// row shares a single variant axis.
fn run_subjects(group: &mut Group<'_>, variant: &str, w: &Workload, subjects: &[Box<dyn Subject>]) {
    for subject in subjects {
        let row = if variant.is_empty() {
            subject.name().to_string()
        } else {
            format!("{}/{variant}", subject.name())
        };
        group.bench_function(row, |b| b.iter(|| subject.run(w)));
    }
}

/// Register the four baselines plus the [`SPEC_IDS`] mega rows.
pub fn register_all(group: &mut Group<'_>, w: &Workload) {
    register_all_suffixed(group, "", w);
}

/// [`register_all`] with a `/variant` suffix on every row.
pub fn register_all_suffixed(group: &mut Group<'_>, variant: &str, w: &Workload) {
    let mut subjects = baseline_subjects();
    subjects.extend(mega_subjects(SPEC_IDS));
    run_subjects(group, variant, w, &subjects);
}

/// Register only the [`SPEC_IDS`] mega rows (no vanilla baselines).
pub fn register_mega(group: &mut Group<'_>, w: &Workload) {
    register_mega_suffixed(group, "", w);
}

/// [`register_mega`] with a `/variant` suffix on every row.
pub fn register_mega_suffixed(group: &mut Group<'_>, variant: &str, w: &Workload) {
    run_subjects(group, variant, w, &mega_subjects(SPEC_IDS));
}

/// Register mega rows for a caller-supplied spec list (e.g. a single spec, or
/// the SELFDESTRUCT-relevant specs).
pub fn register_mega_specs(
    group: &mut Group<'_>,
    specs: &[(&'static str, MegaSpecId)],
    w: &Workload,
) {
    register_mega_specs_suffixed(group, specs, "", w);
}

/// [`register_mega_specs`] with a `/variant` suffix on every row.
pub fn register_mega_specs_suffixed(
    group: &mut Group<'_>,
    specs: &[(&'static str, MegaSpecId)],
    variant: &str,
    w: &Workload,
) {
    run_subjects(group, variant, w, &mega_subjects(specs));
}

/// Register three isolation rows for one spec + workload under a `/variant`
/// suffix, in order:
///
/// 1. `revm_pinned` — the env-agnostic absolute floor (no mega overhead).
/// 2. `baseline_name` — `Mega` over `EmptyExternalEnv`, where the external-env path (SALT / oracle)
///    short-circuits to constants (no hash, no map lookup).
/// 3. `active_name` — `MegaWithEnv` over the supplied `env`, exercising the real external-env path
///    (e.g. crowded SALT buckets or oracle storage).
///
/// The `active_name` − `baseline_name` gap isolates the external-env path cost
/// while holding spec and workload fixed. Shared by the SALT and oracle benches.
pub fn register_env_isolation(
    group: &mut Group<'_>,
    spec: MegaSpecId,
    baseline_name: &'static str,
    active_name: &'static str,
    env: TestExternalEnvs<Infallible>,
    variant: &str,
    w: &Workload,
) {
    run_subjects(group, variant, w, &[Box::new(RevmPinned)]);
    run_subjects(group, variant, w, &[Box::new(Mega { name: baseline_name, spec })]);
    run_subjects(group, variant, w, &[Box::new(MegaWithEnv { name: active_name, spec, env })]);
}

/// Like [`register_env_isolation`] but omits the `revm_pinned` floor row.
///
/// Use this to register more than one spec's env-isolation rows in the SAME
/// group: call [`register_env_isolation`] for the first spec (registering the
/// shared `revm_pinned` floor plus that spec's baseline + active env rows),
/// then call this floor-less variant for each additional spec. The
/// `revm_pinned` row must appear only once per group — registering it twice
/// panics with a duplicate benchmark-id error.
pub fn register_env_isolation_mega_only(
    group: &mut Group<'_>,
    spec: MegaSpecId,
    baseline_name: &'static str,
    active_name: &'static str,
    env: TestExternalEnvs<Infallible>,
    variant: &str,
    w: &Workload,
) {
    run_subjects(group, variant, w, &[Box::new(Mega { name: baseline_name, spec })]);
    run_subjects(group, variant, w, &[Box::new(MegaWithEnv { name: active_name, spec, env })]);
}
