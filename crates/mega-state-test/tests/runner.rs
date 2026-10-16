//! The runner on fixtures written here, whose expectations come from revm's own mainnet EVM on the
//! fixture's fork — Ethereum, and not `MegaEvm` — so a fixture that passes passes because Satin's
//! machinery agrees with Ethereum on it, and one that fails is judged the way the gate judges it.

use std::path::{Path, PathBuf};

use mega_evm::revm::{
    context::{result::ExecutionResult, CfgEnv, Context},
    database::State,
    handler::{MainBuilder, MainContext},
    primitives::B256,
    ExecuteCommitEvm,
};
use serde_json::{json, Value};
use state_test::{
    deviations::{Deviation, Entry, DEVIATIONS},
    roots::{logs_hash, state_root},
    runner::{run, Config, FailureKind, Outcome, Report},
    skips::SkipReason,
    types::TestSuite,
    Fork, Mode,
};

const SENDER: &str = "0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b";
const SENDER_KEY: &str = "0x45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8";
const CONTRACT: &str = "0x00000000000000000000000000000000000c0de0";
const COINBASE: &str = "0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba";
const BASE_FEE_VAULT: &str = "0x4200000000000000000000000000000000000019";

/// `SSTORE(0, 42)`, then `LOG0` of the word 7.
const STORE_AND_LOG: &str = "0x602a5f5560075f5260205fa000";

fn config(mode: Mode, fork: Fork) -> Config {
    Config { mode, fork, threads: 2, json_outcome: false, trace: false, deviations: DEVIATIONS }
}

/// A unit calling `CONTRACT` with `transaction` fields over the defaults, and one entry per fork
/// in `forks` with placeholder roots.
fn unit(forks: &[Fork], transaction: Value) -> Value {
    let mut tx = json!({
        "data": ["0x"],
        "gasLimit": ["0x186a0"],
        "gasPrice": "0x0a",
        "nonce": "0x00",
        "secretKey": SENDER_KEY,
        "sender": SENDER,
        "to": CONTRACT,
        "value": ["0x01"],
    });
    for (key, value) in transaction.as_object().expect("an object") {
        tx[key] = value.clone();
    }
    let entry = json!([{
        "indexes": { "data": 0, "gas": 0, "value": 0 },
        "hash": B256::ZERO,
        "logs": B256::ZERO,
    }]);
    let post: serde_json::Map<_, _> =
        forks.iter().map(|fork| (fork.name().to_string(), entry.clone())).collect();
    json!({
        "env": {
            "currentCoinbase": COINBASE,
            "currentGasLimit": "0x1000000",
            "currentNumber": "0x01",
            "currentTimestamp": "0x03e8",
            "currentBaseFee": "0x07",
            "currentRandom": "0x0000000000000000000000000000000000000000000000000000000000020000",
            "currentExcessBlobGas": "0x00",
        },
        "pre": {
            SENDER: { "balance": "0x3635c9adc5dea00000", "code": "0x", "nonce": "0x00", "storage": {} },
            CONTRACT: { "balance": "0x00", "code": STORE_AND_LOG, "nonce": "0x01", "storage": {} },
        },
        "transaction": tx,
        "post": post,
    })
}

