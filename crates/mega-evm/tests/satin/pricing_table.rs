//! The Satin schedule and what it charges, rendered into `pricing-table.md` next to this file.
//!
//! Three kinds of row. A *schedule* row reads one entry out of the Osaka, the Amsterdam and the
//! Satin schedules, so the two changes Satin makes are visible side by side. A *measured* row
//! runs one probe transaction and reports what it cost, split across the ledgers — every number
//! there comes from a transaction the engine ran, not from arithmetic on the schedule. A *SALT
//! scaling* row runs one of those probes again with the bucket its state charge lands in at a
//! larger capacity, so what the multiplier does to each ledger is visible in the same units.
//!
//! The test renders the table and compares it to the checked-in file, so a change to either the
//! schedule or the engine that moves a number fails here. Regenerate it after an intentional
//! change:
//!
//! ```text
//! UPDATE_SATIN_PRICING_TABLE=1 cargo test -p mega-evm --test satin
//! ```

use std::{fmt::Write as _, fs, path::Path};

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    satin_gas_params,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaEvm, MegaGasUsage, MegaTransaction,
};
use revm::{
    bytecode::opcode::{CALL, LOG1, PUSH0, RETURN},
    context_interface::cfg::{GasId, GasParams},
    primitives::hardfork::SpecId,
};

use crate::{
    common::{call, context, create, runs_at_measurement_prices},
    salt::{crowded_account, crowded_slot, minimal_envs, salt_context, SaltEnvs},
};

const CALLER: Address = address!("0000000000000000000000000000000000b00000");
const CONTRACT: Address = address!("0000000000000000000000000000000000b00001");
const FUNDED: Address = address!("0000000000000000000000000000000000b00002");
const EMPTY: Address = address!("0000000000000000000000000000000000b00003");

/// Below the execution cap, so every probe's reservoir is empty and each state charge spills onto
/// the regular budget — which is why a probe's total includes its state column.
const GAS_LIMIT: u64 = 5_000_000;

/// Where the rendered table lives.
fn table_path() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/satin/pricing-table.md"))
}

/// A database the probes run on: a funded sender, a funded account to transfer to, and a
/// contract running `code` with a balance of its own, so a probe's inner `CALL` can carry value.
fn db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(FUNDED, U256::from(1))
        .account_balance(CONTRACT, U256::from(10u64.pow(9)))
        .account_code(CONTRACT, code)
}

/// One measured row.
struct Measured {
    probe: &'static str,
    gas: MegaGasUsage,
}

/// One SALT scaling row: a probe run with the bucket its state charge lands in at multiplier
/// `m`.
struct Scaled {
    probe: &'static str,
    m: u64,
    gas: MegaGasUsage,
}

/// Runs `tx` on `db` and reports what it spent by ledger. The probe must succeed.
fn measure(probe: &'static str, db: MemoryDatabase, tx: MegaTransaction) -> Measured {
    let mut evm = MegaEvm::new(context(db));
    let outcome = evm.execute_transaction(tx).expect("the probe is valid");
    assert!(outcome.result.is_success(), "{probe}: {:?}", outcome.result);
    Measured { probe, gas: outcome.gas }
}

/// Runs `tx` on `db` with `envs` as the SALT environment and reports what it spent.
fn measure_scaled(
    probe: &'static str,
    m: u64,
    db: MemoryDatabase,
    tx: MegaTransaction,
    envs: SaltEnvs,
) -> Scaled {
    let mut evm = MegaEvm::new(salt_context(db, envs));
    let outcome = evm.execute_transaction(tx).expect("the probe is valid");
    assert!(outcome.result.is_success(), "{probe} at m = {m}: {:?}", outcome.result);
    Scaled { probe, m, gas: outcome.gas }
}

/// Runs `code` in `CONTRACT` as a plain call from `CALLER`.
fn measure_call(probe: &'static str, code: BytecodeBuilder) -> Measured {
    let code = code.stop().build();
    measure(probe, db(code), call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT))
}

/// Runs a creation transaction whose init code deploys `len` bytes of zeros.
fn measure_create(probe: &'static str, len: u64) -> Measured {
    let init_code =
        BytecodeBuilder::default().push_number(len).append_many([PUSH0, RETURN]).build();
    measure(probe, db(Bytes::new()), create(CALLER, init_code, GAS_LIMIT))
}

/// `CALL(gas, target, value, 0, 0, 0, 0)`.
fn value_call(target: Address) -> BytecodeBuilder {
    BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(1u64)
        .push_address(target)
        .push_number(1_000_000u64)
        .append(CALL)
}

fn sstore(slot: u64, value: u64) -> BytecodeBuilder {
    BytecodeBuilder::default().sstore(U256::from(slot), U256::from(value))
}

