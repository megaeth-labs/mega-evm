//! The byte prices as an axis.
//!
//! The prices are fixed once per process, so a transaction cannot be run at two prices in one
//! test. Two things can be checked all the same:
//!
//! - the schedule built from the prices is monotone in them, in the process, through the pure
//!   builder `satin_gas_params_at`: every state-gas entry grows with the cost per state byte, the
//!   deposited code's history entry with the cost per history byte, nothing else moves, and the gas
//!   a byte count costs grows with the price and with the count;
//! - the outcome of a transaction at one price against the same transaction at another, across two
//!   processes: under the `satin-price-override` feature, the record test writes what every case
//!   spent to the file `MEGA_FUZZ_PRICE_RECORD` names, and `scripts/fuzz_price_monotonic.py`
//!   compares two records. Where the two runs followed the same path — the same result kind, logs,
//!   output and state but for balances — the state gas is monotone in the cost per state byte and
//!   the history gas in the cost per history byte. Where the path differs (a limit stopped one run,
//!   an out-of-gas came sooner), nothing is implied: a dearer byte can make a transaction spend
//!   less in total by stopping it earlier, so total gas is not monotone and is not asserted.

use mega_evm::{
    satin_gas_params_at, BytePrice, SatinPrices, HISTORY_GAS_PRICED, STATE_GAS_REPRICED,
};
use proptest::prelude::*;
use revm::context_interface::cfg::GasId;

use crate::harness::{check, prop_check, prop_eq};

/// A price in milli-gas per byte, up to twenty times the dearest candidate.
fn milli_price() -> impl Strategy<Value = u64> {
    prop_oneof![
        1 => Just(0u64),
        1 => Just(1),
        2 => 1u64..=1_000,
        4 => 1_000u64..=200_000_000,
    ]
}

/// The schedule is monotone in the byte prices: every state-gas entry grows with the cost per
/// state byte, the deposited code's history entry with the cost per history byte, and every other
/// entry stands; and the gas a byte count costs grows with the price and with the count.
#[test]
fn test_property_schedule_is_monotone_in_the_byte_prices() {
    check(
        "schedule_is_monotone_in_the_byte_prices",
        256,
        || (milli_price(), milli_price(), milli_price(), milli_price(), 0u64..=100_000),
        |&(a, b, cphb, cphb2, bytes)| {
            let (low, high) = (a.min(b), a.max(b));
            let at = |cpsb: u64, cphb: u64| {
                satin_gas_params_at(SatinPrices {
                    cpsb: BytePrice::from_milli_gas(cpsb),
                    cphb: BytePrice::from_milli_gas(cphb),
                })
            };
            let state_entries: Vec<usize> =
                STATE_GAS_REPRICED.iter().map(|(id, _)| id().as_usize()).collect();
            let history_entries: Vec<usize> =
                HISTORY_GAS_PRICED.iter().map(|(id, _)| id().as_usize()).collect();
            // Along the cost per state byte: the state-gas entries grow, everything else stands.
            let (lower, higher) = (at(low, cphb), at(high, cphb));
            for (index, (l, h)) in lower.table().iter().zip(higher.table().iter()).enumerate() {
                if state_entries.contains(&index) {
                    prop_check!(
                        l <= h,
                        "a state-gas entry falls as the cost per state byte rises: {index}"
                    );
                } else {
                    prop_eq!(
                        l,
                        h,
                        "an entry the cost per state byte does not set moved: {:?}",
                        GasId::new(index as u8)
                    );
                }
            }
            // Along the cost per history byte: the one history entry revm charges itself, the
            // deposited code's, grows, and everything else stands.
            let (lo_h, hi_h) = (cphb.min(cphb2), cphb.max(cphb2));
            let (lower, higher) = (at(low, lo_h), at(low, hi_h));
            for (index, (l, h)) in lower.table().iter().zip(higher.table().iter()).enumerate() {
                if history_entries.contains(&index) {
                    prop_check!(
                        l <= h,
                        "a history entry falls as the cost per history byte rises: {index}"
                    );
                } else {
                    prop_eq!(
                        l,
                        h,
                        "an entry the cost per history byte does not set moved: {:?}",
                        GasId::new(index as u8)
                    );
                }
            }

            let (cheap, dear) = (
                BytePrice::from_milli_gas(cphb.min(cphb2)),
                BytePrice::from_milli_gas(cphb.max(cphb2)),
            );
            prop_check!(
                cheap.gas_for(bytes).unwrap_or(u64::MAX) <= dear.gas_for(bytes).unwrap_or(u64::MAX),
                "the gas a byte count costs falls as the price rises"
            );
            prop_check!(
                dear.gas_for(bytes).unwrap_or(u64::MAX) <=
                    dear.gas_for(bytes.saturating_add(1)).unwrap_or(u64::MAX),
                "the gas a byte count costs falls as the count grows"
            );
            // A fixed-size item is a record or a body: the helper takes at most a thousand bytes.
            let fixed = bytes.min(1_000);
            prop_check!(
                cheap.fixed_size_gas(fixed) <= dear.fixed_size_gas(fixed),
                "the fixed-size gas falls as the price rises"
            );
            Ok(())
        },
    );
}

