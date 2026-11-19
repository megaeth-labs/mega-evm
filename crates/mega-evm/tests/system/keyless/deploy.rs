//! What a keyless deployment deploys and reports: the native creation runs the init code as any
//! creation does, and the call answers with what it did.

use alloy_primitives::{address, hex, B256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    system::{
        keyless::{
            tests::{
                CREATE2_FACTORY_CODE_HASH, CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_DEPLOYER,
                CREATE2_FACTORY_TX, EIP1820_CODE_HASH, EIP1820_CONTRACT, EIP1820_DEPLOYER,
                EIP1820_TX,
            },
            IKeylessDeploy, KEYLESS_DEPLOY_OVERHEAD_GAS,
        },
        IOracle, ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase},
    MegaTransaction,
};
use revm::{
    bytecode::opcode::{
        CALL, CREATE, DELEGATECALL, GAS, GASPRICE, JUMP, JUMPDEST, LOG0, LOG1, MLOAD, ORIGIN, POP,
        PUSH0, REVERT, SELFDESTRUCT, SSTORE, STATICCALL,
    },
    context::BlockEnv,
    primitives::KECCAK_EMPTY,
    Database, DatabaseCommit,
};

use super::*;
use crate::common::{calls_with, split_outcome, with_contract, CALLER as RELAYER, CONTRACT};

/// An account the deployments below call into.
const HELPER: Address = address!("0x0000000000000000000000000000000000500000");

/// The beneficiary of the blocks the fee tests run in.
const BENEFICIARY: Address = address!("0x0000000000000000000000000000000000beef01");

/// Init code that runs `prefix` and deploys a one-byte runtime.
fn running(prefix: BytecodeBuilder) -> Bytes {
    constructor(&prefix.build_vec(), &runtime(1))
}

/// A `CALL` of `target` with no value and no data, forwarding all the gas, its status popped.
fn calling(code: BytecodeBuilder, target: Address, scheme: u8) -> BytecodeBuilder {
    let code = code.push_number(0_u64).push_number(0_u64).push_number(0_u64).push_number(0_u64);
    let code = if scheme == CALL { code.push_number(0_u64) } else { code };
    code.push_address(target).append(GAS).append(scheme).append(POP)
}

/// The canonical `CREATE2` factory deploys at its canonical address, from its canonical signer,
/// with or without a reservoir, and its signer's nonce is spent.
#[test]
fn test_keyless_deploy_create2_factory() {
    for gas_limit in GAS_LIMITS {
        let data = keyless_deploy_call(CREATE2_FACTORY_TX, U256::from(LARGE_OVERRIDE));
        let outcome = run_with(system_db(), data, gas_limit, EvmTxRuntimeLimits::no_limits());
        let ret = returned(&outcome);
        assert_eq!(ret.deployedAddress, CREATE2_FACTORY_CONTRACT, "at {gas_limit}");
        assert!(ret.errorData.is_empty());
        assert!(ret.gasUsed > 0);
        assert_eq!(code_hash(&outcome, CREATE2_FACTORY_CONTRACT), Some(CREATE2_FACTORY_CODE_HASH));
        assert_eq!(nonce(&outcome, CREATE2_FACTORY_DEPLOYER), 1);
    }
}

/// The EIP-1820 registry, the other canonical Nick's-Method deployment.
#[test]
fn test_keyless_deploy_eip1820() {
    for gas_limit in GAS_LIMITS {
        let data = keyless_deploy_call(EIP1820_TX, U256::from(LARGE_OVERRIDE));
        let outcome = run_with(system_db(), data, gas_limit, EvmTxRuntimeLimits::no_limits());
        assert_eq!(returned(&outcome).deployedAddress, EIP1820_CONTRACT, "at {gas_limit}");
        assert_eq!(code_hash(&outcome, EIP1820_CONTRACT), Some(EIP1820_CODE_HASH));
        assert_eq!(nonce(&outcome, EIP1820_DEPLOYER), 1);
    }
}

#[test]
fn test_a_keyless_deployment_returns_the_deploy_code_hash() {
    let code = runtime(3);
    let deployment = Deployment::new(deploying(&code));
    let outcome = deploy(system_db(), &deployment, GAS_LIMITS[0]);
    assert_eq!(code_hash(&outcome, deployment.address), Some(keccak(&code)));
}

/// The hash of `code`.
fn keccak(code: &[u8]) -> B256 {
    alloy_primitives::keccak256(code)
}

/// What a deployment writes survives a commit: the account is created with its code.
#[test]
fn test_a_deployment_survives_a_commit() {
    for gas_limit in GAS_LIMITS {
        let mut db = system_db();
        let data = keyless_deploy_call(CREATE2_FACTORY_TX, U256::from(LARGE_OVERRIDE));
        let outcome = run_with(db.clone(), data, gas_limit, EvmTxRuntimeLimits::no_limits());
        assert!(outcome.result.is_success());
        db.commit(outcome.result_and_state.state);
        let deployed = db.basic(CREATE2_FACTORY_CONTRACT).unwrap().expect("the account exists");
        assert_eq!(deployed.code_hash, CREATE2_FACTORY_CODE_HASH);
        assert_eq!(deployed.nonce, 1);
    }
}