/// Fills every entry of `unit` with what revm's mainnet EVM on the entry's fork produces: the
/// post-state root and the logs hash, which for a rejected transaction are the pre-state's and
/// the empty list's.
fn fill_from_ethereum(mut unit: Value) -> Value {
    let parsed: TestSuite = serde_json::from_value(json!({ "t": unit.clone() })).expect("a unit");
    let parsed = &parsed.0["t"];
    for (spec, tests) in &parsed.post {
        let fork = Fork::ALL.into_iter().find(|fork| fork.is(spec)).expect("a runner fork");
        for (index, test) in tests.iter().enumerate() {
            let mut cfg = CfgEnv::new();
            cfg.set_spec_and_mainnet_gas_params(fork.spec_id());
            cfg.set_max_blobs_per_tx(6);
            let block = parsed.block_env(&mut cfg);
            let mut state =
                State::builder().with_cached_prestate(parsed.state()).with_bundle_update().build();
            let result = match test.tx_env(parsed) {
                Ok(tx) => Context::mainnet()
                    .with_block(block)
                    .with_cfg(cfg)
                    .with_db(&mut state)
                    .build_mainnet()
                    .transact_commit(tx)
                    .ok(),
                Err(_) => None,
            };
            let logs = result.as_ref().map(ExecutionResult::logs).unwrap_or_default();
            let entry = &mut unit["post"][fork.name()][index];
            entry["hash"] = json!(state_root(state.cache.trie_account()));
            entry["logs"] = json!(logs_hash(logs));
        }
    }
    unit
}

/// Writes `units` as one fixture file at `relative` under `dir`.
fn write(dir: &Path, relative: &str, units: Value) -> PathBuf {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
    std::fs::write(&path, serde_json::to_string_pretty(&units).expect("json")).expect("a file");
    path
}

fn outcomes(report: &Report) -> Vec<&Outcome> {
    report.results.iter().map(|result| &result.outcome).collect()
}

fn failure_kind(outcome: &Outcome) -> Option<FailureKind> {
    match outcome {
        Outcome::Failed(failure) => Some(failure.kind),
        _ => None,
    }
}

/// A priced call that writes a slot and logs, filled from Ethereum, passes on Satin's machinery
/// in equivalence mode under both forks: the gas, the refund to the sender, the fee to the
/// beneficiary, the base fee Ethereum burns and Satin routes to its vault, the slot and the log.
#[test]
fn test_a_fixture_ethereum_filled_passes_in_equivalence_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "a.json",
        json!({ "call": fill_from_ethereum(unit(&Fork::ALL, json!({})))}),
    );
    for fork in Fork::ALL {
        let report = run(std::slice::from_ref(&path), config(Mode::Equivalence, fork));
        assert_eq!(outcomes(&report), [&Outcome::Passed], "{fork}");
        let summary = report.summary();
        assert_eq!((summary.defined, summary.executed, summary.passed), (1, 1, 1));
        assert!(report.gate(Some(1), Some(0), false).is_empty());
    }
}

/// The same call fails in Satin mode: Satin's own configuration charges history gas and prices
/// state its own way, so a sender with gas enough for both pays more than Ethereum's post-state
/// says; and it emits the EIP-7708 transfer log of the call's value, which Osaka's logs do not
/// carry, so a call that moves value fails on its logs before its state root is compared.
#[test]
fn test_the_same_fixture_fails_in_satin_mode() {
    let dir = tempfile::tempdir().unwrap();
    for (value, kind) in
        [("0x00", FailureKind::StateRootMismatch), ("0x01", FailureKind::LogsMismatch)]
    {
        let unit = unit(&[Fork::Osaka], json!({ "gasLimit": ["0x0f4240"], "value": [value] }));
        let path = write(dir.path(), "a.json", json!({ "call": fill_from_ethereum(unit) }));
        let report = run(std::slice::from_ref(&path), config(Mode::Equivalence, Fork::Osaka));
        assert_eq!(outcomes(&report), [&Outcome::Passed], "value {value}");

        let report = run(&[path], config(Mode::Satin, Fork::Osaka));
        assert_eq!(failure_kind(outcomes(&report)[0]), Some(kind), "value {value}");
        let summary = report.summary();
        assert_eq!(summary.unattributed, 1, "Satin mode attributes nothing");
        assert!(summary.deviated.is_empty());
    }
}

