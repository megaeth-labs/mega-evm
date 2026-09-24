//! What a keyless deployment deploys and reports: the native creation runs the init code as any
//! creation does, and the call answers with what it did.

use alloy_primitives::{address, B256};
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
        CALL, CREATE, DELEGATECALL, GAS, GASPRICE, INVALID, JUMP, JUMPDEST, LOG0, LOG1, MLOAD,
        ORIGIN, POP, PUSH0, REVERT, SELFDESTRUCT, SSTORE, STATICCALL,
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

/// A deployment lands at the signer's first creation address, whoever the signer is, and the
/// deployed code is the runtime the init code returned.
#[test]
fn test_the_deploy_address_is_the_signers_first_creation_address() {
    for gas_limit in GAS_LIMITS {
        for len in [1, 2, 3] {
            let deployment = Deployment::new(deploying(&runtime(len)));
            let outcome = deploy(system_db(), &deployment, gas_limit);
            assert_eq!(returned(&outcome).deployedAddress, deployment.signer.create(0));
            assert_eq!(code_hash(&outcome, deployment.address), Some(keccak(&runtime(len))));
        }
    }
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

/// A successful deployment spends the signer's nonce, from 0 to 1.
#[test]
fn test_keyless_deploy_increments_nonce_from_zero() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(deploying(&runtime(1)));
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        assert_eq!(nonce(&outcome, deployment.signer), 1);
    }
}

/// A signer at nonce 1 still deploys at its Nick's-Method address, which is its first creation's
/// and not its nonce's, and ends at nonce 2: the creation bumps the real nonce.
#[test]
fn test_a_signer_at_nonce_one_deploys_at_the_same_address() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(deploying(&runtime(1)));
        let db = system_db().account_nonce(deployment.signer, 1);
        let outcome = deploy(db, &deployment, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        assert_eq!(nonce(&outcome, deployment.signer), 2);
    }
}

/// Init code that reverts: the call succeeds and reports `ExecutionReverted` with the revert
/// data, the deployment writes no code, and the signer's nonce is spent.
#[test]
fn test_keyless_deploy_execution_reverted() {
    let deployment =
        Deployment::new(BytecodeBuilder::default().revert_with_data([0xc0, 0xff, 0xee]).build());
    for gas_limit in GAS_LIMITS {
        let outcome = deploy(system_db(), &deployment, gas_limit);
        let KeylessDeployError::ExecutionReverted { gas_used, output } = failure(&outcome) else {
            panic!("expected ExecutionReverted: {:?}", outcome.result);
        };
        assert_eq!(output, Bytes::from_static(&[0xc0, 0xff, 0xee]));
        assert_eq!(gas_used, returned(&outcome).gasUsed);
        assert_eq!(nonce(&outcome, deployment.signer), 1);
        assert!(code_hash(&outcome, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY));
    }
}

/// Init code that hits an invalid opcode: `ExecutionHalted`, and the halt spends the whole
/// forward.
#[test]
fn test_keyless_deploy_execution_halted_invalid_opcode() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(Bytes::from_static(&[INVALID]));
        let outcome = run_with(
            system_db(),
            deployment.call_data(2_000_000),
            gas_limit,
            EvmTxRuntimeLimits::no_limits(),
        );
        let KeylessDeployError::ExecutionHalted { gas_used, .. } = failure(&outcome) else {
            panic!("expected ExecutionHalted: {:?}", outcome.result);
        };
        assert_eq!(gas_used, 2_000_000, "a halt spends what it was forwarded");
        assert_eq!(nonce(&outcome, deployment.signer), 1);
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