/// What the creation of a keyless deployment returned into its call: its regular gas and its
/// reservoir once revm's settlement of the result, which runs after the inspector sees the
/// creation end, rolled a failure's state and history gas back.
#[derive(Default)]
struct CreationReturn(Option<(u64, u64)>);

impl<DB: Database> revm::Inspector<MegaContext<DB>, revm::interpreter::interpreter::EthInterpreter>
    for CreationReturn
{
    fn create_end(
        &mut self,
        _: &mut MegaContext<DB>,
        _: &revm::interpreter::CreateInputs,
        outcome: &mut revm::interpreter::CreateOutcome,
    ) {
        let mut gas = outcome.result.gas;
        revm::handler::handle_reservoir_remaining_gas(
            outcome.result.result,
            &mut revm::context_interface::cfg::gas::GasTracker::default(),
            gas.tracker_mut(),
        );
        self.0 = Some((gas.remaining(), gas.reservoir()));
    }
}

/// Runs a deployment of `init_code` signed for and forwarded `forward`, below the execution cap,
/// where the call holds no reservoir to share with its creation: returns the outcome and what the
/// creation returned, its regular gas and its reservoir.
fn deploy_forwarding(init_code: Bytes, forward: u64) -> (MegaTransactionOutcome, (u64, u64)) {
    deploy_forwarding_on(system_db(), init_code, forward)
}

/// [`deploy_forwarding`] over `db`.
fn deploy_forwarding_on(
    db: MemoryDatabase,
    init_code: Bytes,
    forward: u64,
) -> (MegaTransactionOutcome, (u64, u64)) {
    let deployment = Deployment::signed(0, forward, U256::ZERO, init_code);
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits()))
        .with_inspector(CreationReturn::default());
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(forward), U256::ZERO);
    tx.0.base.gas_limit = GAS_LIMITS[0];
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let returned = evm.inspector().0.expect("the creation ran");
    (outcome, returned)
}

/// A constructor that writes a fresh slot and reverts, run on no reservoir and a forward of
/// 398,560 of room on top of what the slot adds, 500,000 at the spec's prices: regular gas paid
/// the slot's state gas and its record's history, and the revert gives both back into the
/// creation's reservoir, so the creation returns its unspent regular gas and a reservoir of exactly
/// those two. `gasUsed` nets both pools: the regular gas spent less the reservoir it returned,
/// which is the constructor's regular charges alone — 12,109, at any byte price.
#[test]
fn test_a_reverting_constructor_returns_its_slot_as_reservoir_and_reports_its_regular_charges() {
    let given_back = crate::common::slot_state_gas() +
        mega_evm::write_record_history_gas(1).expect("a record has a price");
    let forward = 398_560 + given_back;
    let init_code = Bytes::from_static(&[0x60, 0x01, PUSH0, SSTORE, PUSH0, PUSH0, REVERT]);
    let (outcome, (regular, reservoir)) = deploy_forwarding(init_code, forward);
    let KeylessDeployError::ExecutionReverted { gas_used, .. } = failure(&outcome) else {
        panic!("expected ExecutionReverted: {:?}", outcome.result);
    };
    assert_eq!(gas_used, 12_109, "the constructor's regular charges");
    assert_eq!(reservoir, given_back, "the slot and its record come back as reservoir");
    assert_eq!(regular, forward - gas_used - given_back, "regular gas paid the slot");
    if mega_evm::active_satin_prices().is_constants() {
        assert_eq!(forward, 500_000);
        assert_eq!((regular, reservoir), (386_451, 101_440));
    }
}

/// A constructor that runs out of gas halts the same way, below and above the execution cap,
/// and spends its whole forward in both.
#[test]
fn test_a_constructor_out_of_gas_is_execution_halted() {
    let deployment = Deployment::new(Bytes::from_static(&[JUMPDEST, PUSH0, JUMP]));
    for gas_limit in GAS_LIMITS {
        let outcome = run_with(
            system_db(),
            deployment.call_data(SIGNED_GAS_LIMIT),
            gas_limit,
            EvmTxRuntimeLimits::no_limits(),
        );
        let KeylessDeployError::ExecutionHalted { gas_used, .. } = failure(&outcome) else {
            panic!("expected ExecutionHalted: {:?}", outcome.result);
        };
        assert_eq!(gas_used, SIGNED_GAS_LIMIT, "at {gas_limit}");
    }
}

