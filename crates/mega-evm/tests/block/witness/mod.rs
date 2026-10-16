//! A stateless validator re-executes a Satin block from a witness: what the block read, as the
//! chain held it. These tests execute blocks on a recorder of every read, replay them on a strict
//! database and environments that serve exactly the record, and hold the replay to the original
//! run — receipts, state changes, gas ledgers, logs, block counters and the executor's exports —
//! for each mechanism that reads, and for each read that gas or a limit can skip.

mod basics;
mod deploys;
mod harness;
mod history;
mod keyless;
mod oracle;
mod skips;
mod state;
mod stops;
mod system;
