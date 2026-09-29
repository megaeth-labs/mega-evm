//! The runner every property runs on: a seeded, bounded proptest run whose failure names the seed
//! that reproduces it and prints the minimal case it shrank to.
//!
//! Two modes, chosen by the environment:
//!
//! - **bounded** (the default): a fixed seed and a small number of cases per property, so the run
//!   is deterministic and finishes in CI in about two minutes for the whole target, in debug and in
//!   release;
//! - **long** (`MEGA_FUZZ_LONG=1`): many more cases and a seed drawn from the clock unless
//!   `MEGA_FUZZ_SEED` names one; the seed is printed at the start of every property so a failure of
//!   a long run is reproducible.
//!
//! `MEGA_FUZZ_CASES` overrides the case count in either mode; `MEGA_FUZZ_SEED` the seed. A failure
//! panics with the property's name, the reason, the seed and case count that reproduce the run and
//! the minimal failing case, as `Debug` renders it.

use std::{fmt::Debug, time::SystemTime};

use proptest::{
    strategy::Strategy,
    test_runner::{Config, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner},
};

/// Names the number of cases each property runs.
pub(crate) const CASES_ENV_VAR: &str = "MEGA_FUZZ_CASES";
/// Names the seed the cases are drawn from.
pub(crate) const SEED_ENV_VAR: &str = "MEGA_FUZZ_SEED";
/// Set to anything but `0`, `false` or empty, switches the long mode on.
pub(crate) const LONG_ENV_VAR: &str = "MEGA_FUZZ_LONG";

/// How many times the long mode multiplies a property's bounded case count.
const LONG_MULTIPLIER: u32 = 50;

/// The seed of a bounded run, when the environment names none.
const DEFAULT_SEED: u64 = 0;

/// The stack of the thread a property runs on.
const STACK_SIZE: usize = 256 << 20;

/// How a property runs: its case count, its seed and which mode chose them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FuzzConfig {
    /// Cases to run.
    pub(crate) cases: u32,
    /// The RNG seed.
    pub(crate) seed: u64,
    /// Whether the long mode is on.
    pub(crate) long: bool,
}

impl FuzzConfig {
    /// Reads the mode from the environment; `bounded_cases` is the property's case count in the
    /// bounded mode.
    pub(crate) fn from_env(bounded_cases: u32) -> Self {
        let long = std::env::var(LONG_ENV_VAR)
            .map(|v| !matches!(v.trim(), "" | "0" | "false"))
            .unwrap_or(false);
        let cases = match std::env::var(CASES_ENV_VAR) {
            Ok(v) => {
                v.trim().parse().unwrap_or_else(|_| panic!("{CASES_ENV_VAR}={v:?} is not a number"))
            }
            Err(_) if long => bounded_cases.saturating_mul(LONG_MULTIPLIER),
            Err(_) => bounded_cases,
        };
        let seed = match std::env::var(SEED_ENV_VAR) {
            Ok(v) => {
                v.trim().parse().unwrap_or_else(|_| panic!("{SEED_ENV_VAR}={v:?} is not a number"))
            }
            Err(_) if long => SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(DEFAULT_SEED),
            Err(_) => DEFAULT_SEED,
        };
        Self { cases, seed, long }
    }

    /// The RNG the cases are drawn from: `ChaCha` seeded from the seed, so the same seed draws the
    /// same cases on every platform.
    fn rng(&self) -> TestRng {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&self.seed.to_le_bytes());
        TestRng::from_seed(RngAlgorithm::ChaCha, &bytes)
    }

    fn proptest_config(&self) -> Config {
        Config {
            cases: self.cases,
            // Failures are reproduced from the printed seed, not from a regressions file.
            failure_persistence: None,
            source_file: None,
            // Shrinking re-executes the property on every candidate; cap the time so a failing
            // run in CI still reports a case, minimal or not, well within the job's timeout.
            max_shrink_time: 120_000,
            max_shrink_iters: 4_096,
            ..Config::default()
        }
    }
}

/// Runs `property` on cases drawn from the strategy `strategy` builds, in the mode the environment
/// chooses, and panics with a reproducible report on the first failure. The strategy is built on
/// the property's own thread, because a boxed strategy cannot be sent to one.
///
/// `bounded_cases` is the property's case count in the bounded mode: enough to hit every arm of
/// the generators, few enough for the whole target to finish in about two minutes.
pub(crate) fn check<S>(
    name: &str,
    bounded_cases: u32,
    strategy: impl FnOnce() -> S + Send,
    property: impl Fn(&S::Value) -> Result<(), TestCaseError> + Send,
) where
    S: Strategy,
    S::Value: Debug + Send,
{
    let config = FuzzConfig::from_env(bounded_cases);
    if config.long {
        eprintln!(
            "{name}: long mode, {} cases; reproduce with {SEED_ENV_VAR}={} {CASES_ENV_VAR}={}",
            config.cases, config.seed, config.cases
        );
    }
    // A case can nest creations and calls deep, and a minimal case is rendered whole: run on a
    // thread with a stack the harness's default does not give.
    let outcome = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name(format!("fuzz-{name}"))
            .stack_size(STACK_SIZE)
            .spawn_scoped(scope, move || {
                let strategy = strategy();
                let mut runner = TestRunner::new_with_rng(config.proptest_config(), config.rng());
                runner.run(&strategy, |case| property(&case))
            })
            .expect("the property's thread starts")
            .join()
            .unwrap_or_else(|_| panic!("property `{name}` panicked outside a case"))
    });
    match outcome {
        Ok(()) => {}
        Err(TestError::Fail(reason, minimal)) => panic!(
            "property `{name}` failed: {reason}\n\
             reproduce with {SEED_ENV_VAR}={} {CASES_ENV_VAR}={}\n\
             minimal failing case:\n{minimal:#?}",
            config.seed, config.cases
        ),
        Err(TestError::Abort(reason)) => panic!(
            "property `{name}` aborted: {reason}\n\
             reproduce with {SEED_ENV_VAR}={} {CASES_ENV_VAR}={}",
            config.seed, config.cases
        ),
    }
}

/// Fails the running case with `message`, as a property does when it finds a counterexample.
pub(crate) fn fail(message: impl Into<String>) -> TestCaseError {
    TestCaseError::fail(message.into())
}

/// Asserts `left == right` inside a property, failing the case with `what` and both sides.
macro_rules! prop_eq {
    ($left:expr, $right:expr, $($what:tt)+) => {{
        let (left, right) = (&$left, &$right);
        if left != right {
            return Err($crate::harness::fail(format!(
                "{}\n  left: {left:?}\n right: {right:?}",
                format_args!($($what)+)
            )));
        }
    }};
}

/// Asserts `cond` inside a property, failing the case with `what`.
macro_rules! prop_check {
    ($cond:expr, $($what:tt)+) => {{
        if !$cond {
            return Err($crate::harness::fail(format!($($what)+)));
        }
    }};
}

pub(crate) use prop_check;
pub(crate) use prop_eq;