fn render() -> String {
    let osaka = GasParams::new_spec(SpecId::OSAKA);
    let amsterdam = GasParams::new_spec(SpecId::AMSTERDAM);
    let satin = satin_gas_params();

    let mut out = String::new();
    out.push_str("# The Satin gas table\n\n");
    out.push_str(
        "Generated by `cargo test -p mega-evm --test satin`; the test compares this file to what \
         it renders, so every number here is one the engine produced.\n\n",
    );

    out.push_str("## Schedule entries\n\n");
    out.push_str(
        "Every named entry of the schedule, at the Osaka price, at the Amsterdam (glamsterdam \
         devnet-8) price and at the Satin price. Satin is Amsterdam with the EIP-8038 repricing \
         pressed back to Osaka, the EIP-8037 state entries rebuilt at `MegaETH`'s cost per state \
         byte, and deposited code priced at `MegaETH`'s cost per history byte; the `Satin` column \
         differs from `Amsterdam` in exactly the entries where one of those three applies.\n\n",
    );
    out.push_str("| Gas id | Osaka | Amsterdam | Satin |\n|---|---:|---:|---:|\n");
    for slot in 0..=u8::MAX {
        let id = GasId::new(slot);
        let name = id.name();
        if name == "unknown" {
            continue;
        }
        let _ = writeln!(
            out,
            "| {name} | {} | {} | {} |",
            osaka.get(id),
            amsterdam.get(id),
            satin.get(id)
        );
    }

    out.push_str("\n## Measured transactions\n\n");
    let _ = writeln!(
        out,
        "One probe transaction each, at a {GAS_LIMIT} gas limit. `gas used` is the receipt's \
         figure; the three ledgers below it are the raw spend split by what the gas paid for.\n"
    );
    out.push_str("| Probe | gas used | regular | state | history |\n|---|---:|---:|---:|---:|\n");
    for row in measured() {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            row.probe, row.gas.gas_used, row.gas.regular, row.gas.state, row.gas.history
        );
    }

    out.push_str("\n## SALT scaling\n\n");
    out.push_str(
        "The same probes again, with the SALT bucket the state charge lands in at `m` times the \
         minimum capacity. `m` multiplies the state ledger and nothing else: the regular and the \
         history columns are the same on all three rows of a probe, and `gas used` grows by \
         exactly the state column's growth, because these probes run below the execution cap \
         and every state charge spills onto the regular budget.\n\n",
    );
    out.push_str(
        "| Probe | m | gas used | regular | state | history |\n|---|---:|---:|---:|---:|---:|\n",
    );
    for row in scaled() {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            row.probe, row.m, row.gas.gas_used, row.gas.regular, row.gas.state, row.gas.history
        );
    }

    out.push_str("\n## The execution cap\n\n");
    let _ = writeln!(
        out,
        "Satin pins `tx_gas_limit_cap` at {TX_GAS_LIMIT_CAP}, so a transaction's EIP-8037 \
         reservoir is `max(0, gas_limit - intrinsic gas)` above it. Every probe above runs well \
         below the cap, so its reservoir is empty and each state charge comes out of the regular \
         budget."
    );
    out
}

/// The probes, in the order the table lists them.
fn measured() -> Vec<Measured> {
    vec![
        measure("empty call", db(Bytes::new()), call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT)),
        measure(
            "value transfer to an existing account",
            db(Bytes::new()),
            call(CALLER, FUNDED, U256::from(1), GAS_LIMIT),
        ),
        measure("self-transfer", db(Bytes::new()), call(CALLER, CALLER, U256::from(1), GAS_LIMIT)),
        measure_call("SSTORE 0 -> 1", sstore(0, 1)),
        measure_call(
            "SSTORE 0 -> 1 -> 0",
            BytecodeBuilder::default()
                .sstore(U256::ZERO, U256::from(1))
                .sstore(U256::ZERO, U256::ZERO),
        ),
        measure_call("value CALL to an empty account", value_call(EMPTY)),
        measure_call(
            "LOG1 with 32 data bytes",
            BytecodeBuilder::default()
                .push_number(0u64)
                .push_number(32u64)
                .push_number(0u64)
                .append(LOG1),
        ),
        measure_create("create transaction deploying 0 bytes", 0),
        measure_create("create transaction deploying 32 bytes", 32),
    ]
}

/// The SALT scaling rows, in the order the table lists them.
///
/// One slot-scoped charge and one account-scoped charge, each at the minimum bucket and at two
/// larger ones. The `m = 1` row of each is the same number as the measured row above it.
fn scaled() -> Vec<Scaled> {
    const SLOT: u64 = 0;
    let mut rows = Vec::new();
    for m in [1, 2, 8] {
        rows.push(measure_scaled(
            "SSTORE 0 -> 1",
            m,
            db(sstore(SLOT, 1).stop().build()),
            call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT),
            crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), m),
        ));
    }
    for m in [1, 2, 8] {
        rows.push(measure_scaled(
            "value CALL to an empty account",
            m,
            db(value_call(EMPTY).stop().build()),
            call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT),
            crowded_account(minimal_envs(), EMPTY, m),
        ));
    }
    rows
}