/// A deployment whose initcode returns empty code at the deploy address is refused: the creation
/// reverts, the call answers `EmptyCodeDeployed`, and the deploy address keeps no account, so the
/// signed bytes can still be deployed. The initcode returns empty code when it has more than
/// 2,000,000 gas left and a one-byte runtime otherwise, so whoever submits the signed bytes first
/// chooses the outcome through `gasLimitOverride`: the second submission, with a forward that
/// makes the initcode deploy, finds the address free and deploys. The signer stays at nonce 1,
/// the first creation's bump from 0 being the replay barrier and the second's from 1 taken back.
#[test]
fn test_an_empty_code_deployment_leaves_the_address_deployable() {
    let gas_dependent = Bytes::from(hex!("5a621e848010600d5760015ff35b5f5ff3").to_vec());
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(gas_dependent.clone());
        let mut db = system_db();
        let first = run_nth(
            db.clone(),
            deployment.call_data(5_000_000),
            gas_limit,
            EvmTxRuntimeLimits::no_limits(),
            0,
        );
        assert!(
            matches!(failure(&first), KeylessDeployError::EmptyCodeDeployed { .. }),
            "{:?}",
            first.result
        );
        assert_eq!(nonce(&first, deployment.signer), 1, "the first bump stands");
        assert!(
            first.state.get(&deployment.address).is_none_or(|account| account.info.nonce == 0),
            "the deploy address keeps no nonce"
        );
        assert!(
            code_hash(&first, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY),
            "and no code"
        );
        db.commit(first.result_and_state.state);

        let second = run_nth(
            db,
            deployment.call_data(SIGNED_GAS_LIMIT),
            gas_limit,
            EvmTxRuntimeLimits::no_limits(),
            1,
        );
        assert_eq!(returned(&second).deployedAddress, deployment.address, "at {gas_limit}");
        assert_eq!(nonce(&second, deployment.signer), 1, "the second bump is taken back");
        assert_eq!(nonce(&second, deployment.address), 1);
    }
}

/// Init code that destroys its own account: the value it held goes to the beneficiary, no code
/// is left, and the call reports `EmptyCodeDeployed`.
#[test]
fn test_keyless_deploy_init_code_selfdestructs() {
    for gas_limit in GAS_LIMITS {
        let beneficiary = address!("0x0000000000000000000000000000000000beeef1");
        let init_code =
            BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build();
        let deployment = Deployment::with_value(init_code, U256::from(1_000));
        let outcome = deploy(db_for(&deployment, U256::from(1_000)), &deployment, gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::EmptyCodeDeployed { .. }));
        assert_eq!(
            outcome.state.get(&beneficiary).map(|account| account.info.balance),
            Some(U256::from(1_000)),
        );
        assert_eq!(nonce(&outcome, deployment.signer), 1);
    }
}

/// A constructor that destroys its own account on 500,000 of forward, no reservoir and no
/// balance: the destruction gives the account the call created for it back — its state gas and its
/// record's history — into the creation's reservoir. The creation returns all its regular gas but
/// the constructor's 5,002 and a reservoir of exactly that give-back, and `gasUsed` adds the
/// give-back of the call's own charge back to the creation's net spend, reporting the 5,002.
#[test]
fn test_a_constructor_that_destroys_itself_returns_its_account_as_reservoir() {
    const FORWARD: u64 = 500_000;
    let (outcome, (regular, reservoir)) =
        deploy_forwarding(Bytes::from_static(&[ORIGIN, SELFDESTRUCT]), FORWARD);
    let KeylessDeployError::EmptyCodeDeployed { gas_used } = failure(&outcome) else {
        panic!("expected EmptyCodeDeployed: {:?}", outcome.result);
    };
    let given_back = crate::common::account_state_gas() +
        mega_evm::write_record_history_gas(1).expect("a record has a price");
    assert_eq!(gas_used, 5_002, "ORIGIN and SELFDESTRUCT to a warm account");
    assert_eq!(regular, FORWARD - gas_used, "the constructor's regular charges alone");
    assert_eq!(reservoir, given_back, "the destroyed account comes back as reservoir");
    if mega_evm::active_satin_prices().is_constants() {
        assert_eq!((regular, reservoir), (494_998, 187_120));
    }
}

/// A constructor whose own account is destroyed in a frame that then reverts, and which returns
/// its code afterwards: the revert takes the destruction back, with what the destruction table
/// gave back, so the account stands and the deployment succeeds. `gasUsed` is what the creation
/// spent from its two pools taken together, on no reservoir its regular gas spent less the
/// reservoir it returned, with nothing added for the call's own charge for the account, which the
/// destruction gave back only while it stood.
#[test]
fn test_a_destruction_a_revert_took_back_adds_nothing_to_gas_used() {
    const FORWARD: u64 = 500_000;
    const DESTROYER: Address = address!("0x0000000000000000000000000000000000500001");
    // `HELPER`, run through a `DELEGATECALL`, has the creation's account destroyed by
    // `DESTROYER`, run the same way, then reverts.
    let helper = calling(BytecodeBuilder::default(), DESTROYER, DELEGATECALL)
        .append_many([PUSH0, PUSH0, REVERT])
        .build();
    let db = system_db()
        .account_code(HELPER, helper)
        .account_code(DESTROYER, Bytes::from_static(&[PUSH0, SELFDESTRUCT]));
    let init_code = running(calling(BytecodeBuilder::default(), HELPER, DELEGATECALL));
    let address = Deployment::signed(0, FORWARD, U256::ZERO, init_code.clone()).address;
    let (outcome, (regular, reservoir)) = deploy_forwarding_on(db, init_code, FORWARD);
    let ret = returned(&outcome);
    assert_eq!(ret.deployedAddress, address, "the destruction was taken back: the account stands");
    assert_eq!(code_hash(&outcome, address), Some(keccak(&runtime(1))));
    assert_eq!(
        ret.gasUsed + regular + reservoir,
        FORWARD,
        "the creation's spend from both pools, and nothing added"
    );
}

