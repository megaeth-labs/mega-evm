//! A keyless deployment under gas detention.
//!
//! The `keylessDeploy` call is the transaction's own frame and runs no code: it charges its own
//! work — the overhead, then the `CREATE` opcode's regular gas — on gas held to what the compute
//! limit leaves it, and its creation is an ordinary frame below it. So a deployment is detained
//! as any transaction is: a read of volatile data anywhere in it caps what it may still compute,
//! its charges count as compute, and a crossing stops the transaction with the deployment taken
//! back whole.

use alloy_sol_types::SolError;
use mega_evm::{
    constants::BLOCK_ENV_ACCESS_COMPUTE_GAS, system::keyless::KEYLESS_DEPLOY_OVERHEAD_GAS,
    LimitCheck, LimitKind, MegaLimitExceeded, VolatileDataAccess,
};
use revm::{context::BlockEnv, primitives::KECCAK_EMPTY};

use super::*;
use crate::common::{call_tx, context};

/// The block beneficiary of the cases where nobody in the transaction is the beneficiary.
const BENEFICIARY: Address = address!("0x0000000000000000000000000000000000beef02");

/// The cap a read of the block environment sets, by default.
const CAP: u64 = BLOCK_ENV_ACCESS_COMPUTE_GAS;

/// What a transaction did, and what gas detention made of it.
struct Run {
    outcome: MegaTransactionOutcome,
    limit: Option<u64>,
    accessed: VolatileDataAccess,
}

/// Runs the `keylessDeploy` transaction `data` from the relayer over `db` at `gas_limit`, in a
/// block whose beneficiary is `beneficiary`, under the default limits with the block-environment
/// cap at `cap`.
fn run_in(db: MemoryDatabase, beneficiary: Address, data: Bytes, gas_limit: u64, cap: u64) -> Run {
    let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
    let ctx =
        context(db).with_block(BlockEnv { beneficiary, ..block() }).with_tx_runtime_limits(limits);
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, data, U256::ZERO);
    tx.0.base.gas_limit = gas_limit;
    let mut evm = MegaEvm::new(ctx);
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let detention = evm.ctx().detention();
    Run { outcome, limit: detention.compute_limit(), accessed: detention.accessed() }
}

/// Asserts `run` of `deployment` at `gas_limit` was stopped by gas detention, having computed
/// exactly its limit past its intrinsic gas, and kept nothing of the deployment.
fn assert_stopped(run: &Run, deployment: &Deployment, gas_limit: u64) {
    let limit = run.limit.expect("a read set a limit");
    let ExecutionResult::Revert { output, .. } = &run.outcome.result else {
        panic!("expected the detention stop, got {:?}", run.outcome.result);
    };
    assert_eq!(
        MegaLimitExceeded::abi_decode(output).expect("the revert data is a limit stop"),
        MegaLimitExceeded { kind: LimitKind::ComputeGas.as_u8(), limit },
    );
    assert_eq!(
        run.outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::ComputeGas,
            limit,
            used: limit,
            frame_local: false,
        }),
    );
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
    assert_eq!(
        run.outcome.gas.regular,
        reference.gas.regular + limit,
        "the transaction computed up to its limit and not one unit past it, at {gas_limit}",
    );
    assert_eq!(run.outcome.gas.state, 0, "a stop keeps no state");
    assert_eq!(nonce(&run.outcome, deployment.signer), 0, "the signer's nonce is taken back");
    assert!(code_hash(&run.outcome, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY));
}

/// A signer that is the block beneficiary: the call reads the beneficiary's account for its
/// rules, through the journal, and the creation runs for that account, so the deployment is
/// detained from the read, at the call's compute then — the overhead — as a sender that is the
/// beneficiary is from its start. Under the spec's cap it deploys as without the read. Under a cap
/// its `CREATE` opcode's regular gas crosses, it stops at that charge, before the creation starts.
/// A signer that is not the beneficiary is not detained.
#[test]
fn test_a_signer_that_is_the_beneficiary_detains_its_deployment() {
    let init_code = deploying(&runtime(1));
    let deployment = Deployment::new(init_code.clone());
    let data = deployment.call_data(LARGE_OVERRIDE);
    for gas_limit in GAS_LIMITS {
        let plain =
            run_in(db_for(&deployment, U256::ZERO), BENEFICIARY, data.clone(), gas_limit, 1);
        assert_eq!(returned(&plain.outcome).deployedAddress, deployment.address);
        assert_eq!((plain.limit, plain.accessed), (None, VolatileDataAccess::empty()));

        let signer = deployment.signer;
        let detained =
            run_in(db_for(&deployment, U256::ZERO), signer, data.clone(), gas_limit, CAP);
        assert_eq!(detained.accessed, VolatileDataAccess::BENEFICIARY_BALANCE);
        assert_eq!(detained.limit, Some(KEYLESS_DEPLOY_OVERHEAD_GAS + CAP), "read at the overhead");
        assert_eq!(detained.outcome.result, plain.outcome.result, "at {gas_limit}");
        assert_eq!(detained.outcome.gas, plain.outcome.gas, "at {gas_limit}");

        let cap = create_regular(init_code.len()) - 1;
        let stopped = run_in(db_for(&deployment, U256::ZERO), signer, data.clone(), gas_limit, cap);
        assert_eq!(stopped.limit, Some(KEYLESS_DEPLOY_OVERHEAD_GAS + cap));
        assert_stopped(&stopped, &deployment, gas_limit);
    }
}
