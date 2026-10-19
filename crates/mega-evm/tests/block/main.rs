//! Tests of the block executor.

mod activation;
mod block_hashes;
mod bucket_ids;
mod building_policy;
mod common;
mod counters;
mod deploy;
mod deposits;
mod detention;
mod eips;
mod external_envs;
mod factory;
mod inspector;
mod l1_attributes;
mod limits;
mod receipt_goldens;
mod receipts;
mod rules;
mod salt;
mod schedule;
mod sequencer_registry;
mod sequences;
#[path = "../shared/snapshot.rs"]
mod snapshot;
mod state_gas;
mod transfer_log_order;
mod transfer_logs;
mod witness;
