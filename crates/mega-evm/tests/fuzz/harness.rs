//! The runner every property runs on: a seeded, bounded proptest run whose failure names the seed
//! that reproduces it and prints the minimal case it shrank to.
//!
//! Two modes, chosen by the environment:
//!
//! - **bounded** (the default): a fixed seed and a small number of cases per property, so the run
//!   is deterministic and finishes in seconds for the whole target, in debug and in release;
//! - **long** (`MEGA_FUZZ_LONG=1`): many more cases and a seed drawn from the clock unless
//!   `MEGA_FUZZ_SEED` names one; the seed is printed at the start of every property so a failure of
//!   a long run is reproducible.
//!
//! `MEGA_FUZZ_CASES` replaces a property's case count in either mode: every property then runs
//! exactly that many cases, and the long mode's multiplier does not apply. `MEGA_FUZZ_SEED` names
//! the seed in either mode. Either variable set to the empty string counts as unset, so a caller
//! that always exports them, as a workflow input does, need not unset them. A failure panics with
//! the property's name, the reason, the seed and case count that reproduce the run and the minimal
//! failing case, as `Debug` renders it.

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
        let var = |name: &str| std::env::var(name).ok();
        Self::from_vars(
            bounded_cases,
            var(LONG_ENV_VAR).as_deref(),
            var(CASES_ENV_VAR).as_deref(),
            var(SEED_ENV_VAR).as_deref(),
            || {
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(DEFAULT_SEED)
            },
        )
    }

    /// The mode the three variables choose, each `None` when unset; `clock` draws the long mode's
    /// seed when none is named.
    ///
    /// - The long mode is on when its variable is anything but empty, `0` or `false`.
    /// - A case count, when named, is the count: it replaces the bounded count and the long mode's
    ///   multiple of it alike. Unnamed, the count is the bounded one, times [`LONG_MULTIPLIER`] in
    ///   the long mode.
    /// - A seed, when named, is the seed. Unnamed, the bounded mode's is [`DEFAULT_SEED`] and the
    ///   long mode's comes from the clock.
    /// - An empty value is an unnamed one.
    ///
    /// # Panics
    ///
    /// When a case count or a seed is named and is not a number.
    fn from_vars(
        bounded_cases: u32,
        long: Option<&str>,
        cases: Option<&str>,
        seed: Option<&str>,
        clock: impl FnOnce() -> u64,
    ) -> Self {
        fn named(value: Option<&str>) -> Option<&str> {
            value.map(str::trim).filter(|v| !v.is_empty())
        }
        let long = named(long).is_some_and(|v| !matches!(v, "0" | "false"));
        let cases = match named(cases) {
            Some(v) => {
                v.parse().unwrap_or_else(|_| panic!("{CASES_ENV_VAR}={v:?} is not a number"))
            }
            None if long => bounded_cases.saturating_mul(LONG_MULTIPLIER),
            None => bounded_cases,
        };
        let seed = match named(seed) {
            Some(v) => v.parse().unwrap_or_else(|_| panic!("{SEED_ENV_VAR}={v:?} is not a number")),
            None if long => clock(),
            None => DEFAULT_SEED,
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
/// the generators, few enough for the whole target to finish well within a minute in debug.
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

/// Prints what a property counted along its run, one line per class under the property's name,
/// in one write, so the tallies of properties running side by side do not interleave.
pub(crate) fn print_tally<K: std::fmt::Display>(
    name: &str,
    tally: impl IntoIterator<Item = (K, u32)>,
) {
    let mut out = format!("{name}:\n");
    for (class, n) in tally {
        out.push_str(&format!("{n:7} {class}\n"));
    }
    print!("{out}");
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

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDED: u32 = 256;
    const CLOCK: u64 = 77;

    fn config(long: Option<&str>, cases: Option<&str>, seed: Option<&str>) -> FuzzConfig {
        FuzzConfig::from_vars(BOUNDED, long, cases, seed, || CLOCK)
    }

    /// Every combination of the three variables unset, empty and set: the mode, the case count
    /// and the seed each chooses.
    #[test]
    fn test_the_mode_the_environment_chooses() {
        let unset_or_empty = [None, Some(""), Some("  ")];
        for long_off in [None, Some(""), Some("0"), Some("false")] {
            for cases in unset_or_empty {
                for seed in unset_or_empty {
                    assert_eq!(
                        config(long_off, cases, seed),
                        FuzzConfig { cases: BOUNDED, seed: DEFAULT_SEED, long: false },
                        "the bounded mode: {long_off:?} {cases:?} {seed:?}"
                    );
                }
            }
            assert_eq!(
                config(long_off, Some("5000"), Some("9")),
                FuzzConfig { cases: 5_000, seed: 9, long: false },
                "the bounded mode takes a case count and a seed"
            );
        }
        for long_on in [Some("1"), Some("true"), Some("yes")] {
            for cases in unset_or_empty {
                for seed in unset_or_empty {
                    assert_eq!(
                        config(long_on, cases, seed),
                        FuzzConfig { cases: BOUNDED * LONG_MULTIPLIER, seed: CLOCK, long: true },
                        "the long mode: {long_on:?} {cases:?} {seed:?}"
                    );
                }
                assert_eq!(
                    config(long_on, cases, Some("9")),
                    FuzzConfig { cases: BOUNDED * LONG_MULTIPLIER, seed: 9, long: true },
                    "a named seed replaces the clock's"
                );
            }
            assert_eq!(
                config(long_on, Some("20000"), None),
                FuzzConfig { cases: 20_000, seed: CLOCK, long: true },
                "a named case count is the count: the multiplier does not apply to it"
            );
            assert_eq!(
                config(long_on, Some(" 20000 "), Some(" 9 ")),
                FuzzConfig { cases: 20_000, seed: 9, long: true },
                "surrounding blanks are ignored"
            );
        }
    }

    #[test]
    #[should_panic(expected = "MEGA_FUZZ_CASES=\"many\" is not a number")]
    fn test_a_case_count_that_is_not_a_number_is_refused() {
        config(None, Some("many"), None);
    }

    #[test]
    #[should_panic(expected = "MEGA_FUZZ_SEED=\"lucky\" is not a number")]
    fn test_a_seed_that_is_not_a_number_is_refused() {
        config(Some("1"), None, Some("lucky"));
    }
}