/// A post-state root or a logs hash that is not what executes is a failure no deviation explains.
#[test]
fn test_wrong_roots_fail() {
    let dir = tempfile::tempdir().unwrap();
    let filled = fill_from_ethereum(unit(&[Fork::Osaka], json!({})));
    let mut wrong_root = filled.clone();
    wrong_root["post"]["Osaka"][0]["hash"] = json!(B256::repeat_byte(1));
    let mut wrong_logs = filled;
    wrong_logs["post"]["Osaka"][0]["logs"] = json!(B256::repeat_byte(1));
    let path = write(dir.path(), "a.json", json!({ "a_root": wrong_root, "b_logs": wrong_logs }));

    let report = run(&[path], config(Mode::Equivalence, Fork::Osaka));
    let kinds: Vec<_> = outcomes(&report).into_iter().map(failure_kind).collect();
    assert_eq!(kinds, [Some(FailureKind::StateRootMismatch), Some(FailureKind::LogsMismatch)]);
    assert_eq!(report.summary().unattributed, 2);
    assert_eq!(report.gate(None, None, false), ["2 failed tests no deviation explains"]);
}

/// An expected exception must be the one raised, and must leave the pre-state; an unexpected one,
/// or a missing one, fails.
#[test]
fn test_exceptions_are_matched_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let too_little_gas = json!({ "gasLimit": ["0x5208"], "data": ["0x00"] });
    let named = |exception: &str| {
        let mut unit = fill_from_ethereum(unit(&[Fork::Osaka], too_little_gas.clone()));
        unit["post"]["Osaka"][0]["expectException"] = json!(exception);
        unit
    };
    let mut missing = fill_from_ethereum(unit(&[Fork::Osaka], json!({})));
    missing["post"]["Osaka"][0]["expectException"] =
        json!("TransactionException.INTRINSIC_GAS_TOO_LOW");
    let unexpected = fill_from_ethereum(unit(&[Fork::Osaka], too_little_gas.clone()));
    let path = write(
        dir.path(),
        "a.json",
        json!({
            "a_named": named("TransactionException.INTRINSIC_GAS_TOO_LOW"),
            "b_alternatives": named("TransactionException.NONCE_IS_MAX|TransactionException.INTRINSIC_GAS_TOO_LOW"),
            "c_wrong": named("TransactionException.NONCE_MISMATCH_TOO_HIGH"),
            "d_missing": missing,
            "e_unexpected": unexpected,
        }),
    );
    let report = run(&[path], config(Mode::Equivalence, Fork::Osaka));
    let kinds: Vec<_> = outcomes(&report).into_iter().map(failure_kind).collect();
    assert_eq!(
        kinds,
        [
            None,
            None,
            Some(FailureKind::WrongException),
            Some(FailureKind::MissingException),
            Some(FailureKind::UnexpectedException),
        ]
    );
}

/// An expected output must be produced; a call that returns nothing does not satisfy one.
#[test]
fn test_an_expected_output_must_be_produced() {
    let dir = tempfile::tempdir().unwrap();
    let mut unit = fill_from_ethereum(unit(&[Fork::Osaka], json!({})));
    unit["out"] = json!("0x01");
    let path = write(dir.path(), "a.json", json!({ "call": unit }));
    let report = run(&[path], config(Mode::Equivalence, Fork::Osaka));
    assert_eq!(failure_kind(outcomes(&report)[0]), Some(FailureKind::OutputMismatch));
}

