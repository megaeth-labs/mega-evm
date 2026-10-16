//! The `keylessDeploy` call as a frame: revm builds it on the contract, as any call's, and puts it
//! on its frame stack; its actions are made by hand, so the contract's bytecode never runs for a
//! call the dispatch takes; the creation is its child, one frame above it on the stack; and an
//! inspector sees it as one call frame in which no interpreter runs.

use mega_evm::{
    alloy_consensus::{
        proofs::{state_root_unhashed, storage_root_unhashed},
        TrieAccount,
    },
    system::keyless::{KEYLESS_DEPLOY_CODE_HASH, KEYLESS_DEPLOY_OVERHEAD_GAS},
    test_utils::BytecodeBuilder,
};
use revm::{
    bytecode::opcode::{CALL, GAS, POP, STOP},
    context::{ContextTr, JournalTr},
    database::{states::bundle_state::BundleRetention, PlainAccount, State},
    handler::{EvmTr, FrameResult, ItemOrResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallInput, CallInputs,
        CallOutcome, CallScheme, CallValue, CreateScheme, FrameInput, InstructionResult,
        Interpreter, SharedMemory,
    },
    primitives::KECCAK_EMPTY,
    Database, DatabaseCommit, Inspector,
};

use super::*;
use crate::common::{context, CALLER};

/// The transaction's own frame: a `keylessDeploy` call of `data` from [`CALLER`], forwarded
/// `gas_limit`, over the contract's code as the transaction loads it.
fn call_frame(code: revm::state::Bytecode, data: Bytes, gas_limit: u64) -> FrameInit {
    FrameInit {
        depth: 0,
        memory: SharedMemory::new(),
        frame_input: FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(data),
            return_memory_offset: 0..0,
            gas_limit,
            reservoir: 0,
            bytecode_address: KEYLESS_DEPLOY_ADDRESS,
            known_bytecode: (KEYLESS_DEPLOY_CODE_HASH, code),
            target_address: KEYLESS_DEPLOY_ADDRESS,
            caller: CALLER,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: false,
        })),
    }
}

/// The call is a frame on revm's stack, driven through revm's frame lifecycle by hand: revm builds
/// it at depth 0; its first run starts the creation as its child, at depth 1, which revm builds
/// above it on the stack; the creation returns into it; and on its resume it answers in the ABI,
/// as the outermost frame.
#[test]
fn test_the_call_is_a_frame_on_revms_stack() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let mut evm = MegaEvm::new(context(system_db()));
    let journal = evm.ctx_mut().journal_mut();
    journal.load_account(CALLER).unwrap();
    let code = journal.load_account_with_code(KEYLESS_DEPLOY_ADDRESS).unwrap().info.code.clone();
    let init = call_frame(code.unwrap(), deployment.call_data(LARGE_OVERRIDE), 10_000_000);

    let ItemOrResult::Item(call) = EvmTr::frame_init(&mut evm, init).unwrap() else {
        panic!("revm builds the call's frame");
    };
    assert_eq!(call.depth, 0);
    assert_eq!(call.interpreter.input.target_address, KEYLESS_DEPLOY_ADDRESS);
    assert_eq!(EvmTr::frame_stack(&mut evm).index(), Some(0));

    let ItemOrResult::Item(creation) = EvmTr::frame_run(&mut evm).unwrap() else {
        panic!("the call's first run starts its creation");
    };
    assert_eq!(creation.depth, 1, "the creation is the call's child");
    let FrameInput::Create(inputs) = &creation.frame_input else {
        panic!("the child is a creation: {:?}", creation.frame_input);
    };
    assert_eq!(inputs.caller(), deployment.signer);
    assert_eq!(inputs.scheme(), CreateScheme::Custom { address: deployment.address });
    let ItemOrResult::Item(_) = EvmTr::frame_init(&mut evm, creation).unwrap() else {
        panic!("revm builds the creation's frame");
    };
    assert_eq!(EvmTr::frame_stack(&mut evm).index(), Some(1), "above the call on the stack");

    let ItemOrResult::Result(created) = EvmTr::frame_run(&mut evm).unwrap() else {
        panic!("the creation runs to its end");
    };
    assert!(matches!(&created, FrameResult::Create(outcome) if outcome.result.is_ok()));
    let returned = EvmTr::frame_return_result(&mut evm, created).unwrap();
    assert!(returned.is_none(), "the creation returns into the call");
    assert_eq!(EvmTr::frame_stack(&mut evm).index(), Some(0));

    let ItemOrResult::Result(answer) = EvmTr::frame_run(&mut evm).unwrap() else {
        panic!("the call answers on its resume");
    };
    let answer = EvmTr::frame_return_result(&mut evm, answer).unwrap();
    let Some(FrameResult::Call(outcome)) = answer else {
        panic!("the call is the outermost frame: {answer:?}");
    };
    assert_eq!(outcome.result.result, InstructionResult::Return);
    let ret =
        IKeylessDeploy::keylessDeployCall::abi_decode_returns(&outcome.result.output).unwrap();
    assert_eq!(ret.deployedAddress, deployment.address);
    assert!(EvmTr::frame_stack(&mut evm).index().is_none(), "no frame is left");
}