/// A constructor that has its own account destroyed through a `DELEGATECALL` and then returns
/// code reports `EmptyCodeDeployed`: the code the constructor returned is not code the account
/// keeps (EIP-6780), and the answer is read from the account, not from the return.
#[test]
fn test_a_constructor_destroyed_through_delegatecall_deploys_no_code() {
    for gas_limit in GAS_LIMITS {
        let helper = Bytes::from_static(&[PUSH0, SELFDESTRUCT]);
        let deployment =
            Deployment::new(running(calling(BytecodeBuilder::default(), HELPER, DELEGATECALL)));
        let db = system_db().account_code(HELPER, helper);
        let outcome = deploy(db, &deployment, gas_limit);
        let ret = returned(&outcome);
        assert_eq!(ret.deployedAddress, Address::ZERO);
        assert!(matches!(failure(&outcome), KeylessDeployError::EmptyCodeDeployed { .. }));
        assert!(outcome
            .state
            .get(&deployment.address)
            .is_some_and(|account| account.is_selfdestructed()),);
    }
}

/// Code over the contract size limit is refused by the creation (EIP-170): `ExecutionHalted`.
#[test]
fn test_code_over_the_size_limit_is_refused() {
    for gas_limit in GAS_LIMITS {
        let size = mega_evm::constants::MAX_CONTRACT_SIZE as u64 + 1;
        let init_code =
            BytecodeBuilder::default().push_number(size).push_number(0_u64).append(RETURN).build();
        let deployment = Deployment::new(init_code);
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionHalted { .. }));
        assert!(code_hash(&outcome, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY));
    }
}