/// Under the `satin-price-override` feature, when `MEGA_FUZZ_PRICE_RECORD` names a file, writes
/// one line per case — its index, the prices in effect, the result kind, the state and history
/// gas, the history bytes, the gas used, and a hash of the path the transaction followed — so two
/// runs at two prices can be compared with `scripts/fuzz_price_monotonic.py`.
#[cfg(feature = "satin-price-override")]
#[test]
fn test_property_price_record() {
    use std::io::Write as _;

    use crate::{
        gen::{case::case, Flavor},
        render::render_state,
    };
    use revm::context::result::ExecutionResult;

    let Ok(path) = std::env::var("MEGA_FUZZ_PRICE_RECORD") else { return };
    let prices = mega_evm::active_satin_prices();
    let lines = std::sync::Mutex::new(Vec::new());
    let index = std::sync::atomic::AtomicU64::new(0);
    check(
        "price_record",
        512,
        || case(Flavor::Satin),
        |case| {
            let i = index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let line = match case.execute() {
                Ok(outcome) => {
                    let kind = match &outcome.result {
                        ExecutionResult::Success { .. } => "success",
                        ExecutionResult::Revert { .. } => "revert",
                        ExecutionResult::Halt { .. } => "halt",
                    };
                    let mut path = String::new();
                    path.push_str(&format!(
                        "{:?}|{:?}|",
                        outcome.result.logs(),
                        outcome.result.output()
                    ));
                    let mut state = render_state(&outcome.state);
                    // Balances move with the fees, which move with the prices: they are not the
                    // path.
                    state = state
                        .lines()
                        .map(|line| match line.find(" balance=") {
                            Some(start) => {
                                let end = line[start + 1..]
                                    .find(' ')
                                    .map_or(line.len(), |e| start + 1 + e);
                                format!("{}{}", &line[..start], &line[end..])
                            }
                            None => line.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    path.push_str(&state);
                    format!(
                        "{i} {} {} {kind} {} {} {} {} {}",
                        prices.cpsb.milli_gas(),
                        prices.cphb.milli_gas(),
                        outcome.gas.state,
                        outcome.gas.history,
                        outcome.gas.history_bytes,
                        outcome.gas.gas_used,
                        alloy_primitives::keccak256(path.as_bytes())
                    )
                }
                Err(_) => format!(
                    "{i} {} {} refused 0 0 0 0 0x",
                    prices.cpsb.milli_gas(),
                    prices.cphb.milli_gas()
                ),
            };
            lines.lock().unwrap().push(line);
            Ok(())
        },
    );
    let mut file = std::fs::File::create(&path).expect("the record is written");
    for line in lines.lock().unwrap().iter() {
        writeln!(file, "{line}").expect("the record is written");
    }
}