/// What an inspector was told, and at which journal depth.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Call(usize, Address),
    CallEnd(usize, InstructionResult),
    Interpreter(Address),
    Step(Address),
}

/// Records the frame starts and ends of calls, every interpreter initialized and every step.
#[derive(Default)]
struct Watcher {
    seen: Vec<Seen>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Watcher {
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _: &mut MegaContext<DB>,
    ) {
        self.seen.push(Seen::Interpreter(interp.input.target_address));
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.seen.push(Seen::Step(interp.input.target_address));
    }

    fn call(&mut self, ctx: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.seen.push(Seen::Call(ctx.journal_ref().depth(), inputs.target_address));
        None
    }

    fn call_end(&mut self, ctx: &mut MegaContext<DB>, _: &CallInputs, outcome: &mut CallOutcome) {
        self.seen.push(Seen::CallEnd(ctx.journal_ref().depth(), outcome.result.result));
    }
}

/// Runs `data` to `to` from [`CALLER`] over `db` at `gas_limit`, carrying `value`, under a
/// [`Watcher`].
fn watched(
    db: MemoryDatabase,
    to: Address,
    data: Bytes,
    value: U256,
    gas_limit: u64,
) -> (MegaTransactionOutcome, Vec<Seen>) {
    let mut tx = call_tx(to, data, value);
    tx.0.base.gas_limit = gas_limit;
    let mut evm = MegaEvm::new(context(db)).with_inspector(Watcher::default());
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    (outcome, evm.inspector().seen.clone())
}

/// An inspector sees the call as one call frame, started and ended at the transaction's journal
/// depth, in which no interpreter is initialized and no step runs: every step it sees is the
/// creation's, whose interpreter is the only one. So for a deployment, and for a call a rule
/// refuses.
#[test]
fn test_the_inspector_sees_one_call_frame_in_which_nothing_runs() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let refused = Deployment::signed(1, SIGNED_GAS_LIMIT, U256::ZERO, deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let (outcome, seen) = watched(
            system_db(),
            KEYLESS_DEPLOY_ADDRESS,
            deployment.call_data(LARGE_OVERRIDE),
            U256::ZERO,
            gas_limit,
        );
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let calls: Vec<_> =
            seen.iter().filter(|s| matches!(s, Seen::Call(..) | Seen::CallEnd(..))).collect();
        assert_eq!(
            calls,
            [&Seen::Call(0, KEYLESS_DEPLOY_ADDRESS), &Seen::CallEnd(0, InstructionResult::Return)],
            "at {gas_limit}",
        );
        assert!(seen.contains(&Seen::Interpreter(deployment.address)));
        assert!(
            seen.iter().all(|s| !matches!(s, Seen::Interpreter(a) | Seen::Step(a)
                if *a == KEYLESS_DEPLOY_ADDRESS)),
            "nothing runs in the call's frame: {seen:?}",
        );

        let (outcome, seen) = watched(
            system_db(),
            KEYLESS_DEPLOY_ADDRESS,
            refused.call_data(LARGE_OVERRIDE),
            U256::ZERO,
            gas_limit,
        );
        assert_eq!(refusal(&outcome), KeylessDeployError::NonZeroTxNonce { tx_nonce: 1 });
        assert_eq!(
            seen,
            [Seen::Call(0, KEYLESS_DEPLOY_ADDRESS), Seen::CallEnd(0, InstructionResult::Revert)],
            "at {gas_limit}",
        );
    }
}