/// A constructor that writes another contract's storage: the write is kept.
#[test]
fn test_keyless_deploy_modifies_other_contract_state() {
    for gas_limit in GAS_LIMITS {
        let store =
            Bytes::from_static(&[PUSH0, revm::bytecode::opcode::CALLDATALOAD, PUSH0, SSTORE]);
        let value = U256::from(0x1234_5678_u64);
        let prefix = BytecodeBuilder::default()
            .mstore(0, value.to_be_bytes::<32>())
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(32_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_address(HELPER)
            .append(GAS)
            .append(CALL)
            .append(POP);
        let deployment = Deployment::new(running(prefix));
        let outcome = deploy(system_db().account_code(HELPER, store), &deployment, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let slot = outcome.state[&HELPER].storage.get(&U256::ZERO).map(|slot| slot.present_value);
        assert_eq!(slot, Some(value));
    }
}

/// A constructor that creates a contract: the child is deployed at the new contract's first
/// creation address (its nonce starts at 1).
#[test]
fn test_keyless_deploy_creates_child_contract() {
    for gas_limit in GAS_LIMITS {
        let child_init = deploying(&[0x00]);
        let prefix = BytecodeBuilder::default()
            .mstore(0, &child_init)
            .push_number(child_init.len() as u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .append(CREATE)
            .append(POP);
        let deployment = Deployment::new(running(prefix));
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let child = deployment.address.create(1);
        assert_eq!(code_hash(&outcome, child), Some(keccak(&[0x00])));
        assert_eq!(nonce(&outcome, deployment.address), 2, "the contract created one child");
    }
}

/// A child's destruction gives back the child's leaf alone: the keyless deployment still
/// reports the code its own constructor kept.
#[test]
fn test_a_keyless_constructor_keeps_its_code_when_its_child_selfdestructs() {
    for gas_limit in GAS_LIMITS {
        let child_init = Bytes::from_static(&[ORIGIN, SELFDESTRUCT]);
        let prefix = BytecodeBuilder::default()
            .mstore(0, &child_init)
            .push_number(child_init.len() as u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .append(CREATE)
            .append(POP);
        let init_code = running(prefix);
        let init_len = init_code.len();
        let deployment = Deployment::new(init_code);

        let outcome = deploy(system_db(), &deployment, gas_limit);

        let answer = returned(&outcome);
        let [total, ..] =
            beyond(&outcome, &reference(deployment.call_data(LARGE_OVERRIDE), gas_limit));
        assert_eq!(
            total,
            KEYLESS_DEPLOY_OVERHEAD_GAS +
                create_regular(init_len) +
                entry(GasId::new_account_state_gas()) +
                entry(GasId::create_state_gas()) +
                2 * record() +
                answer.gasUsed,
            "the child's give-back belongs to the constructor at {gas_limit}"
        );
        assert_eq!(answer.deployedAddress, deployment.address, "at {gas_limit}");
        assert!(answer.errorData.is_empty(), "{:?}", answer.errorData);
        assert_eq!(code_hash(&outcome, deployment.address), Some(keccak(&[0x00])));
        assert_eq!(nonce(&outcome, deployment.address), 2);
        let child = &outcome.state[&deployment.address.create(1)];
        assert!(child.is_selfdestructed());
        assert!(child.info.code.as_ref().is_none_or(|code| code.is_empty()));
    }
}

/// A constructor that reads another contract, and keeps what it read in its own storage.
#[test]
fn test_keyless_deploy_reads_existing_contract() {
    for gas_limit in GAS_LIMITS {
        let reader =
            BytecodeBuilder::default().return_with_data(U256::from(42).to_be_bytes::<32>()).build();
        let prefix = BytecodeBuilder::default()
            .push_number(32_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_address(HELPER)
            .append(GAS)
            .append(STATICCALL)
            .append(POP)
            .push_number(0_u64)
            .append(MLOAD)
            .push_number(0_u64)
            .append(SSTORE);
        let deployment = Deployment::new(running(prefix));
        let outcome = deploy(system_db().account_code(HELPER, reader), &deployment, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let slot =
            outcome.state[&deployment.address].storage.get(&U256::ZERO).map(|s| s.present_value);
        assert_eq!(slot, Some(U256::from(42)));
    }
}

/// A constructor's log is the transaction's: it is in the receipt, emitted by the deployed
/// address.
#[test]
fn test_keyless_deploy_emits_logs() {
    for gas_limit in GAS_LIMITS {
        let prefix = BytecodeBuilder::default()
            .mstore(0, B256::repeat_byte(0xab))
            .push_number(32_u64)
            .push_number(0_u64)
            .append(LOG0);
        let deployment = Deployment::new(running(prefix));
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let logs = outcome.result.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, deployment.address);
        assert_eq!(logs[0].data.data, Bytes::from(B256::repeat_byte(0xab).to_vec()));
    }
}

/// Init code that logs and deploys nothing keeps no log: the call reports `EmptyCodeDeployed`, and
/// the creation, refused its empty deposit, reverted with its log.
#[test]
fn test_an_empty_code_deployment_keeps_no_log() {
    for gas_limit in GAS_LIMITS {
        let topic = B256::repeat_byte(0x11);
        let init_code = BytecodeBuilder::default()
            .mstore(0, [0xde, 0xad, 0xbe, 0xef])
            .push_bytes(topic)
            .push_number(4_u64)
            .push_number(0_u64)
            .append(LOG1)
            .return_empty()
            .build();
        let deployment = Deployment::new(init_code);
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::EmptyCodeDeployed { .. }));
        assert!(outcome.result.logs().is_empty());
        assert_eq!(nonce(&outcome, deployment.signer), 1);
    }
}

/// A deployment that fails keeps no log: the creation's revert takes its logs with it.
#[test]
fn test_a_failed_deployment_keeps_no_log() {
    for gas_limit in GAS_LIMITS {
        let init_code = BytecodeBuilder::default()
            .push_number(0_u64)
            .push_number(0_u64)
            .append(LOG0)
            .revert()
            .build();
        let deployment = Deployment::new(init_code);
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionReverted { .. }));
        assert!(outcome.result.logs().is_empty());
    }
}

/// The `keylessDeploy` transaction from the relayer at `gas_price`, at `gas_limit`.
fn priced_tx(deployment: &Deployment, gas_limit: u64, gas_price: u128) -> MegaTransaction {
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
    tx.0.base.gas_limit = gas_limit;
    tx.0.base.gas_price = gas_price;
    tx
}

/// `ORIGIN` and `GASPRICE` in the init code are the transaction's own: the relayer, and the gas
/// price the relayer pays — not the signer, and not the price the deployment was signed at.
#[test]
fn test_origin_and_gasprice_follow_the_outer_transaction() {
    let prefix = BytecodeBuilder::default()
        .append(ORIGIN)
        .push_number(0_u64)
        .append(SSTORE)
        .append(GASPRICE)
        .push_number(1_u64)
        .append(SSTORE);
    let deployment = Deployment::new(running(prefix));
    for gas_limit in GAS_LIMITS {
        let outcome = MegaEvm::new(context(system_db()))
            .execute_transaction(priced_tx(&deployment, gas_limit, 7))
            .expect("a valid transaction");
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let slot = |key: u64| {
            outcome.state[&deployment.address]
                .storage
                .get(&U256::from(key))
                .map(|s| s.present_value)
        };
        assert_eq!(slot(0), Some(U256::from_be_slice(RELAYER.as_slice())), "ORIGIN");
        assert_eq!(slot(1), Some(U256::from(7)), "GASPRICE");
    }
}

/// The block's beneficiary is paid for the whole transaction, deployment included, whether the
/// deployment succeeds or fails, and the signer pays nothing.
fn assert_beneficiary_paid(init_code: Bytes) {
    let deployment = Deployment::new(init_code);
    let block = BlockEnv { beneficiary: BENEFICIARY, ..block() };
    let outcome = MegaEvm::new(context(system_db()).with_block(block))
        .execute_transaction(priced_tx(&deployment, GAS_LIMITS[0], 5))
        .expect("a valid transaction");
    assert!(outcome.result.is_success());
    let used = outcome.result.gas().tx_gas_used();
    assert_eq!(
        outcome.state.get(&BENEFICIARY).map(|account| account.info.balance),
        Some(U256::from(used) * U256::from(5)),
    );
    assert_eq!(
        outcome.state.get(&deployment.signer).map(|account| account.info.balance),
        Some(U256::ZERO),
    );
}

/// The beneficiary is paid for a deployment whose creation fails, as for one that succeeds.
#[test]
fn test_beneficiary_receives_fees_on_execution_failure() {
    assert_beneficiary_paid(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
}

/// A contract's `keylessDeploy` call is not dispatched: it runs the method body, which reverts
/// with `NotIntercepted()`, and nothing is deployed.
#[test]
fn test_keyless_deploy_not_intercepted_for_inner_calls() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(deploying(&runtime(1)));
        let code =
            calls_with(CALL, KEYLESS_DEPLOY_ADDRESS, &deployment.call_data(LARGE_OVERRIDE), 0);
        let mut tx = call_tx(CONTRACT, [], U256::ZERO);
        tx.0.base.gas_limit = gas_limit;
        let outcome = MegaEvm::new(context(with_contract(code)))
            .execute_transaction(tx)
            .expect("a valid transaction");
        let output = outcome.result.output().cloned().unwrap_or_default();
        let (status, returned) = split_outcome(&output);
        assert!(!status);
        assert_eq!(returned, IKeylessDeploy::NotIntercepted::SELECTOR);
        assert!(!outcome.state.contains_key(&deployment.signer), "no rule read the signer");
        assert!(!outcome.state.contains_key(&deployment.address));
    }
}

