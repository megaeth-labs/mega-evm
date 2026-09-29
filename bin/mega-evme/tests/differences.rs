//! The expected differences between Satin and the legacy engine, pinned.
//!
//! Renders `tests/satin-differences.md` from what the two engines produce on the same inputs, and
//! compares it with the checked-in file, so a change that moves any number of either engine
//! shows up as a diff of that file. After an intended change, rewrite it with
//! `UPDATE_EVME_DIFFERENCES=1 cargo test --manifest-path bin/mega-evme/Cargo.toml --test
//! differences`.
//!
//! The inputs are the ones this repository can build without a chain: the `run` and `tx`
//! fixtures, each run on both engines (`test_<name>.json` and `test_satin_<name>.json`, whose
//! outputs `tests/integration.rs` holds the binary to), and the recorded mainnet blocks of
//! `tests/fixtures/blocks`, whose legacy replay matches the chain.
#![cfg(feature = "legacy")]

mod common;

use std::{collections::BTreeMap, fmt::Write as _, path::Path};

use common::blocks::{cache_copy_with_absent_factory, read_block, run_evme, BLOCKS};
use mega_evm::constants::{COST_PER_HISTORY_BYTE, COST_PER_STATE_BYTE};
use serde_json::Value;

const TABLE: &str = "tests/satin-differences.md";

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// `a / b` to two decimals, rounded down.
fn ratio(a: u64, b: u64) -> String {
    if b == 0 {
        return "-".to_string();
    }
    let hundredths = u128::from(a) * 100 / u128::from(b);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

fn signed(a: u64, b: u64) -> String {
    if a >= b {
        format!("+{}", a - b)
    } else {
        format!("-{}", b - a)
    }
}

fn u64_of(value: &Value) -> u64 {
    value.as_u64().unwrap_or_else(|| panic!("not a number: {value}"))
}

/// `success`, `revert` or `halt`, from a `run`/`tx` JSON summary.
fn summary_status(summary: &Value) -> &'static str {
    if summary["success"] == true {
        "success"
    } else if summary.get("halt_reason").is_some() {
        "halt"
    } else {
        "revert"
    }
}

/// The mechanisms a Satin row shows, from its numbers.
fn classes(satin: &Value, legacy_success: bool, satin_success: bool, log_delta: i64) -> String {
    let mut classes = Vec::new();
    if u64_of(&satin["history_gas"]) > 0 {
        classes.push("history".to_string());
    }
    if u64_of(&satin["state_gas"]) > 0 {
        classes.push("state".to_string());
    }
    if log_delta > 0 {
        classes.push(format!("+{log_delta} logs"));
    }
    if legacy_success != satin_success {
        classes.push("status".to_string());
    }
    if let Some(kind) = satin["limit_exceeded"]["kind"].as_str() {
        classes.push(format!("stop:{kind}"));
    }
    classes.join(", ")
}

fn render_fixtures(out: &mut String) {
    let dir = manifest_dir().join("tests/fixtures");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            name.strip_prefix("test_satin_").map(|rest| rest.trim_end_matches(".json").to_string())
        })
        .collect();
    names.sort();
    writeln!(out, "## `run` and `tx` fixtures\n").unwrap();
    writeln!(
        out,
        "Each fixture's command run on both engines: `tests/fixtures/test_<name>.json` and \
         `test_satin_<name>.json`, which is the same command with `--spec Satin`.\n"
    )
    .unwrap();
    writeln!(
        out,
        "| Fixture | Legacy | Satin | Satin - legacy | Regular | State | History | History bytes \
         | Logs | Classes |"
    )
    .unwrap();
    writeln!(out, "|---|---|---|---:|---:|---:|---:|---:|---|---|").unwrap();
    for name in names {
        let load = |file: String| -> Value {
            let fixture: Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join(file)).unwrap()).unwrap();
            fixture["expected"].clone()
        };
        let legacy = load(format!("test_{name}.json"));
        let satin = load(format!("test_satin_{name}.json"));
        let (legacy_gas, satin_gas) = (u64_of(&legacy["gas_used"]), u64_of(&satin["gas_used"]));
        let (legacy_logs, satin_logs) =
            (u64_of(&legacy["logs_count"]), u64_of(&satin["logs_count"]));
        let ledgers = &satin["satin"];
        writeln!(
            out,
            "| {name} | {} {legacy_gas} | {} {satin_gas} | {} | {} | {} | {} | {} | {legacy_logs} \
             -> {satin_logs} | {} |",
            summary_status(&legacy),
            summary_status(&satin),
            signed(satin_gas, legacy_gas),
            ledgers["regular_gas"],
            ledgers["state_gas"],
            ledgers["history_gas"],
            ledgers["history_bytes"],
            classes(
                ledgers,
                legacy["success"] == true,
                satin["success"] == true,
                satin_logs as i64 - legacy_logs as i64
            ),
        )
        .unwrap();
    }
}