/// Only the run's fork's entries count; every entry of a file on the skip list is skipped and
/// counted as defined; a transaction that cannot be built is skipped when the fixture expects it
/// invalid for the reason it cannot be, a wrong exception when the fixture names another reason,
/// and a fixture failure when it expects none.
#[test]
fn test_what_is_counted_and_what_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let both = fill_from_ethereum(unit(&Fork::ALL, json!({})));
    let collision =
        write(dir.path(), "paris/eip7610_create_collision/c.json", json!({ "c": &both }));
    let mut unbuildable = unit(&[Fork::Osaka], json!({}));
    unbuildable["transaction"].as_object_mut().unwrap().remove("secretKey");
    let mut expected_invalid = unbuildable.clone();
    expected_invalid["post"]["Osaka"][0]["expectException"] =
        json!("TransactionException.INVALID_SIGNATURE_VRS");
    let mut other_reason = unbuildable.clone();
    other_reason["post"]["Osaka"][0]["expectException"] =
        json!("TransactionException.INTRINSIC_GAS_TOO_LOW");
    let others = write(
        dir.path(),
        "o.json",
        json!({
            "a_both": both,
            "b_expected_invalid": expected_invalid,
            "c_unbuildable": unbuildable,
            "d_other_reason": other_reason,
        }),
    );

    let report = run(&[collision, others], config(Mode::Equivalence, Fork::Osaka));
    let outcomes = outcomes(&report);
    assert_eq!(outcomes.len(), 5, "one entry per unit for Osaka, none for Amsterdam");
    assert_eq!(outcomes[0], &Outcome::Passed);
    assert_eq!(
        outcomes[1],
        &Outcome::Skipped { reason: SkipReason::UnbuildableInvalidTransaction }
    );
    assert_eq!(failure_kind(outcomes[2]), Some(FailureKind::Fixture));
    assert_eq!(failure_kind(outcomes[3]), Some(FailureKind::WrongException));
    assert_eq!(outcomes[4], &Outcome::Skipped { reason: SkipReason::CreateCollisionWithStorage });
    let summary = report.summary();
    assert_eq!((summary.defined, summary.executed, summary.skipped_total()), (5, 3, 2));
    assert_eq!(report.gate(Some(3), Some(2), false), ["2 failed tests no deviation explains"]);
    assert_eq!(
        report.gate(Some(4), Some(1), false)[1..],
        ["3 tests executed, 4 pinned".to_string(), "2 tests skipped, 1 pinned".to_string()]
    );
}

/// A test name that appears twice in a file fails the file, rather than the second test silently
/// replacing the first.
#[test]
fn test_a_duplicate_test_name_fails_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let unit = fill_from_ethereum(unit(&[Fork::Osaka], json!({})));
    let path = dir.path().join("twice.json");
    std::fs::write(&path, format!("{{\"t\": {unit}, \"t\": {unit}}}")).unwrap();
    let report = run(&[path], config(Mode::Equivalence, Fork::Osaka));
    assert!(report.results.is_empty());
    assert_eq!(report.file_failures.len(), 1);
    assert!(
        report.file_failures[0].1.detail.contains("appears twice"),
        "{:?}",
        report.file_failures
    );
}

/// A file that cannot be parsed is a failure of the run, not a set of skipped tests.
#[test]
fn test_an_unreadable_file_fails_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "broken.json", json!({ "t": { "env": {} } }));
    let report = run(&[path], config(Mode::Equivalence, Fork::Osaka));
    assert!(report.results.is_empty());
    assert_eq!(report.file_failures.len(), 1);
    let summary = report.summary();
    assert_eq!((summary.file_failures, summary.defined), (1, 0));
    assert_eq!(report.gate(None, None, false), ["1 fixture files could not be read"]);
}

/// The base-fee vault Satin credits is taken back only when the fee routing made it: a vault the
/// pre-state already holds stays, and the fee it received is a post-state Ethereum does not have.
#[test]
fn test_the_fee_vault_is_taken_back_only_when_the_routing_made_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut unit = unit(&[Fork::Osaka], json!({}));
    unit["pre"][BASE_FEE_VAULT] =
        json!({ "balance": "0x01", "code": "0x", "nonce": "0x00", "storage": {} });
    let path = write(dir.path(), "a.json", json!({ "call": fill_from_ethereum(unit) }));
    let report = run(&[path], config(Mode::Equivalence, Fork::Osaka));
    assert_eq!(failure_kind(outcomes(&report)[0]), Some(FailureKind::StateRootMismatch));
}