/// A constructor that sends an Oracle hint reaches the node's oracle service: the creation runs
/// in the transaction's own environment.
#[test]
fn test_a_constructor_reaches_the_oracle_service() {
    for gas_limit in GAS_LIMITS {
        let topic = B256::repeat_byte(0x11);
        let hint = Bytes::from_static(b"a hint from a constructor");
        let data: Bytes = IOracle::sendHintCall { topic, data: hint.clone() }.abi_encode().into();
        let prefix = BytecodeBuilder::default()
            .mstore(0, &data)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(data.len() as u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_address(ORACLE_CONTRACT_ADDRESS)
            .append(GAS)
            .append(CALL)
            .append(POP);
        let deployment = Deployment::new(running(prefix));
        let envs = TestExternalEnvs::<String>::new();
        let outcome = salt_run(system_db(), envs.clone(), &deployment, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let hints = envs.recorded_hints();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].from, deployment.address);
        assert_eq!(hints[0].topic, topic);
        assert_eq!(hints[0].data, hint);
    }
}

/// What a deployment writes is the transaction's usage, counted where it is written: the
/// signer's nonce, the created account, a slot the constructor fills, the code it deposits.
#[test]
fn test_a_deployment_is_counted_where_it_writes() {
    let prefix = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1));
    let deployment = Deployment::new(running(prefix));
    for gas_limit in GAS_LIMITS {
        let outcome = deploy(system_db(), &deployment, gas_limit);
        let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
        assert_eq!(outcome.usage.write_records, 3, "at {gas_limit}");
        assert_eq!(outcome.usage.data_size - reference.usage.data_size, 3 * 40 + 1);
    }
}

/// A signer's first deployment pays for its account; a retry, from the account the first attempt
/// created, does not.
#[test]
fn test_a_retry_does_not_pay_for_the_signer_again() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
        let mut db = system_db();
        let first = run_nth(
            db.clone(),
            deployment.call_data(LARGE_OVERRIDE),
            gas_limit,
            EvmTxRuntimeLimits::no_limits(),
            0,
        );
        assert!(matches!(failure(&first), KeylessDeployError::ExecutionReverted { .. }));
        db.commit(first.result_and_state.state.clone());
        let retry = run_nth(
            db,
            deployment.call_data(LARGE_OVERRIDE),
            gas_limit,
            EvmTxRuntimeLimits::no_limits(),
            1,
        );
        assert!(matches!(failure(&retry), KeylessDeployError::ExecutionReverted { .. }));
        assert_eq!(first.gas.state - retry.gas.state, entry(GasId::new_account_state_gas()));
        assert_eq!(retry.gas.state, 0);
    }
}