/// One transaction of a recorded block, on both engines.
struct TxPair {
    target: String,
    selector: String,
    legacy: Value,
    satin: Value,
}

fn is_success(record: &Value) -> bool {
    record["status"] == "success"
}

fn render_blocks(out: &mut String) {
    let cache = cache_copy_with_absent_factory();
    let mut pairs = Vec::new();
    writeln!(out, "## Recorded mainnet blocks\n").unwrap();
    writeln!(
        out,
        "The three Rex6 blocks of `tests/fixtures/blocks`, each replayed on both engines. The \
         legacy replay of each matches the chain (every receipt and the receipts root), so the \
         legacy column is the chain's. Satin runs the transactions as signed, on the state \
         their parent block left.\n"
    )
    .unwrap();
    writeln!(
        out,
        "| Block | Transactions | Legacy gas | Satin gas | Satin / legacy | Regular | State | \
         History | Status differs | Logs | Refused |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|").unwrap();
    for number in BLOCKS {
        let replay = |spec: Option<&str>| -> Vec<Value> {
            let mut args = vec![
                "replay".to_string(),
                "--block".to_string(),
                number.to_string(),
                "--block-cache".to_string(),
                cache.path().to_str().unwrap().to_string(),
                "--json".to_string(),
            ];
            if let Some(spec) = spec {
                args.extend(["--override.spec".to_string(), spec.to_string()]);
            }
            let run = run_evme(&args);
            assert_eq!(run.code, 0, "{}", run.stderr);
            run.records()
        };
        let (legacy, satin) = (replay(None), replay(Some("Satin")));
        let block = |records: &[Value]| records.last().unwrap().clone();
        let (legacy_block, satin_block) = (block(&legacy), block(&satin));
        assert_eq!(legacy_block["matches_chain"], true, "the legacy replay of {number}");

        let recorded = read_block(cache.path(), number);
        let txs = recorded.block["transactions"].as_array().unwrap();
        let (mut status_differs, mut legacy_logs, mut satin_logs) = (0, 0, 0);
        for ((legacy, satin), tx) in legacy.iter().zip(&satin).zip(txs) {
            if legacy["kind"] != "tx" {
                continue;
            }
            status_differs += usize::from(is_success(legacy) != is_success(satin));
            legacy_logs += u64_of(&legacy["logs"]);
            satin_logs += u64_of(&satin["logs"]);
            let input = tx["input"].as_str().unwrap_or("0x");
            let (target, selector) = if tx["type"] == "0x7e" {
                ("deposit".to_string(), "-".to_string())
            } else {
                (
                    tx["to"].as_str().unwrap_or("create").to_string(),
                    if input.len() >= 10 { input[..10].to_string() } else { "-".to_string() },
                )
            };
            pairs.push(TxPair { target, selector, legacy: legacy.clone(), satin: satin.clone() });
        }
        let ledgers = &satin_block["satin"];
        writeln!(
            out,
            "| {number} | {} | {} | {} | {} | {} | {} | {} | {status_differs} | {legacy_logs} -> \
             {satin_logs} | {} |",
            legacy_block["transactions"],
            legacy_block["gas_used"],
            satin_block["gas_used"],
            ratio(u64_of(&satin_block["gas_used"]), u64_of(&legacy_block["gas_used"])),
            ledgers["regular_gas"],
            ledgers["state_gas"],
            ledgers["history_gas"],
            satin_block["refused"],
        )
        .unwrap();
    }

    writeln!(out, "\n## Recorded transactions by call target\n").unwrap();
    writeln!(
        out,
        "The transactions of the three blocks grouped by the contract they call and the \
         selector they call it with; `deposit` is the block's L1 attributes deposit, `-` a call \
         without calldata.\n"
    )
    .unwrap();
    writeln!(
        out,
        "| Call target | Selector | Transactions | Legacy gas | Satin gas | Satin / legacy | \
         State gas | Status differs | Logs |"
    )
    .unwrap();
    writeln!(out, "|---|---|---:|---:|---:|---:|---:|---:|---|").unwrap();
    let mut groups: BTreeMap<(String, String), Vec<&TxPair>> = BTreeMap::new();
    for pair in &pairs {
        groups.entry((pair.target.clone(), pair.selector.clone())).or_default().push(pair);
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    for ((target, selector), txs) in groups {
        let sum = |f: &dyn Fn(&TxPair) -> u64| txs.iter().map(|p| f(p)).sum::<u64>();
        let legacy_gas = sum(&|p| u64_of(&p.legacy["gas_used"]));
        let satin_gas = sum(&|p| u64_of(&p.satin["gas_used"]));
        let state_gas = sum(&|p| u64_of(&p.satin["satin"]["state_gas"]));
        let status = txs.iter().filter(|p| is_success(&p.legacy) != is_success(&p.satin)).count();
        let legacy_logs = sum(&|p| u64_of(&p.legacy["logs"]));
        let satin_logs = sum(&|p| u64_of(&p.satin["logs"]));
        writeln!(
            out,
            "| {target} | {selector} | {} | {legacy_gas} | {satin_gas} | {} | {state_gas} | \
             {status} | {legacy_logs} -> {satin_logs} |",
            txs.len(),
            ratio(satin_gas, legacy_gas),
        )
        .unwrap();
    }
}

fn render() -> String {
    let mut out = String::new();
    writeln!(out, "# Satin against the legacy engine: the expected differences\n").unwrap();
    writeln!(
        out,
        "Generated by `cargo test --manifest-path bin/mega-evme/Cargo.toml --test differences`; \
         the test compares this file to what it renders, so every number here is one the two \
         engines produced. After an intended change of either, rewrite it with \
         `UPDATE_EVME_DIFFERENCES=1`.\n"
    )
    .unwrap();
    writeln!(
        out,
        "Legacy is `Rex6` on the released `mega-evm` 1.7.1. Satin is the in-tree engine at a \
         cost per state byte of {COST_PER_STATE_BYTE} and a cost per history byte of \
         {COST_PER_HISTORY_BYTE}, the minimum SALT bucket everywhere. Every row runs one input on \
         both engines.\n"
    )
    .unwrap();
    writeln!(out, "What the Satin columns show:\n").unwrap();
    for line in [
        "- **Regular**: execution, at the Satin schedule (Amsterdam's, with the entries EIP-8038 \
         repriced back at Osaka's) and EIP-2780's intrinsic cost.",
        "- **State**: EIP-8037 state gas for the state a transaction adds (a slot, an account, \
         deployed code), at the cost per state byte.",
        "- **History**: gas for the bytes a transaction appends to history (its body, one \
         40-byte record per kept write, its logs, its deployed code), at the cost per history \
         byte; **History bytes** is their count. The legacy engine has neither ledger.",
        "- **Logs**: Satin emits an EIP-7708 `Transfer` log for every value movement; the \
         legacy engine does not.",
        "- **Classes**: the mechanisms a row shows, read off its numbers: `history`, `state`, \
         `+N logs`, `status` (success differs), `stop:<kind>` (a Satin limit stopped it).",
    ] {
        writeln!(out, "{line}").unwrap();
    }
    writeln!(out).unwrap();
    render_fixtures(&mut out);
    writeln!(out).unwrap();
    render_blocks(&mut out);
    out
}

#[test]
fn test_the_difference_table_is_current() {
    let rendered = render();
    let path = manifest_dir().join(TABLE);
    if std::env::var_os("UPDATE_EVME_DIFFERENCES").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let pinned = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        pinned == rendered,
        "{TABLE} is out of date; rerun with UPDATE_EVME_DIFFERENCES=1 and review the diff.\n\n\
         Rendered:\n{rendered}"
    );
}