/// The rendered table is the one checked in. `UPDATE_SATIN_PRICING_TABLE=1` writes it instead.
#[test]
fn test_the_pricing_table_is_up_to_date() {
    if runs_at_measurement_prices() {
        return;
    }
    let rendered = render();
    let path = table_path();
    if std::env::var_os("UPDATE_SATIN_PRICING_TABLE").is_some() {
        fs::write(path, &rendered).expect("the table is writable");
        return;
    }
    let checked_in = fs::read_to_string(path).expect("the table is checked in");
    assert_eq!(
        checked_in, rendered,
        "pricing-table.md is out of date; regenerate it with \
         UPDATE_SATIN_PRICING_TABLE=1 cargo test -p mega-evm --test satin"
    );
}

/// The schedule section covers every entry, and the entries Satin moves are the ones the two
/// rules name and no others — read off the rendered table rather than the schedule, so the table
/// cannot drift from what it claims.
#[test]
fn test_the_table_shows_where_satin_differs_from_amsterdam() {
    if runs_at_measurement_prices() {
        return;
    }
    let rendered = render();
    let differing: Vec<&str> = rendered
        .lines()
        .filter(|line| line.starts_with("| ") && line.matches('|').count() == 5)
        .filter_map(|line| {
            let mut cells = line.split('|').map(str::trim).skip(1);
            let name = cells.next()?;
            let (_osaka, amsterdam, satin) = (cells.next()?, cells.next()?, cells.next()?);
            (name != "Gas id" && amsterdam != satin).then_some(name)
        })
        .collect();

    // The pressed-back entries whose Osaka and Amsterdam prices are not the same number. The
    // other three of the seventeen — `warm_storage_read_cost`, `sstore_static` and
    // `cold_storage_additional_cost` — EIP-8038 left where they were, so pressing them back
    // moves nothing. The state entries are rebuilt rather than copied, and land on Amsterdam's
    // numbers because the provisional cost per state byte is the one Glamsterdam uses. The
    // history entry is the one Satin adds: upstream prices no history byte at all.
    assert_eq!(
        differing,
        [
            "create",
            "transfer_value_cost",
            "cold_account_additional_cost",
            "new_account_cost",
            "sstore_set_without_load_cost",
            "sstore_reset_without_cold_load_cost",
            "sstore_clearing_slot_refund",
            "cold_storage_cost",
            "new_account_cost_for_selfdestruct",
            "tx_access_list_address_cost",
            "tx_access_list_storage_key_cost",
            "tx_create_cost",
            "sstore_set_refund",
            "sstore_reset_refund",
            "code_deposit_history_gas",
        ],
        "the entries Satin prices differently from Amsterdam"
    );
}

/// The SALT scaling section says what it claims: on each probe the regular ledger and the
/// history ledger are the same at every multiplier [S6.5] — history gas is not scaled by SALT
/// [S7.2] — and the state ledger is the minimum bucket's times the multiplier. Read off the
/// rendered table rather than the engine, so the table cannot drift from the claim above it
/// (`constants`: the relation across `m` is the spec's; the `m` = 1 row is the table's own).
#[test]
fn test_the_table_shows_the_multiplier_on_the_state_ledger_alone() {
    if runs_at_measurement_prices() {
        return;
    }
    let rendered = render();
    let section = rendered.split("## SALT scaling").nth(1).expect("the scaling section");
    let section = section.split("\n## ").next().expect("the section ends at the next heading");

    let mut baselines: Vec<(String, u64, u64, u64)> = Vec::new();
    for line in section.lines().filter(|l| l.starts_with("| ") && l.matches('|').count() == 7) {
        let mut cells = line.split('|').map(str::trim).skip(1);
        let probe = cells.next().expect("probe").to_string();
        let m = cells.next().expect("m");
        if m == "m" {
            continue;
        }
        let m: u64 = m.parse().expect("m is a number");
        let _gas_used = cells.next();
        let regular: u64 = cells.next().expect("regular").parse().expect("a number");
        let state: u64 = cells.next().expect("state").parse().expect("a number");
        let history: u64 = cells.next().expect("history").parse().expect("a number");

        match baselines.iter().find(|(name, ..)| *name == probe) {
            None => {
                assert_eq!(m, 1, "{probe}: the first row of a probe is the minimum bucket");
                baselines.push((probe, regular, state, history));
            }
            Some((_, base_regular, base_state, base_history)) => {
                assert_eq!(regular, *base_regular, "{probe} at m = {m}: regular gas moved");
                assert_eq!(state, base_state * m, "{probe} at m = {m}: state gas");
                assert_eq!(history, *base_history, "{probe} at m = {m}: history gas moved");
            }
        }
    }
    assert_eq!(baselines.len(), 2, "one slot-scoped probe and one account-scoped probe");
    assert!(baselines.iter().all(|(_, _, state, _)| *state > 0), "a probe must charge state gas");
    assert!(
        baselines.iter().all(|(_, _, _, history)| *history > 0),
        "a probe must pay history gas, or the column would hold constant for nothing"
    );
}
