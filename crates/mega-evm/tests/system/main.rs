//! Tests of the system contracts: the interceptor dispatch, what each contract answers, and the
//! system-address transaction.

mod access_control;
mod common;
mod control;
mod deploy;
mod dispatch;
mod keyless;
mod limit_control;
mod oracle;
mod oracle_storage;
mod remaining_compute_gas;
#[path = "../shared/snapshot.rs"]
mod snapshot;
mod system_tx;