/// A chain id that does not fit a `u64`, and a fixture value the transaction builder cannot take,
/// fail the test they belong to and nothing else.
#[test]
fn test_bad_fixture_values_fail_their_own_test() {
    let dir = tempfile::tempdir().unwrap();
    let good = fill_from_ethereum(unit(&[Fork::Osaka], json!({})));
    let mut chain = good.clone();
    chain["env"]["currentChainID"] = json!("0x010000000000000000");
    let fee = unit(
        &[Fork::Osaka],
        // One more than `u128::MAX`, which the fixture types' transaction builder refuses with a
        // panic rather than an error.
        json!({ "maxFeePerGas": "0x0a", "maxPriorityFeePerGas": "0x0100000000000000000000000000000000" }),
    );
    let path =
        write(dir.path(), "a.json", json!({ "a_chain": chain, "b_fee": fee, "c_good": good }));
    let report = run(&[path], config(Mode::Equivalence, Fork::Osaka));
    let kinds: Vec<_> = outcomes(&report).into_iter().map(failure_kind).collect();
    assert_eq!(kinds, [Some(FailureKind::Fixture), Some(FailureKind::Panic), None]);
}

/// A registry of one Osaka deviation listing every failure of `report`, in the file at `path`
/// under the run's directory, with the hashes it produced.
fn registry_of(report: &Report, path: &'static str) -> &'static [Deviation] {
    let entries: Vec<_> = report
        .results
        .iter()
        .filter_map(|result| match &result.outcome {
            Outcome::Failed(failure) => Some(Entry {
                path,
                name: result.id.name.clone().leak(),
                indexes: result.id.indexes,
                produced: failure.produced.expect("a hash mismatch"),
            }),
            _ => None,
        })
        .collect();
    Box::leak(Box::new([Deviation {
        id: "listed",
        rule: "the rule",
        reason: "the reason",
        fork: Fork::Osaka,
        entries: entries.leak(),
    }]))
}

/// A deviation explains exactly the entries it lists, with the hashes they produce, and the gate
/// holds every one of them to its listing. Each edit below fails the gate on its own: a listed
/// failure made to pass, an unlisted entry of the same file broken — the two together keep the
/// count the deviation explains, which is all a count could see — and a listed entry that fails
/// with other hashes, the state root of a logs mismatch included.
#[test]
fn test_a_deviation_holds_the_entries_it_lists() {
    let dir = tempfile::tempdir().unwrap();
    let filled = fill_from_ethereum(unit(&[Fork::Osaka], json!({})));
    let root = filled["post"]["Osaka"][0]["hash"].clone();
    let mut wrong_root = filled.clone();
    wrong_root["post"]["Osaka"][0]["hash"] = json!(B256::repeat_byte(1));
    let mut wrong_logs = filled.clone();
    wrong_logs["post"]["Osaka"][0]["logs"] = json!(B256::repeat_byte(1));
    let units = json!({ "a_root": wrong_root, "b_logs": wrong_logs, "c_passes": filled });

    const PATH: &str = "cancun/deviated.json";
    let path = write(dir.path(), PATH, units.clone());
    let equivalence = config(Mode::Equivalence, Fork::Osaka);
    let registry = registry_of(&run(std::slice::from_ref(&path), equivalence), PATH);
    assert_eq!(registry[0].entries.len(), 2);
    let run_with = |units: &Value| {
        write(dir.path(), PATH, units.clone());
        run(std::slice::from_ref(&path), Config { deviations: registry, ..equivalence })
    };
    let gate = |units: &Value| run_with(units).gate(None, None, true);

    let report = run_with(&units);
    assert_eq!(report.gate(None, None, true), Vec::<String>::new());
    let summary = report.summary();
    assert_eq!(summary.deviated.get("listed"), Some(&2));
    assert_eq!((summary.unattributed, summary.unreproduced.len()), (0, 0));

    let unreproduced =
        "deviation listed: 1 of the 2 entries it lists on Osaka did not fail as listed";
    let explains_one = "deviation listed explains 1 failed tests on Osaka, 2 listed";
    let unattributed = "1 failed tests no deviation explains";

    // The listed state-root failure made to pass.
    let mut passes = units.clone();
    passes["a_root"]["post"]["Osaka"][0]["hash"] = root;
    assert_eq!(gate(&passes), [unreproduced, explains_one]);
    let report = run_with(&passes);
    let seen: Vec<_> = report.unreproduced().collect();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].entry.name, "a_root");
    assert!(matches!(seen[0].seen.as_slice(), [result] if result.outcome == Outcome::Passed));

    // The passing entry of the same file broken.
    let mut broken = units.clone();
    broken["c_passes"]["post"]["Osaka"][0]["hash"] = json!(B256::repeat_byte(2));
    assert_eq!(gate(&broken), [unattributed]);

    // Both at once: two failures in the file, as the deviation lists, and the gate still fails.
    let mut both = passes.clone();
    both["c_passes"] = broken["c_passes"].clone();
    let report = run_with(&both);
    assert_eq!(report.summary().failed_total(), 2);
    assert_eq!(report.gate(None, None, true), [unattributed, unreproduced, explains_one]);

    // The listed entries failing with other hashes: a sender that starts with one more wei moves
    // the post-state root, and leaves the logs as they were.
    for name in ["a_root", "b_logs"] {
        let mut other = units.clone();
        other[name]["pre"][SENDER]["balance"] = json!("0x3635c9adc5dea00001");
        let report = run_with(&other);
        let failure = report
            .results
            .iter()
            .find_map(|result| match &result.outcome {
                Outcome::Failed(failure) if result.id.name == name => Some(failure.clone()),
                _ => None,
            })
            .expect("the entry fails");
        assert_eq!(
            failure.kind,
            registry[0].entries.iter().find(|e| e.name == name).unwrap().produced.kind()
        );
        assert_eq!(failure.deviation, None, "{name}");
        assert_eq!(
            report.gate(None, None, true),
            [unattributed, unreproduced, explains_one],
            "{name}"
        );
    }

    // The same tests in another file are not the listed entries.
    let elsewhere = write(dir.path(), "cancun/elsewhere.json", units);
    let report = run(&[elsewhere], Config { deviations: registry, ..equivalence });
    assert_eq!(report.summary().unattributed, 2);
    assert_eq!(report.summary().unreproduced.get("listed"), Some(&2));

    // Satin mode attributes nothing, and has nothing to reproduce.
    let report = run(&[path], Config { deviations: registry, ..config(Mode::Satin, Fork::Osaka) });
    let summary = report.summary();
    assert!(summary.deviated.is_empty() && summary.unreproduced.is_empty());
}

/// A failure of an entry the registry lists, with hashes the registry does not list, is
/// unattributed: the registry explains its own failures, not the place they are in.
#[test]
fn test_the_registry_does_not_absorb_another_failure_of_its_entries() {
    let dir = tempfile::tempdir().unwrap();
    let listed = &DEVIATIONS
        .iter()
        .find(|deviation| deviation.fork == Fork::Osaka)
        .expect("an Osaka deviation")
        .entries[0];
    let mut wrong = fill_from_ethereum(unit(&[Fork::Osaka], json!({})));
    wrong["post"]["Osaka"][0]["hash"] = json!(B256::repeat_byte(1));
    let path = write(dir.path(), listed.path, json!({ listed.name: wrong }));
    let report = run(&[path], config(Mode::Equivalence, Fork::Osaka));
    assert!(matches!(
        &report.results[0].outcome,
        Outcome::Failed(failure) if failure.kind == listed.produced.kind() && failure.deviation.is_none()
    ));
    assert_eq!(report.summary().unattributed, 1);
}