/// A halted deployment is paid for, and its nonce is spent: the transaction spends the overhead,
/// the `CREATE` opcode's regular gas, the signer's account and its nonce record, and the whole
/// forward.
#[test]
fn test_a_halted_deployment_is_paid_for_and_spends_the_nonce() {
    let deployment = Deployment::new(Bytes::from_static(&[INVALID]));
    for gas_limit in GAS_LIMITS {
        let data = deployment.call_data(SIGNED_GAS_LIMIT);
        let outcome =
            run_with(system_db(), data.clone(), gas_limit, EvmTxRuntimeLimits::no_limits());
        assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionHalted { .. }));
        assert_eq!(nonce(&outcome, deployment.signer), 1);
        let [total, ..] = beyond(&outcome, &reference(data, gas_limit));
        let new_account = entry(GasId::new_account_state_gas());
        assert_eq!(
            total,
            KEYLESS_DEPLOY_OVERHEAD_GAS +
                create_regular(1) +
                new_account +
                record() +
                SIGNED_GAS_LIMIT,
            "at {gas_limit}",
        );
    }
}

/// Init code that returns nothing deploys no code: `EmptyCodeDeployed`, and the nonce is spent.
#[test]
fn test_keyless_deploy_empty_initcode() {
    for gas_limit in GAS_LIMITS {
        for init_code in [Bytes::new(), Bytes::from_static(&[PUSH0, PUSH0, RETURN])] {
            let deployment = Deployment::new(init_code);
            let outcome = deploy(system_db(), &deployment, gas_limit);
            let KeylessDeployError::EmptyCodeDeployed { gas_used } = failure(&outcome) else {
                panic!("expected EmptyCodeDeployed: {:?}", outcome.result);
            };
            assert_eq!(gas_used, returned(&outcome).gasUsed);
            assert_eq!(nonce(&outcome, deployment.signer), 1);
        }
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

/// Code starting with `0xEF` is refused by the creation (EIP-3541): `ExecutionHalted`.
#[test]
fn test_code_starting_with_ef_is_refused() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(deploying(&[0xef, 0x00]));
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionHalted { .. }));
        assert!(code_hash(&outcome, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY));
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

/// A deployment carrying a value moves it from the signer to the contract.
#[test]
fn test_keyless_deploy_with_value_transfer() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::with_value(deploying(&runtime(1)), U256::from(7_777));
        let outcome = deploy(db_for(&deployment, U256::from(10_000)), &deployment, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let balance = |address| outcome.state.get(&address).map(|account| account.info.balance);
        assert_eq!(balance(deployment.address), Some(U256::from(7_777)));
        assert_eq!(balance(deployment.signer), Some(U256::from(2_223)));
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

/// A constructor that writes storage and then reverts leaves neither the storage nor code, and
/// the call still reports the revert with a gas figure.
#[test]
fn test_failed_initcode_keeps_no_storage() {
    let init_code =
        BytecodeBuilder::default().sstore(U256::from(1), U256::from(7)).revert().build();
    let deployment = Deployment::new(init_code);
    for gas_limit in GAS_LIMITS {
        let outcome = deploy(system_db(), &deployment, gas_limit);
        let KeylessDeployError::ExecutionReverted { gas_used, .. } = failure(&outcome) else {
            panic!("expected ExecutionReverted: {:?}", outcome.result);
        };
        assert!(gas_used > 0);
        let kept = outcome
            .state
            .get(&deployment.address)
            .and_then(|account| account.storage.get(&U256::from(1)).map(|slot| slot.present_value));
        assert!(kept.is_none_or(|value| value.is_zero()), "at {gas_limit}");
        assert!(code_hash(&outcome, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY));
        assert_eq!(outcome.gas.state, entry(GasId::new_account_state_gas()));
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

/// Init code that logs and deploys nothing keeps its log: the call reports `EmptyCodeDeployed`
/// and the receipt carries the log, because the creation succeeded — it just left no code.
#[test]
fn test_an_empty_code_deployment_keeps_its_logs() {
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
        let logs = outcome.result.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].topics(), [topic]);
        assert_eq!(logs[0].data.data, Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(nonce(&outcome, deployment.signer), 1);
    }
}

/// And init code that logs nothing leaves none.
#[test]
fn test_an_empty_code_deployment_adds_no_log() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(BytecodeBuilder::default().return_empty().build());
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::EmptyCodeDeployed { .. }));
        assert!(outcome.result.logs().is_empty());
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