/// A transaction that fails with a database error inside a keyless deployment leaves nothing
/// behind for the next: the next transaction on the same EVM runs as if the first never had.
#[test]
fn test_a_failed_keyless_transaction_leaves_nothing_for_the_next() {
    for gas_limit in GAS_LIMITS {
        let failing = address!("0x0000000000000000000000000000000000fa11ed");
        let prefix = BytecodeBuilder::default()
            .push_address(failing)
            .append(revm::bytecode::opcode::BALANCE)
            .append(POP);
        let deployment = Deployment::new(running(prefix));
        let answer = BytecodeBuilder::default().return_with_data([0x2a]).build();
        let mut db = ErrorInjectingDatabase::new(system_db().account_code(HELPER, answer));
        db.fail_on_account = Some(failing);
        let mut evm = MegaEvm::new(context(db));

        let mut tx =
            call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
        tx.0.base.gas_limit = gas_limit;
        assert!(evm.execute_transaction(tx).is_err(), "the deployment fails with the database");

        let mut next = call_tx(HELPER, [], U256::ZERO);
        next.0.base.gas_limit = gas_limit;
        let outcome = evm.execute_transaction(next).expect("the next transaction is valid");
        assert_eq!(outcome.result.output().cloned(), Some(Bytes::from_static(&[0x2a])));
        assert_eq!(outcome.usage.write_records, 0, "no record of the deployment is left over");
    }
}

/* ---------- EIP-7708 transfer logs ---------- */

/// Counts the logs an inspector is told of.
#[derive(Default)]
struct LogCounter(usize);

impl<DB: Database> revm::Inspector<MegaContext<DB>, revm::interpreter::interpreter::EthInterpreter>
    for LogCounter
{
    fn log(&mut self, _: &mut MegaContext<DB>, _: alloy_primitives::Log) {
        self.0 += 1;
    }
}

/// A deployment carrying value moves it from the signer to the created account at the creation's
/// start, and revm journals the EIP-7708 transfer log of the move, which the receipt carries and
/// an inspector is told of once. The `keylessDeploy` call itself carries none. The log is data
/// size and nothing else: next to the same deployment without value, each against a transaction
/// carrying its calldata and nothing else, it counts 160 bytes more and the same records, history
/// gas and history bytes.
#[test]
fn test_a_deployment_carrying_value_logs_the_move() {
    let with_value = Deployment::with_value(deploying(&runtime(1)), U256::from(7_777));
    let without = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let moved = deploy(db_for(&with_value, U256::from(10_000)), &with_value, gas_limit);
        assert_eq!(returned(&moved).deployedAddress, with_value.address);
        let transfer = mega_evm::test_utils::transfer_log(
            with_value.signer,
            with_value.address,
            U256::from(7_777),
        );
        assert_eq!(moved.result.logs(), [transfer], "at {gas_limit}");

        let still = deploy(db_for(&without, U256::ZERO), &without, gas_limit);
        assert!(still.result.logs().is_empty());
        let beyond_reference = |outcome: &MegaTransactionOutcome, deployment: &Deployment| {
            let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
            (
                outcome.usage.data_size - reference.usage.data_size,
                outcome.usage.write_records,
                beyond(outcome, &reference)[3..].to_vec(),
            )
        };
        let (moved_bytes, moved_records, moved_history) = beyond_reference(&moved, &with_value);
        let (still_bytes, still_records, still_history) = beyond_reference(&still, &without);
        assert_eq!(moved_bytes, still_bytes + 160, "the transfer log's bytes, at {gas_limit}");
        assert_eq!((moved_records, moved_history), (still_records, still_history));

        let mut evm = MegaEvm::new(context(db_for(&with_value, U256::from(10_000))))
            .with_inspector(LogCounter::default());
        let mut tx =
            call_tx(KEYLESS_DEPLOY_ADDRESS, with_value.call_data(LARGE_OVERRIDE), U256::ZERO);
        tx.0.base.gas_limit = gas_limit;
        let inspected = evm.execute_transaction(tx).expect("the transaction is valid");
        assert_eq!(inspected.result, moved.result, "inspected as plain");
        assert_eq!(evm.inspector().0, 1, "the inspector is told of the log once");
    }
}

