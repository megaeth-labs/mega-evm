//! Input limits refuse whole fixtures rather than reporting a partial corpus or state as valid.

use std::fs::File;

use state_test::{
    blockchain,
    input::{find_json_files, MAX_BLOCKCHAIN_FIXTURE_BYTES, MAX_FIXTURE_BYTES, MAX_FIXTURE_DEPTH},
    runner, Fork, Mode,
};

#[test]
fn test_both_runners_refuse_oversized_files_before_parsing() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("oversized.json");
    File::create(&path).unwrap().set_len(MAX_FIXTURE_BYTES + 1).unwrap();

    let failure = runner::run_file(
        &path,
        runner::Config {
            mode: Mode::Equivalence,
            fork: Fork::Osaka,
            threads: 1,
            json_outcome: false,
            trace: false,
            digest: None,
            witness: None,
            limits: None,
            deviations: &[],
        },
    )
    .unwrap_err();
    assert_eq!(failure.kind, runner::FailureKind::Fixture);
    assert!(failure.detail.contains("exceeds 67108864 bytes"), "{failure:?}");
    File::create(&path).unwrap().set_len(MAX_BLOCKCHAIN_FIXTURE_BYTES + 1).unwrap();
    let failure = blockchain::run_file(
        &path,
        blockchain::Config { threads: 1, json_outcome: false, witness: None, deviations: &[] },
    )
    .unwrap_err();
    assert_eq!(failure.kind, blockchain::FailureKind::Fixture);
    assert!(failure.detail.contains("exceeds 268435456 bytes"), "{failure:?}");
}

#[test]
fn test_discovery_refuses_excessive_depth_even_without_json_below_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("visible.json"), "{}").unwrap();
    let mut nested = root.path().to_path_buf();
    for _ in 0..MAX_FIXTURE_DEPTH {
        nested.push("d");
    }
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(find_json_files(root.path()).unwrap().len(), 1);
    std::fs::create_dir(nested.join("one-too-deep")).unwrap();
    let error = find_json_files(root.path()).unwrap_err();
    assert!(error.to_string().contains("depth exceeds 64"), "{error}");
}

/// A compact but over-wide state stays below the byte limit; cardinality must reject it.
fn oversized_state() -> String {
    let mut state = String::from("{");
    for index in 0..=state_test::input::MAX_STATE_ACCOUNTS {
        if index != 0 {
            state.push(',');
        }
        state.push_str(&format!(
            "\"0x{index:040x}\":{{\"balance\":\"0x0\",\"code\":\"0x\",\"nonce\":\"0x0\",\"storage\":{{}}}}"
        ));
    }
    state.push('}');
    assert!(state.len() < MAX_FIXTURE_BYTES as usize);
    state
}

#[test]
fn test_state_runner_rejects_wide_prestate_before_materializing_or_hashing_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("wide.json");
    let fixture = format!(
        r#"{{"wide":{{
            "env":{{"currentCoinbase":"0x0000000000000000000000000000000000000000",
                "currentGasLimit":"0x1000000","currentNumber":"0x1","currentTimestamp":"0x1",
                "currentBaseFee":"0x7","currentRandom":"0x0000000000000000000000000000000000000000000000000000000000000000",
                "currentExcessBlobGas":"0x0"}},
            "pre":{},
            "transaction":{{"data":["0x"],"gasLimit":["0x5208"],"gasPrice":"0xa",
                "nonce":"0x0","secretKey":"0x45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8",
                "to":"0x0000000000000000000000000000000000000001","value":["0x0"]}},
            "post":{{"Osaka":[{{"indexes":{{"data":0,"gas":0,"value":0}},
                "hash":"0x0000000000000000000000000000000000000000000000000000000000000000",
                "logs":"0x0000000000000000000000000000000000000000000000000000000000000000"}}]}}
        }}}}"#,
        oversized_state()
    );
    std::fs::write(&path, fixture).unwrap();
    let results = runner::run_file(
        &path,
        runner::Config {
            mode: Mode::Equivalence,
            fork: Fork::Osaka,
            threads: 1,
            json_outcome: false,
            trace: false,
            digest: None,
            witness: None,
            limits: None,
            deviations: &[],
        },
    )
    .unwrap();
    let [result] = &results[..] else { panic!("expected one result") };
    let runner::Outcome::Failed(failure) = &result.outcome else {
        panic!("oversized pre-state was accepted: {:?}", result.outcome)
    };
    assert_eq!(failure.kind, runner::FailureKind::Fixture);
    assert!(failure.detail.contains("pre-state: state account count exceeds"), "{failure:?}");
}

#[test]
fn test_blockchain_runner_rejects_wide_pre_and_post_states_before_hashing() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("wide.json");
    let state = oversized_state();
    for (pre, post, label) in [(&*state, "{}", "pre-state"), ("{}", &*state, "post-state")] {
        let fixture = format!(
            r#"{{"wide":{{"network":"Osaka","genesisRLP":"0x","blocks":[],
                "pre":{pre},"postState":{post},
                "lastblockhash":"0x0000000000000000000000000000000000000000000000000000000000000000",
                "config":{{"chainid":"0x1","blobSchedule":{{"Osaka":{{"baseFeeUpdateFraction":"0x1"}}}}}}
            }}}}"#
        );
        std::fs::write(&path, fixture).unwrap();
        let results = blockchain::run_file(
            &path,
            blockchain::Config { threads: 1, json_outcome: false, witness: None, deviations: &[] },
        )
        .unwrap();
        let [result] = &results[..] else { panic!("expected one result") };
        let blockchain::Outcome::Failed(failure) = &result.outcome else {
            panic!("oversized {label} was accepted: {:?}", result.outcome)
        };
        assert_eq!(failure.kind, blockchain::FailureKind::Fixture);
        assert!(
            failure.detail.contains(&format!("{label}: state account count exceeds")),
            "{failure:?}"
        );
    }
}
