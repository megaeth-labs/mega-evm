//! Tests of the block executor.

mod activation;
mod block_hashes;
mod bucket_ids;
mod building_policy;
mod common;
mod counters;
mod deposits;
mod detention;
mod eips;
mod external_envs;
mod factory;
mod inspector;
mod l1_attributes;
mod limits;
mod predeploys;
mod receipt_goldens;
mod receipts;
mod rules;
mod salt;
mod schedule;
mod sequencer_registry;
mod sequences;
// This target compares a pre-block call alone: the reference's transactions and the reservoir it
// reads are the `satin` target's.
#[allow(dead_code)]
#[path = "../shared/op_reference.rs"]
mod op_reference;
#[path = "../shared/snapshot.rs"]
mod snapshot;
mod stale;
mod state_gas;
mod system_tx_budgets;
mod transfer_logs;
mod witness;