/// Code that stores 1 at slot 0, then stops: run, it leaves a mark.
fn marking() -> Bytes {
    BytecodeBuilder::default().sstore(U256::ZERO, U256::ONE).append(STOP).build()
}

/// Whether `outcome` left the mark [`marking`] makes at the contract.
fn marked(outcome: &MegaTransactionOutcome) -> bool {
    outcome
        .state
        .get(&KEYLESS_DEPLOY_ADDRESS)
        .and_then(|account| account.storage.get(&U256::ZERO))
        .is_some_and(|slot| slot.present_value == U256::ONE)
}

/// The contract's bytecode never runs for a call the dispatch takes, whatever the bytecode: with
/// code at the address that would leave a mark, a deployment deploys and a refused call is
/// refused, neither leaving it. A call the dispatch does not take runs the bytecode as any call
/// does: another selector, and a `keylessDeploy` call a contract makes.
#[test]
fn test_the_contracts_bytecode_never_runs_for_a_dispatched_call() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let data = deployment.call_data(LARGE_OVERRIDE);
    let relayer = address!("0x0000000000000000000000000000000000c0de02");
    let relaying = BytecodeBuilder::default()
        .mstore(0, &data)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(u16::try_from(data.len()).unwrap())
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(KEYLESS_DEPLOY_ADDRESS)
        .append_many([GAS, CALL, POP, STOP])
        .build();
    let db =
        system_db().account_code(KEYLESS_DEPLOY_ADDRESS, marking()).account_code(relayer, relaying);
    for gas_limit in GAS_LIMITS {
        let (deployed, seen) =
            watched(db.clone(), KEYLESS_DEPLOY_ADDRESS, data.clone(), U256::ZERO, gas_limit);
        assert_eq!(returned(&deployed).deployedAddress, deployment.address, "at {gas_limit}");
        assert!(!marked(&deployed), "a deployment runs no bytecode at {gas_limit}");
        assert!(!seen.contains(&Seen::Interpreter(KEYLESS_DEPLOY_ADDRESS)));

        let (refused, _) =
            watched(db.clone(), KEYLESS_DEPLOY_ADDRESS, data.clone(), U256::ONE, gas_limit);
        assert_eq!(refusal(&refused), KeylessDeployError::NoEtherTransfer);
        assert!(!marked(&refused), "a refused call runs no bytecode at {gas_limit}");

        let version = Bytes::from_static(&[0x54, 0xfd, 0x4d, 0x50]);
        let (other, _) =
            watched(db.clone(), KEYLESS_DEPLOY_ADDRESS, version, U256::ZERO, gas_limit);
        assert!(marked(&other), "another selector runs the bytecode at {gas_limit}");

        let (from_a_contract, seen) =
            watched(db.clone(), relayer, Bytes::new(), U256::ZERO, gas_limit);
        assert!(marked(&from_a_contract), "a contract's call runs the bytecode at {gas_limit}");
        assert!(seen.contains(&Seen::Interpreter(KEYLESS_DEPLOY_ADDRESS)));
    }
}

/// The dispatch is by address and selector, whatever the address holds: where the state holds no
/// contract code, the call's frame is built on the contract's own, and the call deploys.
#[test]
fn test_a_call_is_dispatched_where_the_state_holds_no_contract_code() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000_000_u64));
    for gas_limit in GAS_LIMITS {
        let outcome = run_with(
            db.clone(),
            deployment.call_data(LARGE_OVERRIDE),
            gas_limit,
            EvmTxRuntimeLimits::no_limits(),
        );
        assert_eq!(returned(&outcome).deployedAddress, deployment.address, "at {gas_limit}");
    }
}

/// The state root over `accounts`, as a node computes it from the committed state.
fn state_root<'a>(accounts: impl IntoIterator<Item = (Address, &'a PlainAccount)>) -> B256 {
    state_root_unhashed(accounts.into_iter().map(|(address, account)| {
        let storage = account.storage.iter().filter(|(_, value)| !value.is_zero());
        let leaf = TrieAccount {
            nonce: account.info.nonce,
            balance: account.info.balance,
            storage_root: storage_root_unhashed(
                storage.map(|(key, value)| (B256::from(*key), *value)),
            ),
            code_hash: account.info.code_hash,
        };
        (address, leaf)
    }))
}