/// The beneficiary is paid for a deployment that succeeds.
#[test]
fn test_beneficiary_receives_fees_on_success() {
    assert_beneficiary_paid(deploying(&runtime(1)));
}

/// And for one that fails.
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

/// A `keylessDeploy` call made inside a keyless deployment is a contract's call like any other:
/// not dispatched. The outer deployment deploys; the inner one does not.
#[test]
fn test_keyless_deploy_nested_inside_a_keyless_deployment_is_not_intercepted() {
    for gas_limit in GAS_LIMITS {
        let inner = Deployment::new(deploying(&[0x00, 0x00]));
        let data = inner.call_data(LARGE_OVERRIDE);
        let prefix = BytecodeBuilder::default()
            .mstore(0, &data)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(data.len() as u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_address(KEYLESS_DEPLOY_ADDRESS)
            .append(GAS)
            .append(CALL)
            .push_number(0_u64)
            .append(SSTORE);
        let outer = Deployment::new(running(prefix));
        let outcome = deploy(system_db(), &outer, gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, outer.address);
        let status =
            outcome.state[&outer.address].storage.get(&U256::ZERO).map(|slot| slot.present_value);
        assert_eq!(status, Some(U256::ZERO), "the inner call failed");
        assert!(code_hash(&outcome, inner.address).is_none_or(|hash| hash == KECCAK_EMPTY));
        assert_eq!(nonce(&outcome, inner.signer), 0);
    }
}

/// A constructor's storage writes are priced by the SALT environment the transaction runs in: a
/// crowded bucket at the deploy address makes the deposited code cost more state gas, and the
/// creation's `gasUsed` grows by exactly that.
#[test]
fn test_a_deployment_is_priced_by_the_transactions_salt_env() {
    let deployment = Deployment::new(deploying(&runtime(5)));
    let deposit = satin_gas_params().code_deposit_state_gas(5);
    for gas_limit in GAS_LIMITS {
        let minimal = salt_run(system_db(), TestExternalEnvs::new(), &deployment, gas_limit);
        let envs = crowded(TestExternalEnvs::new(), deployment.address, 3);
        let crowded_run = salt_run(system_db(), envs, &deployment, gas_limit);
        assert_eq!(
            returned(&crowded_run).gasUsed - returned(&minimal).gasUsed,
            2 * deposit,
            "at {gas_limit}",
        );
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

/// A deployment that fails at run time spends the signer's nonce and pays for its account, as a
/// successful one does.
#[test]
fn test_a_deployment_that_fails_still_spends_the_nonce_and_pays_for_the_signer() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
        let outcome = deploy(system_db(), &deployment, gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionReverted { .. }));
        assert_eq!(nonce(&outcome, deployment.signer), 1);
        assert_eq!(outcome.gas.state, entry(GasId::new_account_state_gas()));
    }
}

/// A successful deployment spends the nonce too, and pays for the signer's account, the
/// contract's account and its code.
#[test]
fn test_a_deployment_that_succeeds_spends_the_nonce_and_pays_for_three_accounts() {
    for gas_limit in GAS_LIMITS {
        let data = keyless_deploy_call(CREATE2_FACTORY_TX, U256::from(LARGE_OVERRIDE));
        let outcome = run_with(system_db(), data, gas_limit, EvmTxRuntimeLimits::no_limits());
        assert_eq!(returned(&outcome).deployedAddress, CREATE2_FACTORY_CONTRACT);
        assert_eq!(nonce(&outcome, CREATE2_FACTORY_DEPLOYER), 1);
        let code_len = outcome.state[&CREATE2_FACTORY_CONTRACT]
            .info
            .code
            .as_ref()
            .map_or(0, |code| code.original_bytes().len());
        assert_eq!(
            outcome.gas.state,
            entry(GasId::new_account_state_gas()) +
                entry(GasId::create_state_gas()) +
                satin_gas_params().code_deposit_state_gas(code_len),
        );
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