/// The transfer log is counted with the creation's start: a creation whose budget holds its
/// record and not the log is stopped at its start, alone, before it runs, and its revert takes
/// back the value revm moved and the log it journaled. The same deployment without value deploys
/// under the same limit.
#[test]
fn test_a_creation_is_stopped_on_its_transfer_log_before_the_value_moves() {
    let with_value = Deployment::with_value(deploying(&runtime(1)), U256::from(7_777));
    let without = Deployment::new(deploying(&runtime(1)));
    // The creation gets 98% of the 200 bytes the call has: 196, room for the created account's
    // record and its byte of code, not for the log's 160 on top of the record. The call keeps the
    // signer's nonce record in what the creation leaves it.
    let limits = |deployment: &Deployment, gas_limit| {
        let body = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit).usage.data_size;
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(body + 200)
    };
    for gas_limit in GAS_LIMITS {
        let stopped = run_with(
            db_for(&with_value, U256::from(10_000)),
            with_value.call_data(LARGE_OVERRIDE),
            gas_limit,
            limits(&with_value, gas_limit),
        );
        let KeylessDeployError::ExecutionReverted { output, .. } = failure(&stopped) else {
            panic!("the creation was not stopped: {:?}", stopped.result);
        };
        let stop = mega_evm::MegaLimitExceeded::abi_decode(&output).expect("a limit stop");
        assert_eq!(stop.kind, mega_evm::LimitKind::DataSize.as_u8(), "at {gas_limit}");
        assert!(stopped.result.logs().is_empty(), "no log is journaled");
        let balance = |address| stopped.state.get(&address).map(|account| account.info.balance);
        assert_eq!(balance(with_value.signer), Some(U256::from(10_000)), "the value is unmoved");
        assert!(balance(with_value.address).is_none_or(|balance| balance.is_zero()));
        assert_eq!(nonce(&stopped, with_value.signer), 1, "a stopped creation spends the nonce");

        let deployed = run_with(
            db_for(&without, U256::ZERO),
            without.call_data(LARGE_OVERRIDE),
            gas_limit,
            limits(&without, gas_limit),
        );
        assert_eq!(returned(&deployed).deployedAddress, without.address, "at {gas_limit}");
    }
}

/// A constructor that returns empty code for the deploy address is refused, unless it destroyed
/// the address in this transaction, and code a constructor returns is kept unless it destroyed the
/// address in this transaction: a destruction an earlier transaction on the same journal made —
/// two transactions run one after another without the journal finalized between them, as a block
/// builder runs them — is not this one. The constructor destroys its account while a probe address
/// is empty, and returns empty code, or one byte, otherwise. The first deployment destroys it; a
/// transfer then funds the probe; the same deployment again returns code for an account it did not
/// destroy: empty code is refused, so the deploy address is not created, and one byte is deployed.
#[test]
fn test_a_destruction_an_earlier_transaction_made_is_not_the_deployments() {
    use revm::{
        bytecode::opcode::{BALANCE, ISZERO, JUMPDEST, JUMPI, SELFDESTRUCT},
        context::ContextTr,
        ExecuteEvm,
    };
    const PROBE: Address = address!("0x00000000000000000000000000000000000ba1a0");
    for returned in [0_u8, 1] {
        // `PUSH20 probe; BALANCE; ISZERO; PUSH1 destroy; JUMPI; PUSH1 returned; PUSH0; RETURN;
        // JUMPDEST` at 30, then the destruction.
        let init = BytecodeBuilder::default()
            .push_address(PROBE)
            .append_many([BALANCE, ISZERO])
            .push_number(30_u8)
            .append(JUMPI)
            .push_number(returned)
            .append_many([PUSH0, RETURN, JUMPDEST])
            .push_address(NOBODY)
            .append(SELFDESTRUCT)
            .build();
        assert_eq!(init[30], JUMPDEST);
        let deployment = Deployment::new(init);
        let mut evm = MegaEvm::new(
            context(db_for(&deployment, U256::ZERO))
                .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits()),
        );
        let deploy = |nonce| {
            let data = deployment.call_data(LARGE_OVERRIDE);
            let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, data, U256::ZERO);
            tx.0.base.nonce = nonce;
            tx
        };
        let first = ExecuteEvm::transact_one(&mut evm, deploy(0)).expect("valid");
        assert!(first.is_success(), "returning {returned}: {first:?}");
        assert!(
            evm.ctx().journal_ref().state[&deployment.address].is_selfdestructed(),
            "returning {returned}: the first deployment destroyed the account"
        );
        let mut fund = call_tx(PROBE, Bytes::new(), U256::from(1));
        fund.0.base.nonce = 1;
        assert!(ExecuteEvm::transact_one(&mut evm, fund).expect("valid").is_success());

        let second = ExecuteEvm::transact_one(&mut evm, deploy(2)).expect("valid");
        let ExecutionResult::Success { output, .. } = &second else { panic!("{second:?}") };
        let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data())
            .expect("a keylessDeploy return");
        let state = ExecuteEvm::finalize(&mut evm);
        let deployed = state.get(&deployment.address);
        if returned == 0 {
            assert_eq!(
                decode_error_result(&ret.errorData),
                Some(KeylessDeployError::EmptyCodeDeployed { gas_used: ret.gasUsed }),
                "the empty code is refused"
            );
            assert!(
                deployed.is_none_or(|account| account.info.nonce == 0),
                "the refused creation leaves the deploy address uncreated"
            );
        } else {
            assert_eq!(ret.deployedAddress, deployment.address, "the byte is deployed");
            assert!(deployed.is_some_and(|account| account.info.code_hash != KECCAK_EMPTY));
        }
    }
}