/// Where the state holds no contract code, the call's frame is built on an account that does not
/// exist, and the call touches it as it touches any target. A touched empty account is not
/// committed (EIP-161): once the state is applied the account is still absent, the changes the
/// block hands on hold nothing for it, and the state root is the root of the other accounts alone,
/// which a committed empty account would have moved.
#[test]
fn test_a_call_where_the_state_holds_no_contract_code_commits_nothing_for_it() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let db =
            MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000_000_u64));
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut tx =
            call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
        tx.0.base.gas_limit = gas_limit;
        let outcome = MegaEvm::new(context(&mut state))
            .execute_transaction(tx)
            .expect("the transaction is valid");
        assert_eq!(returned(&outcome).deployedAddress, deployment.address, "at {gas_limit}");
        let contract = &outcome.state[&KEYLESS_DEPLOY_ADDRESS];
        assert!(contract.is_touched(), "the call touches its target");
        assert!(contract.is_loaded_as_not_existing(), "which does not exist");

        state.commit(outcome.result_and_state.state);
        state.merge_transitions(BundleRetention::Reverts);
        assert_eq!(state.basic(KEYLESS_DEPLOY_ADDRESS).unwrap(), None, "still absent");
        let bundle = state.take_bundle();
        assert!(bundle.account(&KEYLESS_DEPLOY_ADDRESS).is_none(), "no change for the contract");
        assert!(bundle.account(&deployment.address).is_some(), "the deployment is committed");

        let accounts: Vec<_> = state.cache.trie_account().into_iter().collect();
        let root = state_root(accounts.iter().copied());
        let others =
            accounts.iter().copied().filter(|(address, _)| *address != KEYLESS_DEPLOY_ADDRESS);
        assert_eq!(root, state_root(others), "the root is the other accounts' alone");
        let empty = PlainAccount::new_empty_with_storage(Default::default());
        assert_eq!(empty.info.code_hash, KECCAK_EMPTY);
        let with_empty = accounts.iter().copied().chain([(KEYLESS_DEPLOY_ADDRESS, &empty)]);
        assert_ne!(root, state_root(with_empty), "a committed empty account moves the root");
    }
}

/// A call carrying value is refused before revm builds its frame, which would move the value:
/// its start counts nothing — no record of the contract's account, no transfer log — so a data-size
/// limit that holds the body and not the 200 bytes a value call's start counts does not stop it,
/// and the refusal is what it reports. It pays the overhead, as any call the dispatch takes does.
#[test]
fn test_a_call_carrying_value_is_refused_before_its_start_is_counted() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let data = deployment.call_data(LARGE_OVERRIDE);
    let body = reference(data.clone(), GAS_LIMITS[0]).usage.data_size;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(body + 199);
    for gas_limit in GAS_LIMITS {
        let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, data.clone(), U256::ONE);
        tx.0.base.gas_limit = gas_limit;
        let outcome = MegaEvm::new(context(system_db()).with_tx_runtime_limits(limits))
            .execute_transaction(tx)
            .expect("the transaction is valid");
        assert_eq!(refusal(&outcome), KeylessDeployError::NoEtherTransfer, "at {gas_limit}");
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(outcome.usage.data_size, body, "nothing but the body is counted");
        assert!(outcome.gas.regular > KEYLESS_DEPLOY_OVERHEAD_GAS, "the overhead is paid");
    }
}

/// revm builds the call's frame on the contract as its target, as any call's: the contract's
/// account, which the transaction loads with its code to start its first frame, is touched by a
/// call that returns, and not by one that reverts, whose journal checkpoint takes the touch back.
#[test]
fn test_the_call_touches_the_contract_as_any_call_does() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let refused = Deployment::signed(1, SIGNED_GAS_LIMIT, U256::ZERO, deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let contract = |outcome: &MegaTransactionOutcome| {
            outcome.state.get(&KEYLESS_DEPLOY_ADDRESS).cloned().expect("the target is loaded")
        };
        let deployed = deploy(system_db(), &deployment, gas_limit);
        assert!(contract(&deployed).info.code.is_some(), "loaded with its code");
        assert!(contract(&deployed).is_touched(), "touched by a call that returns");
        let reverted = deploy(system_db(), &refused, gas_limit);
        assert!(contract(&reverted).info.code.is_some(), "loaded with its code");
        assert!(!contract(&reverted).is_touched(), "untouched by a call that reverts");
    }
}
