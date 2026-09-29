//! Gas detention: a read of volatile data caps the compute the transaction may still spend.
//!
//! The cap is relative: at the read, the transaction's compute — its regular gas, without state
//! and history gas that spilled onto it — may grow by at most the cap. Crossing it stops the
//! transaction with the revert-class stop every transaction-level limit uses. The limit a read
//! sets is its compute then plus the cap, so a transaction that stops has spent its intrinsic gas
//! and its compute up to the charge that would have crossed the limit on the regular ledger: that
//! charge is not made, and the frame keeps what it had before it, the spendable part and the part
//! detention withheld. Where a stop lands follows from the charges a program makes after its read
//! ([`Charges`]).
//!
//! Every case runs below the execution cap, without a reservoir, and above it, with one.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    constants::{BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS, TX_GAS_LIMIT_CAP},
    system::{IMegaLimitControl, LIMIT_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    volatile_data_access_disabled_revert_data, write_record_history_gas, EvmTxRuntimeLimits,
    LimitCheck, LimitKind, MegaContext, MegaEvm, MegaLimitExceeded, MegaSpecId, MegaTransaction,
    MegaTransactionOutcome, ProtocolLimits, VolatileDataAccess,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ExecutionResult, BlockEnv, TxEnv},
    context_interface::{block::BlobExcessGasAndPrice, cfg::GasId},
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, InstructionResult, Interpreter,
    },
    Database, Inspector,
};

pub(crate) const CALLER: Address = address!("0000000000000000000000000000000000d00000");
pub(crate) const CONTRACT: Address = address!("0000000000000000000000000000000000d00001");
pub(crate) const CHILD: Address = address!("0000000000000000000000000000000000d00002");
pub(crate) const DELEGATOR: Address = address!("0000000000000000000000000000000000d00003");
pub(crate) const BENEFICIARY: Address = address!("0000000000000000000000000000000000bef000");

pub(crate) const CAP: u64 = BLOCK_ENV_ACCESS_COMPUTE_GAS;

/// Below the execution cap: no reservoir.
pub(crate) const BELOW: u64 = 100_000_000;
/// Above it: a reservoir of 100,000,000.
pub(crate) const ABOVE: u64 = TX_GAS_LIMIT_CAP + 100_000_000;
pub(crate) const TIERS: [u64; 2] = [BELOW, ABOVE];

/// The compute of a write to a fresh, cold slot as [`BytecodeBuilder::sstore`] makes it: its two
/// pushes and `SSTORE`'s 22,100.
pub(crate) const FRESH_WRITE: u64 = 3 + 3 + 22_100;

/// What a write to a fresh slot costs beside its compute: the slot's state gas and its record's
/// history, at the prices the schedule was built with.
pub(crate) fn fresh_write_spill() -> u64 {
    crate::salt::entry(GasId::sstore_set_state_gas()) +
        write_record_history_gas(1).expect("a record has a price")
}

/// Rounds of [`work`] spent before a read: 5,283,600 of compute.
pub(crate) const WORK: u32 = 1_700;

/// The compute one round of [`work`] spends once its memory is expanded.
pub(crate) const WORK_ROUND: u64 = 3_108;

/// Appends a copy of 32 KiB within memory: 3,075 gas, and 5,120 more to expand the memory the
/// first time. A round of it costs the interpreter little, whatever it costs in gas.
pub(crate) fn copy(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(0x8000_u16).append_many([PUSH0, PUSH0, MCOPY])
}

pub(crate) fn block() -> BlockEnv {
    BlockEnv {
        number: U256::from(300),
        beneficiary: BENEFICIARY,
        timestamp: U256::from(1_700_000_000),
        gas_limit: 10_000_000_000,
        basefee: 0,
        prevrandao: Some(B256::repeat_byte(7)),
        blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
            excess_blob_gas: 0,
            blob_gasprice: 3,
        }),
        slot_num: 9,
        ..Default::default()
    }
}

pub(crate) fn context<DB: Database>(db: DB) -> MegaContext<DB> {
    MegaContext::new(db, MegaSpecId::SATIN).with_block(block()).with_chain(zero_fee_l1_block_info())
}

pub(crate) fn tx(caller: Address, to: Address, gas_limit: u64) -> MegaTransaction {
    OpTx(op_transaction(TxEnv { caller, kind: TxKind::Call(to), gas_limit, ..Default::default() }))
}

/// What a transaction did, and what detention made of it.
pub(crate) struct Run {
    pub(crate) outcome: MegaTransactionOutcome,
    pub(crate) limit: Option<u64>,
    pub(crate) accessed: VolatileDataAccess,
    pub(crate) detains: bool,
}

pub(crate) fn run_on<INSP>(evm: &mut MegaEvm<MemoryDatabase, INSP>, tx: MegaTransaction) -> Run
where
    INSP: Inspector<MegaContext<MemoryDatabase>, EthInterpreter>,
{
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let detention = evm.ctx().detention();
    Run {
        outcome,
        limit: detention.compute_limit(),
        accessed: detention.accessed(),
        detains: detention.detains(),
    }
}

pub(crate) fn execute(db: MemoryDatabase, tx: MegaTransaction) -> Run {
    run_on(&mut MegaEvm::new(context(db)), tx)
}

/// The regular gas a call from `CALLER` spends before its first instruction.
pub(crate) fn intrinsic(gas_limit: u64) -> u64 {
    let db =
        MemoryDatabase::default().account_code(CONTRACT, BytecodeBuilder::default().stop().build());
    execute(db, tx(CALLER, CONTRACT, gas_limit)).outcome.gas.regular
}

/// Appends a loop that never ends, 3,094 gas a round.
pub(crate) fn spin(code: BytecodeBuilder) -> Bytes {
    let dest = code.len() as u32;
    copy(code.append(JUMPDEST)).push_number(dest).append(JUMP).build()
}

/// Appends `rounds` rounds of a counting loop that also copies memory, [`WORK_ROUND`] gas a
/// round.
pub(crate) fn work(code: BytecodeBuilder, rounds: u32) -> BytecodeBuilder {
    let code = code.push_number(rounds);
    let dest = code.len() as u32;
    copy(code.append(JUMPDEST))
        .push_number(1_u8)
        .append_many([SWAP1, SUB, DUP1])
        .push_number(dest)
        .append_many([JUMPI, POP])
}

/// Appends `rounds` rounds of a counting loop, twenty-six gas a round.
pub(crate) fn burn(code: BytecodeBuilder, rounds: u32) -> BytecodeBuilder {
    let code = code.push_number(rounds);
    let dest = code.len() as u32;
    code.append(JUMPDEST)
        .push_number(1_u8)
        .append_many([SWAP1, SUB, DUP1])
        .push_number(dest)
        .append_many([JUMPI, POP])
}

/// Appends a call of `scheme` to `to` forwarding all gas, dropping its status.
pub(crate) fn call(code: BytecodeBuilder, scheme: u8, to: Address) -> BytecodeBuilder {
    let code = code.append_many([PUSH0, PUSH0, PUSH0, PUSH0]);
    let code = if matches!(scheme, CALL | CALLCODE) { code.append(PUSH0) } else { code };
    code.push_address(to).append(GAS).append(scheme).append(POP)
}

/// The revert data of the detention stop at `limit`.
pub(crate) fn stop_data(limit: u64) -> Bytes {
    MegaLimitExceeded { kind: LimitKind::ComputeGas.as_u8(), limit }.abi_encode().into()
}

/// Asserts the transaction was stopped by detention, having computed up to the charge that
/// crossed its limit and not one unit past it: the stopped frame keeps what it had before that
/// charge, `left` short of the limit, so the transaction's compute, which the stop reports and its
/// regular ledger bills, is the limit less `left`. Returns the limit.
pub(crate) fn assert_stopped(run: &Run, intrinsic: u64, left: u64) -> u64 {
    let limit = run.limit.expect("a read set a limit");
    match &run.outcome.result {
        ExecutionResult::Revert { output, .. } => assert_eq!(output, &stop_data(limit)),
        other => panic!("expected the detention stop, got {other:?}"),
    }
    let used = limit - left;
    assert_eq!(
        run.outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::ComputeGas,
            limit,
            used,
            frame_local: false
        })
    );
    assert_eq!(
        run.outcome.gas.regular,
        intrinsic + used,
        "the transaction computed up to the charge that crossed its limit and not one unit past it"
    );
    assert_eq!(run.outcome.gas.state, 0, "a stop keeps no state");
    assert!(run.outcome.result.logs().is_empty(), "a stop keeps no log");
    limit
}

/* ---------- where a stop lands ---------- */

/// The regular charges a program makes after its read, in the order the interpreter makes them:
/// each instruction's static gas, then its dynamic charges. Each part is a run of charges made
/// some number of times, or forever.
///
/// The stop lands on the first charge the limit leaves no room for: that charge is not made, and
/// the stopped frame keeps what it had before it ([`left`](Self::left)).
#[derive(Clone, Debug, Default)]
pub(crate) struct Charges(Vec<(Vec<u64>, Option<u64>)>);

impl Charges {
    /// `charges`, once.
    pub(crate) fn then(self, charges: &[u64]) -> Self {
        self.repeat(charges, 1)
    }

    /// `charges`, `times` times over.
    pub(crate) fn repeat(mut self, charges: &[u64], times: u64) -> Self {
        self.0.push((charges.to_vec(), Some(times)));
        self
    }

    /// `charges`, over and over.
    pub(crate) fn forever(mut self, charges: &[u64]) -> Self {
        self.0.push((charges.to_vec(), None));
        self
    }

    /// What the charges add up to, for a program that ends.
    pub(crate) fn total(&self) -> u64 {
        self.0
            .iter()
            .map(|(charges, times)| {
                charges.iter().sum::<u64>() * times.expect("a program that ends")
            })
            .sum()
    }

    /// What `allowance` has left when the first charge it cannot pay comes: what a frame the limit
    /// left `allowance` at the read has when it crosses the limit.
    pub(crate) fn left(&self, mut allowance: u64) -> u64 {
        for (charges, times) in &self.0 {
            let round: u64 = charges.iter().sum();
            if round == 0 {
                continue;
            }
            let rounds = allowance / round;
            if times.is_some_and(|times| rounds >= times) {
                allowance -= round * times.unwrap();
                continue;
            }
            allowance -= round * rounds;
            for charge in charges {
                if *charge > allowance {
                    return allowance;
                }
                allowance -= charge;
            }
            unreachable!("a round the allowance cannot pay whole has a charge it cannot pay");
        }
        panic!("the program ends within the allowance")
    }

    /// The charges of [`spin`]'s loop in a frame whose memory holds `words` words: `JUMPDEST`, the
    /// copy — a push, two `PUSH0`, and `MCOPY`'s static gas, then its copy, then the memory it
    /// expands to the 32 KiB it copies ([`expansion`]) — then the push of the loop's start and
    /// `JUMP`.
    pub(crate) fn spin(self, words: u64) -> Self {
        self.then(&[1, 3, 2, 2, 3, 3_072, expansion(words), 3, 8])
            .forever(&[1, 3, 2, 2, 3, 3_072, 3, 8])
    }

    /// The charges of [`work`]'s `rounds` rounds in a frame whose memory holds `words` words: the
    /// push of the counter, then each round — `JUMPDEST`, the copy as [`spin`](Self::spin) makes
    /// it, `PUSH1`, `SWAP1`, `SUB`, `DUP1`, the push of the loop's start and `JUMPI` — then the
    /// `POP` of the counter.
    pub(crate) fn work(self, rounds: u32, words: u64) -> Self {
        let round = [1, 3, 2, 2, 3, 3_072, 3, 3, 3, 3, 3, 10];
        let charges = self.then(&[3]);
        let charges = match rounds {
            0 => charges,
            rounds => charges
                .then(&[1, 3, 2, 2, 3, 3_072, expansion(words), 3, 3, 3, 3, 3, 10])
                .repeat(&round, u64::from(rounds) - 1),
        };
        charges.then(&[2])
    }

    /// The charges of [`burn`]'s `rounds` rounds: the push of the counter, then each round —
    /// `JUMPDEST`, `PUSH1`, `SWAP1`, `SUB`, `DUP1`, the push of the loop's start and `JUMPI` —
    /// then the `POP` of the counter.
    pub(crate) fn burn(self, rounds: u32) -> Self {
        self.then(&[3]).repeat(&[1, 3, 3, 3, 3, 3, 10], u64::from(rounds)).then(&[2])
    }

    /// The charges of `count` writes of a fresh, cold slot, each pushing its value and its slot:
    /// two pushes, then `SSTORE`'s static gas and its dynamic gas, the cold access and the set.
    /// Their state and history gas are not compute.
    pub(crate) fn fresh_writes(self, count: u64) -> Self {
        self.repeat(&[3, 3, 100, 22_000], count)
    }
}

/// What a frame's memory of `words` words costs: `3 w + w² / 512`.
pub(crate) const fn memory_cost(words: u64) -> u64 {
    3 * words + words * words / 512
}

/// What expanding a frame's memory from `words` words to the 1,024 words of [`copy`] costs.
pub(crate) const fn expansion(words: u64) -> u64 {
    if words >= 1_024 {
        0
    } else {
        memory_cost(1_024) - memory_cost(words)
    }
}

/* ---------- every read ---------- */

/// A piece of code appended to a program.
type Append = fn(BytecodeBuilder) -> BytecodeBuilder;

/// One volatile read: the code that makes it, what else the database needs, the kind it records,
/// and the regular charges its code makes after the read is committed.
struct Read {
    name: &'static str,
    code: Append,
    db: fn(MemoryDatabase) -> MemoryDatabase,
    access: VolatileDataAccess,
    after: &'static [u64],
}

fn no_setup(db: MemoryDatabase) -> MemoryDatabase {
    db
}

pub(crate) fn op(code: BytecodeBuilder, opcode: u8) -> BytecodeBuilder {
    code.append(opcode).append(POP)
}

pub(crate) fn on_beneficiary(code: BytecodeBuilder, opcode: u8) -> BytecodeBuilder {
    code.push_address(BENEFICIARY).append(opcode).append(POP)
}

/// Every opcode read of volatile data.
fn reads() -> Vec<Read> {
    vec![
        Read {
            name: "NUMBER",
            code: |c| op(c, NUMBER),
            db: no_setup,
            access: VolatileDataAccess::BLOCK_NUMBER,
            after: &[2],
        },
        Read {
            name: "TIMESTAMP",
            code: |c| op(c, TIMESTAMP),
            db: no_setup,
            access: VolatileDataAccess::TIMESTAMP,
            after: &[2],
        },
        Read {
            name: "COINBASE",
            code: |c| op(c, COINBASE),
            db: no_setup,
            access: VolatileDataAccess::COINBASE,
            after: &[2],
        },
        Read {
            name: "PREVRANDAO",
            code: |c| op(c, DIFFICULTY),
            db: no_setup,
            access: VolatileDataAccess::PREV_RANDAO,
            after: &[2],
        },
        Read {
            name: "GASLIMIT",
            code: |c| op(c, GASLIMIT),
            db: no_setup,
            access: VolatileDataAccess::GAS_LIMIT,
            after: &[2],
        },
        Read {
            name: "BASEFEE",
            code: |c| op(c, BASEFEE),
            db: no_setup,
            access: VolatileDataAccess::BASE_FEE,
            after: &[2],
        },
        Read {
            name: "BLOBBASEFEE",
            code: |c| op(c, BLOBBASEFEE),
            db: no_setup,
            access: VolatileDataAccess::BLOB_BASE_FEE,
            after: &[2],
        },
        Read {
            name: "SLOTNUM",
            code: |c| op(c, SLOTNUM),
            db: no_setup,
            access: VolatileDataAccess::SLOT_NUM,
            after: &[2],
        },
        Read {
            name: "BLOCKHASH",
            code: |c| c.push_number(299_u16).append(BLOCKHASH).append(POP),
            db: no_setup,
            access: VolatileDataAccess::BLOCK_NUMBER | VolatileDataAccess::BLOCK_HASH,
            after: &[2],
        },
        Read {
            name: "BALANCE",
            code: |c| on_beneficiary(c, BALANCE),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            after: &[2],
        },
        Read {
            name: "EXTCODESIZE",
            code: |c| on_beneficiary(c, EXTCODESIZE),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            after: &[2],
        },
        Read {
            name: "EXTCODEHASH",
            code: |c| on_beneficiary(c, EXTCODEHASH),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            after: &[2],
        },
        Read {
            name: "EXTCODECOPY",
            code: |c| {
                c.push_number(32_u8)
                    .append_many([PUSH0, PUSH0])
                    .push_address(BENEFICIARY)
                    .append(EXTCODECOPY)
            },
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            // The copy leaves nothing on the stack.
            after: &[],
        },
        Read {
            name: "CALL",
            code: |c| call(c, CALL, BENEFICIARY),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            after: &[2],
        },
        Read {
            name: "CALLCODE",
            code: |c| call(c, CALLCODE, BENEFICIARY),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            after: &[2],
        },
        Read {
            name: "DELEGATECALL",
            code: |c| call(c, DELEGATECALL, BENEFICIARY),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            after: &[2],
        },
        Read {
            name: "STATICCALL",
            code: |c| call(c, STATICCALL, BENEFICIARY),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            after: &[2],
        },
        Read {
            name: "CALL to an EIP-7702 delegator of the beneficiary",
            code: |c| call(c, CALL, DELEGATOR),
            db: |db| with_delegation(db, DELEGATOR, BENEFICIARY),
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            after: &[2],
        },
        Read {
            name: "SELFDESTRUCT to the beneficiary",
            code: |c| call(c, CALL, CHILD),
            db: |db| {
                db.account_code(
                    CHILD,
                    BytecodeBuilder::default()
                        .push_address(BENEFICIARY)
                        .append(SELFDESTRUCT)
                        .build(),
                )
            },
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
            after: &[2],
        },
        Read {
            name: "SLOAD of the Oracle's storage",
            code: |c| call(c, CALL, ORACLE_CONTRACT_ADDRESS),
            db: |db| {
                db.account_code(
                    ORACLE_CONTRACT_ADDRESS,
                    BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build(),
                )
            },
            access: VolatileDataAccess::ORACLE,
            // The Oracle's `POP`, then its caller's.
            after: &[2, 2],
        },
    ]
}

/// Gives `address` the `0xef0100 || to` designator an applied EIP-7702 authorization leaves.
pub(crate) fn with_delegation(
    mut db: MemoryDatabase,
    address: Address,
    to: Address,
) -> MemoryDatabase {
    use revm::{database::AccountState, state::Bytecode};
    let bytecode = Bytecode::new_eip7702(to);
    let code_hash = bytecode.hash_slow();
    let account = db.load_account(address).expect("the account is in memory");
    account.info.code = Some(bytecode);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
    db
}

/// Every read caps the transaction from where it read, not from its start: the transaction first
/// spends more compute than the cap, then reads, then computes forever, and it stops at the first
/// charge past its compute at the read plus the cap, having spent everything before it.
#[test]
fn test_every_volatile_read_caps_the_transaction_from_where_it_read() {
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        for read in reads() {
            let code = spin((read.code)(work(BytecodeBuilder::default(), WORK)));
            let db = (read.db)(MemoryDatabase::default().account_code(CONTRACT, code));
            let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
            // The work expanded the memory the loop copies.
            let left = Charges::default().then(read.after).spin(1_024).left(CAP);
            let limit = assert_stopped(&run, intrinsic, left);
            let at_read = limit - CAP;
            assert!(
                at_read > u64::from(WORK) * WORK_ROUND &&
                    at_read < u64::from(WORK) * WORK_ROUND + 20_000,
                "{}: the cap counts from the read, which came after {at_read} of compute",
                read.name
            );
            assert_eq!(run.accessed, read.access, "{}", read.name);
        }
    }
}

/// The cap is relative: a transaction that spent more than the cap before it read may still
/// spend the cap after, and one that stops soon after the read completes.
#[test]
fn test_the_cap_counts_from_a_spend_larger_than_itself() {
    let before = 7_000_u32;
    for gas_limit in TIERS {
        let code = spin(op(work(BytecodeBuilder::default(), before), TIMESTAMP));
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        let left = Charges::default().then(&[2]).spin(1_024).left(CAP);
        let limit = assert_stopped(&run, intrinsic(gas_limit), left);
        assert!(limit - CAP > u64::from(before) * WORK_ROUND, "{limit}");
        assert!(limit - CAP > CAP);

        let code = op(work(BytecodeBuilder::default(), before), TIMESTAMP).stop().build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert_eq!(run.limit, Some(limit), "the same read at the same compute");
    }
}

/// Without the read, the same computation is not capped: it runs until the frame's own gas is
/// gone, and halts.
#[test]
fn test_without_a_read_nothing_is_capped() {
    for gas_limit in TIERS {
        let code = spin(work(BytecodeBuilder::default(), WORK));
        let db = MemoryDatabase::default().account_code(CONTRACT, code);
        let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
        assert!(
            matches!(
                run.outcome.result,
                ExecutionResult::Halt { reason: mega_evm::MegaHaltReason::Base(_), .. }
            ),
            "{:?}",
            run.outcome.result
        );
        assert_eq!(run.limit, None);
        assert_eq!(run.outcome.limit_exceeded, None);
        assert_eq!(run.accessed, VolatileDataAccess::empty());
    }
}

/// A transaction whose sender, recipient or applied EIP-7702 authority is the beneficiary is
/// detained from its first instruction: its limit is the cap.
#[test]
fn test_a_transaction_touching_the_beneficiary_is_detained_from_the_start() {
    use revm::{
        context::transaction::TransactionType,
        context_interface::{
            either::Either,
            transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
        },
    };
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let spinner = spin(BytecodeBuilder::default());
        // Detained before its first instruction: the whole program runs after the read.
        let left = Charges::default().spin(0).left(CAP);

        // The sender.
        let db = MemoryDatabase::default().account_code(CONTRACT, spinner.clone());
        let run_sender = execute(db, tx(BENEFICIARY, CONTRACT, gas_limit));
        // A sender's intrinsic gas is the same whoever it is.
        assert_eq!(assert_stopped(&run_sender, intrinsic, left), CAP, "the sender");

        // The recipient.
        let db = MemoryDatabase::default().account_code(BENEFICIARY, spinner.clone());
        let run_recipient = execute(db, tx(CALLER, BENEFICIARY, gas_limit));
        assert_eq!(assert_stopped(&run_recipient, intrinsic, left), CAP, "the recipient");

        // An applied authority.
        let authorization = Either::Right(RecoveredAuthorization::new_unchecked(
            Authorization { chain_id: U256::ZERO, address: CHILD, nonce: 0 },
            RecoveredAuthority::Valid(BENEFICIARY),
        ));
        let authorizing = OpTx(op_transaction(TxEnv {
            tx_type: TransactionType::Eip7702 as u8,
            caller: CALLER,
            kind: TxKind::Call(CONTRACT),
            gas_limit,
            gas_priority_fee: Some(0),
            authorization_list: vec![authorization],
            ..Default::default()
        }));
        let db = MemoryDatabase::default().account_code(CONTRACT, spinner);
        let run_authority = execute(db, authorizing);
        assert_eq!(run_authority.limit, Some(CAP), "the authority");
        assert!(matches!(run_authority.outcome.result, ExecutionResult::Revert { .. }));
        assert_eq!(run_authority.accessed, VolatileDataAccess::BENEFICIARY_BALANCE);
    }
}

/// An account read of anything but the beneficiary, and a storage read of anything but the
/// Oracle, are not volatile.
#[test]
fn test_other_accounts_and_storage_are_not_volatile() {
    let code = BytecodeBuilder::default()
        .push_address(CHILD)
        .append(BALANCE)
        .append(POP)
        .append_many([PUSH0, SLOAD, POP]);
    let code = call(call(code, CALL, CHILD), STATICCALL, CHILD).stop().build();
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, code)
        .account_code(CHILD, BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP]).build());
    let run = execute(db, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success());
    assert_eq!(run.accessed, VolatileDataAccess::empty());
    assert_eq!(run.limit, None);
}

/// `BLOBHASH` reads the transaction's own blob hashes, which nothing but the transaction decides:
/// it is not volatile.
#[test]
fn test_blobhash_is_not_volatile() {
    let code = BytecodeBuilder::default().append_many([PUSH0, BLOBHASH, POP, STOP]).build();
    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        tx(CALLER, CONTRACT, BELOW),
    );
    assert!(run.outcome.result.is_success());
    assert_eq!(run.accessed, VolatileDataAccess::empty());
}

/// A read answers with the block's own field: the Host marks the read and serves the value it
/// loaded, whether or not the read caps anything.
#[test]
fn test_a_read_answers_with_the_blocks_field() {
    let reads = [
        (NUMBER, U256::from(300)),
        (TIMESTAMP, U256::from(1_700_000_000)),
        (COINBASE, U256::from_be_slice(BENEFICIARY.as_slice())),
        (DIFFICULTY, U256::from_be_bytes(B256::repeat_byte(7).0)),
        (GASLIMIT, U256::from(10_000_000_000_u64)),
        (BASEFEE, U256::from(1)),
        (BLOBBASEFEE, U256::from(3)),
        (SLOTNUM, U256::from(9)),
    ];
    let mut code = BytecodeBuilder::default();
    for (slot, (opcode, _)) in reads.iter().enumerate() {
        code = code.append(*opcode).push_number(slot as u8).append(SSTORE);
    }
    // BLOCKHASH of the parent, and the beneficiary's balance.
    let code = code
        .push_number(299_u16)
        .append(BLOCKHASH)
        .push_number(8_u8)
        .append(SSTORE)
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .push_number(9_u8)
        .append(SSTORE)
        .stop()
        .build();
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, code)
        .account_balance(BENEFICIARY, U256::from(42))
        .account_balance(CALLER, U256::from(1_000_000_000_000_u64));
    let mut ctx = context(db).with_block(BlockEnv { basefee: 1, ..block() });
    ctx.modify_chain(|chain| chain.l1_base_fee = U256::ZERO);
    let tx = OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit: BELOW,
        gas_price: 1,
        ..Default::default()
    }));
    let run = run_on(&mut MegaEvm::new(ctx), tx);
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    let storage = &run.outcome.state[&CONTRACT].storage;
    for (slot, (opcode, value)) in reads.iter().enumerate() {
        assert_eq!(storage[&U256::from(slot)].present_value, *value, "{opcode:#04x}");
    }
    let hash = revm::Database::block_hash(&mut MemoryDatabase::default(), 299).unwrap();
    assert_eq!(storage[&U256::from(8)].present_value, U256::from_be_bytes(hash.0), "BLOCKHASH");
    assert_eq!(storage[&U256::from(9)].present_value, U256::from(42), "BALANCE");
    assert!(run.limit.is_some());
}

/* ---------- the limit ---------- */

/// A later read lowers the limit only when its own is lower: with equal caps the first read
/// binds, in either order of block environment and Oracle.
#[test]
fn test_the_most_restrictive_limit_binds_whatever_the_order() {
    assert_eq!(ORACLE_ACCESS_COMPUTE_GAS, CAP, "the two caps are equal today");
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build();
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        for block_env_first in [true, false] {
            let first = |c| {
                if block_env_first {
                    op(c, TIMESTAMP)
                } else {
                    call(c, CALL, ORACLE_CONTRACT_ADDRESS)
                }
            };
            let second = |c| {
                if block_env_first {
                    call(c, CALL, ORACLE_CONTRACT_ADDRESS)
                } else {
                    op(c, TIMESTAMP)
                }
            };
            // The first read, 1,000,000 of compute, the second read, then forever.
            let code = spin(second(work(first(BytecodeBuilder::default()), 300)));
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, code)
                .account_code(ORACLE_CONTRACT_ADDRESS, oracle.clone());
            let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
            // What the program charges after the first read, which sets the limit: the loop
            // expands the memory its work copies first.
            let charges = if block_env_first {
                // The `POP`, the work, then the call to the Oracle — five pushes, its address,
                // `GAS` and the cold access — the Oracle's push, cold `SLOAD` and `POP`, and the
                // caller's `POP`.
                Charges::default()
                    .then(&[2])
                    .work(300, 0)
                    .then(&[2, 2, 2, 2, 2, 3, 2, 2_600, 2, 2_100, 2, 2])
            } else {
                // The Oracle's `POP` and its caller's, the work, then `TIMESTAMP` and its `POP`.
                Charges::default().then(&[2, 2]).work(300, 0).then(&[2, 2])
            };
            let limit = assert_stopped(&run, intrinsic, charges.spin(1_024).left(CAP));
            assert!(limit < CAP + 100_000, "the first read set the limit: {limit}");
            assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP | VolatileDataAccess::ORACLE);
        }
    }
}

/// The caps are runtime limits: a caller's limits set either one, and each kind of read is held
/// to its own. Under `no_limits` both are unlimited and a transaction that reads is not detained:
/// it computes on until its own gas is gone, and halts.
#[test]
fn test_a_callers_limits_set_the_caps() {
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build();
    let limits = EvmTxRuntimeLimits::default()
        .with_block_env_access_compute_gas_limit(1_000_000)
        .with_oracle_access_compute_gas_limit(2_000_000);
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let run_under = |limits: EvmTxRuntimeLimits, code: Bytes| {
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, code)
                .account_code(ORACLE_CONTRACT_ADDRESS, oracle.clone());
            let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
            run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit))
        };

        // `TIMESTAMP` is the first instruction: 2 of compute at the read. Its `POP` and the loop
        // come after it.
        let run = run_under(limits, spin(op(BytecodeBuilder::default(), TIMESTAMP)));
        let left = Charges::default().then(&[2]).spin(0).left(1_000_000);
        assert_eq!(assert_stopped(&run, intrinsic, left), 2 + 1_000_000);

        let run = run_under(
            limits,
            spin(call(BytecodeBuilder::default(), CALL, ORACLE_CONTRACT_ADDRESS)),
        );
        // The Oracle's `POP` and its caller's, then the loop.
        let left = Charges::default().then(&[2, 2]).spin(0).left(2_000_000);
        let limit = assert_stopped(&run, intrinsic, left);
        assert!((2_000_000..2_100_000).contains(&limit), "the Oracle's own cap: {limit}");
        assert_eq!(run.accessed, VolatileDataAccess::ORACLE);

        let run = run_under(
            EvmTxRuntimeLimits::no_limits(),
            spin(op(work(BytecodeBuilder::default(), WORK), TIMESTAMP)),
        );
        assert!(!run.detains);
        assert_eq!(run.limit, None);
        assert_eq!(run.outcome.limit_exceeded, None);
        assert!(
            matches!(run.outcome.result, ExecutionResult::Halt { .. }),
            "{:?}",
            run.outcome.result
        );
    }
}

/// The default runtime limits detain: [`EvmTxRuntimeLimits::default`], and the transaction half
/// of [`ProtocolLimits::DEFAULT`] that a block executor installs, hold each kind of read to the
/// spec's cap. It is `no_limits` that turns detention off, with every other per-transaction limit.
#[test]
fn test_the_default_limits_detain() {
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build();
    let timestamp = op(BytecodeBuilder::default(), TIMESTAMP).stop().build();
    let reads_oracle =
        call(BytecodeBuilder::default(), CALL, ORACLE_CONTRACT_ADDRESS).stop().build();
    for limits in [EvmTxRuntimeLimits::default(), ProtocolLimits::DEFAULT.tx_runtime_limits] {
        let run_under = |limits: EvmTxRuntimeLimits, code: &Bytes| {
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, code.clone())
                .account_code(ORACLE_CONTRACT_ADDRESS, oracle.clone());
            let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
            run_on(&mut evm, tx(CALLER, CONTRACT, BELOW))
        };
        let run = run_under(limits, &timestamp);
        assert!(run.detains);
        assert_eq!(run.limit, Some(2 + BLOCK_ENV_ACCESS_COMPUTE_GAS), "TIMESTAMP was the first");
        let run = run_under(limits, &reads_oracle);
        let limit = run.limit.unwrap();
        assert!(
            (ORACLE_ACCESS_COMPUTE_GAS..ORACLE_ACCESS_COMPUTE_GAS + 100_000).contains(&limit),
            "{limit}"
        );

        let run = run_under(EvmTxRuntimeLimits::no_limits(), &timestamp);
        assert!(!run.detains);
        assert_eq!(run.limit, None);
    }
}

/// Spending exactly the cap after the read is within it; the limit is strict.
#[test]
fn test_spending_exactly_the_cap_completes() {
    // TIMESTAMP (2) and POP (2) leave 19,999,998 of the cap; the loop spends 26 a round after
    // the three gas its counter's push costs, and its last round ends in POP (2) and STOP.
    let rounds = (CAP - 2 - 2 - 3 - 2) / 26;
    let spent = 2 + 2 + 3 + u64::from(u32::try_from(rounds).unwrap()) * 26 + 2;
    assert!(spent <= CAP + 2, "{spent}");
    for gas_limit in TIERS {
        let code = burn(op(BytecodeBuilder::default(), TIMESTAMP), u32::try_from(rounds).unwrap())
            .stop()
            .build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert_eq!(run.outcome.gas.regular, intrinsic(gas_limit) + spent);

        // One more round crosses it.
        let more = u32::try_from(rounds).unwrap() + 1;
        let code = burn(op(BytecodeBuilder::default(), TIMESTAMP), more).stop().build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        let left = Charges::default().then(&[2]).burn(more).left(CAP);
        assert_stopped(&run, intrinsic(gas_limit), left);
    }
}

/// A value call's stipend is gas nobody paid, so what the callee runs on it is not compute beyond
/// what its caller's ledger shows: a value-carrying child that reads and spins stops with the
/// regular ledger at its compute before the charge that crossed, and so does a transaction
/// carrying value, whose own frame is given what the transaction has left and no stipend.
#[test]
fn test_a_stipend_is_not_compute() {
    let child = spin(op(BytecodeBuilder::default(), TIMESTAMP));
    let parent = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(1_u8)
        .push_address(CHILD)
        .append_many([GAS, CALL, POP, STOP])
        .build();
    for gas_limit in TIERS {
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent.clone())
            .account_balance(CONTRACT, U256::from(1))
            .account_code(CHILD, child.clone());
        let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
        // Either way the reading frame's `POP` and its loop come after the read.
        let left = Charges::default().then(&[2]).spin(0).left(CAP);
        assert_stopped(&run, intrinsic(gas_limit), left);

        let db = MemoryDatabase::default()
            .account_code(CONTRACT, spin(op(BytecodeBuilder::default(), TIMESTAMP)))
            .account_balance(CALLER, U256::from(1));
        let mut valued = tx(CALLER, CONTRACT, gas_limit);
        valued.0.base.value = U256::from(1);
        let run = execute(db, valued);
        let limit = assert_stopped(&run, intrinsic_of_a_value_call(gas_limit), left);
        assert_eq!(limit, CAP + 2, "TIMESTAMP was the first opcode");
    }
}

/// A read made by a call is counted from the compute the caller paid at it: its pushes, its
/// static and dynamic gas, and the gas it forwards to the callee less a value call's stipend,
/// which nobody paid. The limit is exact for a plain call and for one carrying value.
#[test]
fn test_a_call_that_reads_counts_from_what_its_caller_paid() {
    for (value, compute) in [
        // PUSH0 x4, PUSH0, PUSH20, GAS and a warm CALL.
        (false, 2 * 4 + 2 + 3 + 2 + 100),
        // PUSH0 x4, PUSH1, PUSH20, GAS, a warm CALL and the value transfer.
        (true, 2 * 4 + 3 + 3 + 2 + 100 + 9_000),
    ] {
        let code = BytecodeBuilder::default().append_many([PUSH0, PUSH0, PUSH0, PUSH0]);
        let code = if value { code.push_number(1_u8) } else { code.append(PUSH0) };
        let code = code.push_address(BENEFICIARY).append_many([GAS, CALL, POP, STOP]).build();
        for gas_limit in TIERS {
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, code.clone())
                .account_balance(CONTRACT, U256::from(1))
                .account_balance(BENEFICIARY, U256::from(1));
            let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
            assert!(run.outcome.result.is_success(), "value {value}: {:?}", run.outcome.result);
            assert_eq!(run.limit, Some(compute + CAP), "value {value}");
        }
    }
}

/// The regular gas a call from `CALLER` carrying one wei spends before its first instruction.
fn intrinsic_of_a_value_call(gas_limit: u64) -> u64 {
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, BytecodeBuilder::default().stop().build())
        .account_balance(CALLER, U256::from(1));
    let mut valued = tx(CALLER, CONTRACT, gas_limit);
    valued.0.base.value = U256::from(1);
    execute(db, valued).outcome.gas.regular
}

/* ---------- across frames ---------- */

/// A child's read caps its caller after the child returns: the caller resumes on what the limit
/// leaves the transaction, and stops there.
#[test]
fn test_a_childs_read_caps_its_caller() {
    let child = work(op(BytecodeBuilder::default(), TIMESTAMP), 500).stop().build();
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let parent = spin(call(BytecodeBuilder::default(), CALL, CHILD));
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent)
            .account_code(CHILD, child.clone());
        let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
        // The child's `POP` and work, then its caller's `POP` and loop, each in its own memory.
        let left = Charges::default().then(&[2]).work(500, 0).then(&[2]).spin(0).left(CAP);
        let limit = assert_stopped(&run, intrinsic, left);
        assert!(limit < CAP + 100_000, "the child read near the start: {limit}");
    }
}

/// When a child crosses the cap, no caller resumes: the caller's code after the call — a write
/// and a log — never runs, the child runs the last step, and every frame returns the stop.
#[test]
fn test_no_caller_resumes_after_the_stop() {
    let child = spin(op(BytecodeBuilder::default(), TIMESTAMP));
    let parent = call(BytecodeBuilder::default(), CALL, CHILD)
        .sstore(U256::from(1), U256::from(1))
        .append_many([PUSH0, PUSH0, LOG0])
        .stop()
        .build();
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent.clone())
            .account_code(CHILD, child.clone());
        let mut evm = MegaEvm::new(context(db)).with_inspector(Calls::default());
        let run = run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit));
        let left = Charges::default().then(&[2]).spin(0).left(CAP);
        let limit = assert_stopped(&run, intrinsic, left);
        assert!(run.outcome.state[&CONTRACT].storage.values().all(|slot| !slot.is_changed()));
        let calls = &evm.inspector().calls;
        assert_eq!(
            calls.iter().map(|c| (c.target, c.result, c.output.clone())).collect::<Vec<_>>(),
            vec![
                (CHILD, InstructionResult::Revert, stop_data(limit)),
                (CONTRACT, InstructionResult::Revert, stop_data(limit)),
            ]
        );
        assert!(!evm.inspector().opcodes.contains(&SSTORE), "the caller did not resume");
        // A caller that resumed would fail its first charge on the withheld part, before its
        // write, and report the same stop on the same bill: only its step shows it ran.
        assert_eq!(evm.inspector().last_frame, Some(CHILD), "the child ran the last step");
    }
}

/// The stop settles like an EIP-8037 revert: what the sender pays does not depend on whether
/// the transaction had a reservoir, because the reservoir and everything detention withheld go
/// back to it. Above the execution cap the reservoir comes back whole, less the body's history,
/// which it paid first.
#[test]
fn test_the_stop_bills_the_same_above_and_below_the_execution_cap() {
    let child = spin(op(BytecodeBuilder::default(), TIMESTAMP));
    let parent = spin(call(BytecodeBuilder::default(), CALL, CHILD));
    let used: Vec<_> = TIERS
        .iter()
        .map(|gas_limit| {
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, parent.clone())
                .account_code(CHILD, child.clone());
            let run = execute(db, tx(CALLER, CONTRACT, *gas_limit));
            let left = Charges::default().then(&[2]).spin(0).left(CAP);
            assert_stopped(&run, intrinsic(*gas_limit), left);
            let body_history = run.outcome.gas.history;
            let reservoir = gas_limit.saturating_sub(TX_GAS_LIMIT_CAP).saturating_sub(body_history);
            assert_eq!(run.outcome.gas.reservoir_remaining, reservoir, "the reservoir came back");
            let gas = run.outcome.gas;
            (gas.gas_used, gas.regular, gas.state, gas.history)
        })
        .collect();
    assert_eq!(used[0], used[1]);
    assert!(used[0].0 < CAP + 1_000_000, "the stop does not burn the gas: {}", used[0].0);
}

/* ---------- real out-of-gas ---------- */

/// A frame detention withheld nothing from runs out of its own gas: it halts and burns what it
/// was given, below and above the execution cap.
#[test]
fn test_running_out_of_the_frames_own_gas_still_halts() {
    // The transaction has less than the cap: nothing is withheld.
    let code = spin(op(BytecodeBuilder::default(), TIMESTAMP));
    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        tx(CALLER, CONTRACT, 1_000_000),
    );
    assert!(matches!(run.outcome.result, ExecutionResult::Halt { .. }), "{:?}", run.outcome.result);
    assert_eq!(run.outcome.result.gas().tx_gas_used(), 1_000_000, "a halt burns the gas");
    assert_eq!(run.outcome.limit_exceeded, None);
    assert!(run.limit.is_some(), "the read still set a limit");

    // A child given less than the limit leaves halts on its own, and its caller resumes.
    let child = spin(BytecodeBuilder::default());
    let parent = op(BytecodeBuilder::default(), TIMESTAMP)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CHILD)
        .push_number(100_000_u32)
        .append(CALL)
        .push_number(0_u8)
        .append(SSTORE)
        .stop()
        .build();
    for gas_limit in TIERS {
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent.clone())
            .account_code(CHILD, child.clone());
        let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        let slot = run.outcome.state[&CONTRACT].storage.get(&U256::ZERO).unwrap();
        assert_eq!(slot.present_value, U256::ZERO, "the call failed: the child halted");
        assert!(slot.is_changed() || slot.original_value.is_zero());
    }
}

/// Writes after a read are held to the cap by the regular gas they spend. With room in the
/// reservoir for their state and history gas, a thousand fresh slots — 22,106,000 of compute —
/// cross it and stop the transaction, at the first write's charge past it. Without a reservoir,
/// and with less gas than the writes spend before their compute reaches the cap, the writes drain
/// the withheld gas first and the frame runs out of its own gas where it would have undetained: it
/// halts.
#[test]
fn test_writes_after_a_read_stop_at_the_cap_or_run_out_of_their_own_gas() {
    let mut code = op(BytecodeBuilder::default(), TIMESTAMP);
    for slot in 1..=1_000_u64 {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    let code = code.stop().build();

    // A reservoir that pays every write's state gas and history.
    let gas_limit = TX_GAS_LIMIT_CAP + 1_000 * fresh_write_spill();
    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code.clone()),
        tx(CALLER, CONTRACT, gas_limit),
    );
    let left = Charges::default().then(&[2]).fresh_writes(1_000).left(CAP);
    assert_stopped(&run, intrinsic(gas_limit), left);

    // The cap, and half of what the writes that fit in it spill: more than the cap, so the read
    // withholds gas, and less than those writes spend, so the gas runs out before the compute
    // reaches the cap. Below the execution cap either way.
    let gas_limit = BELOW.min(CAP + CAP / FRESH_WRITE * fresh_write_spill() / 2);
    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        tx(CALLER, CONTRACT, gas_limit),
    );
    assert!(matches!(run.outcome.result, ExecutionResult::Halt { .. }), "{:?}", run.outcome.result);
    assert_eq!(run.outcome.limit_exceeded, None);
    assert_eq!(run.outcome.result.gas().tx_gas_used(), gas_limit, "a halt burns the gas");
}

/* ---------- what is not marked ---------- */

/// A read that does not happen marks nothing, and neither does one whose opcode fails after it:
/// the frame halts, and the caller it halts back into computes on uncapped.
///
/// - The Oracle's slot is cold and the frame cannot pay the cold access, so the Host does not load
///   it. (The block beneficiary is always warm, so its load cannot be skipped this way.)
/// - A value transfer to the empty beneficiary loads it, then cannot pay the new account, and the
///   `CALL` fails.
#[test]
fn test_a_failed_load_caps_nothing() {
    // The push and SLOAD's static 100 leave less than the 2,000 more a cold slot costs.
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, STOP]).build();
    // 9,000 for the transfer and 25,000 for the new account do not fit in 30,000.
    let child = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(1_u8)
        .push_address(BENEFICIARY)
        .append_many([PUSH0, CALL, STOP])
        .build();
    for (callee, gas) in [(ORACLE_CONTRACT_ADDRESS, 2_000_u32), (CHILD, 30_000)] {
        let parent = BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(callee)
            .push_number(gas)
            .append(CALL)
            .push_number(0_u8)
            .append(SSTORE);
        // More compute than the cap.
        let parent = work(parent, 7_000).stop().build();
        for gas_limit in TIERS {
            let db = MemoryDatabase::default()
                .account_balance(CHILD, U256::from(10))
                .account_code(CONTRACT, parent.clone())
                .account_code(ORACLE_CONTRACT_ADDRESS, oracle.clone())
                .account_code(CHILD, child.clone());
            let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
            assert!(run.outcome.result.is_success(), "{callee}: {:?}", run.outcome.result);
            let slot = run.outcome.state[&CONTRACT].storage.get(&U256::ZERO);
            assert!(
                slot.is_none_or(|slot| slot.present_value.is_zero()),
                "{callee}: the call failed"
            );
            assert!(run.outcome.gas.regular > CAP, "{callee}: the caller computed past the cap");
            assert_eq!(run.limit, None, "{callee}");
            assert_eq!(run.accessed, VolatileDataAccess::empty(), "{callee}");
        }
    }
}

/// Reads the block's timestamp through the Host before every step, as a tracer might.
struct ReadsTheTimestamp;

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for ReadsTheTimestamp {
    fn step(&mut self, _interp: &mut Interpreter<EthInterpreter>, context: &mut MegaContext<DB>) {
        let _ = revm::context_interface::Host::timestamp(context);
    }
}

/// What an inspector reads through the Host is the inspector's, not the transaction's: it is
/// neither committed as the next opcode's read nor refused as one. A transaction that loads a
/// slot and computes past the cap under a tracer reading the timestamp at every step is not
/// detained, and with volatile-data access off it still loads its slot.
#[test]
fn test_an_inspectors_reads_are_not_the_transactions() {
    let code = work(BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP]), 7_000);
    let code = code.stop().build();
    for disabled in [false, true] {
        let db = MemoryDatabase::default().account_code(CONTRACT, code.clone());
        let mut ctx = context(db);
        if disabled {
            ctx = ctx.with_volatile_access_disabled_from(0);
        }
        let mut evm = MegaEvm::new(ctx).with_inspector(ReadsTheTimestamp);
        let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
        assert!(run.outcome.result.is_success(), "disabled {disabled}: {:?}", run.outcome.result);
        assert_eq!(run.limit, None, "disabled {disabled}");
        assert_eq!(run.accessed, VolatileDataAccess::empty(), "disabled {disabled}");
        assert!(run.detains, "a user's transaction is detained when it reads");
    }
}

/* ---------- refused reads ---------- */

/// Records every call's result and the gas it spent, every opcode that ran, and the frame the
/// last one ran in.
#[derive(Default)]
pub(crate) struct Calls {
    pub(crate) calls: Vec<CallRecord>,
    pub(crate) opcodes: Vec<u8>,
    pub(crate) last_frame: Option<Address>,
}

pub(crate) struct CallRecord {
    pub(crate) target: Address,
    pub(crate) result: InstructionResult,
    pub(crate) output: Bytes,
    pub(crate) spent: u64,
    /// The gas the caller forwarded.
    pub(crate) forward: u64,
    /// Whether revm ran a precompile for the call.
    pub(crate) precompile_ran: bool,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Calls {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        use revm::interpreter::interpreter_types::Jumps;
        self.opcodes.push(interp.bytecode.opcode());
        self.last_frame = Some(interp.input.target_address);
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let gas = outcome.result.gas;
        self.calls.push(CallRecord {
            target: inputs.target_address,
            result: outcome.result.result,
            output: outcome.result.output.clone(),
            spent: gas.limit() - gas.remaining(),
            forward: inputs.gas_limit,
            precompile_ran: outcome.was_precompile_called,
        });
    }
}

/// While volatile-data access is off, every read is refused: the frame reverts with
/// `VolatileDataAccessDisabled(accessType)`, having paid the static gas of what ran and nothing
/// more — not a cold access, not a copy's memory, not a call's value transfer — and the refused
/// read caps nothing.
#[test]
fn test_a_refused_read_reverts_the_frame_and_charges_its_static_gas() {
    // (code, the static gas of everything in it, the refused kind)
    let cases: Vec<(&str, Bytes, u64, VolatileDataAccess)> = vec![
        (
            "TIMESTAMP",
            BytecodeBuilder::default().append(TIMESTAMP).build(),
            2,
            VolatileDataAccess::TIMESTAMP,
        ),
        (
            "SLOTNUM",
            BytecodeBuilder::default().append(SLOTNUM).build(),
            2,
            VolatileDataAccess::SLOT_NUM,
        ),
        (
            "BLOCKHASH",
            BytecodeBuilder::default().push_number(299_u16).append(BLOCKHASH).build(),
            3 + 20,
            VolatileDataAccess::BLOCK_HASH,
        ),
        (
            "BALANCE",
            on_beneficiary(BytecodeBuilder::default(), BALANCE).build(),
            3 + 100,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
        (
            "EXTCODECOPY",
            BytecodeBuilder::default()
                .push_number(1024_u16)
                .append_many([PUSH0, PUSH0])
                .push_address(BENEFICIARY)
                .append(EXTCODECOPY)
                .build(),
            3 + 2 + 2 + 3 + 100,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
        (
            "CALL with value",
            BytecodeBuilder::default()
                .push_number(64_u8)
                .append_many([PUSH0, PUSH0, PUSH0])
                .push_number(1_u8)
                .push_address(BENEFICIARY)
                .append(GAS)
                .append(CALL)
                .build(),
            3 + 2 + 2 + 2 + 3 + 3 + 2 + 100,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
        (
            "SELFDESTRUCT",
            on_beneficiary(BytecodeBuilder::default(), SELFDESTRUCT).build(),
            3 + 5_000,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
        (
            "SELFBALANCE of the beneficiary's own frame",
            BytecodeBuilder::default().append(SELFBALANCE).build(),
            5,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
    ];
    for gas_limit in TIERS {
        for (name, code, static_gas, refused) in &cases {
            // The beneficiary runs its own SELFBALANCE; every other read runs in the child.
            let target = if name.starts_with("SELFBALANCE") { BENEFICIARY } else { CHILD };
            let parent = call(BytecodeBuilder::default(), CALL, target).stop().build();
            let db = MemoryDatabase::default()
                .account_balance(CHILD, U256::from(10))
                .account_code(CONTRACT, parent)
                .account_code(target, code.clone());
            let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
                .with_inspector(Calls::default());
            let run = run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit));
            assert!(run.outcome.result.is_success(), "{name}: the caller resumes");
            let record = &evm.inspector().calls[0];
            assert_eq!(record.target, target);
            assert_eq!(record.result, InstructionResult::Revert, "{name}");
            assert_eq!(
                record.output,
                volatile_data_access_disabled_revert_data(*refused),
                "{name}"
            );
            assert_eq!(record.spent, *static_gas, "{name}: the static gas of what ran");
            // Only the calls a transaction to the beneficiary makes read it; the refusal did not.
            if target == CHILD {
                assert_eq!(run.accessed, VolatileDataAccess::empty(), "{name}");
                assert_eq!(run.limit, None, "{name}");
            }
        }
    }
}

/// The refusal of a `SLOTNUM` names access type 12, one past the contract's enum; the others name
/// their enum variant, `BLOCKHASH`'s the hash it reads (7), as on the legacy engine, although
/// revm's instruction loads the block number first.
#[test]
fn test_the_refusal_names_the_access_type() {
    use mega_evm::system::{IMegaAccessControl, VolatileDataAccessType};
    for (access, variant) in [
        (VolatileDataAccess::ORACLE, VolatileDataAccessType::Oracle),
        (VolatileDataAccess::BLOCK_HASH, VolatileDataAccessType::BlockHash),
        (VolatileDataAccess::BLOCK_NUMBER, VolatileDataAccessType::BlockNumber),
    ] {
        assert_eq!(
            volatile_data_access_disabled_revert_data(access),
            Bytes::from(
                IMegaAccessControl::VolatileDataAccessDisabled { accessType: variant }.abi_encode()
            )
        );
    }
    assert_eq!(VolatileDataAccessType::BlockHash as u8, 7);
    let slot_num = volatile_data_access_disabled_revert_data(VolatileDataAccess::SLOT_NUM);
    assert_eq!(&slot_num[..4], IMegaAccessControl::VolatileDataAccessDisabled::SELECTOR.as_slice());
    assert_eq!(U256::from_be_slice(&slot_num[4..]), U256::from(12));
    assert_eq!(mega_evm::system::SLOT_NUM_ACCESS_TYPE, 12);
    assert_eq!(
        mega_evm::decode_volatile_data_access_disabled(&slot_num),
        Some(VolatileDataAccess::SLOT_NUM)
    );
}

/// The switch holds for the frame it is off from and every frame below, and a frame above it
/// reads as usual once that frame returned.
#[test]
fn test_the_switch_is_scoped_to_its_subtree() {
    let child = op(BytecodeBuilder::default(), TIMESTAMP).stop().build();
    // The top frame calls the child twice, then reads itself.
    let parent = call(call(BytecodeBuilder::default(), CALL, CHILD), CALL, CHILD);
    let parent = op(parent, NUMBER).stop().build();
    let db = MemoryDatabase::default().account_code(CONTRACT, parent).account_code(CHILD, child);
    let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
        .with_inspector(Calls::default());
    let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success());
    let results: Vec<_> = evm.inspector().calls.iter().map(|c| c.result).collect();
    assert_eq!(
        results,
        vec![InstructionResult::Revert, InstructionResult::Stop, InstructionResult::Stop],
        "the first child is refused; the switch was its own, so the second child reads"
    );
    assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP | VolatileDataAccess::BLOCK_NUMBER);

    // Off from the top: the transaction's own frame is refused.
    let code = op(BytecodeBuilder::default(), TIMESTAMP).stop().build();
    let db = MemoryDatabase::default().account_code(CONTRACT, code);
    let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(0));
    let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    match &run.outcome.result {
        ExecutionResult::Revert { output, .. } => assert_eq!(
            output,
            &volatile_data_access_disabled_revert_data(VolatileDataAccess::TIMESTAMP)
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(run.outcome.limit_exceeded, None, "a refusal is the frame's revert, not a stop");
}

/* ---------- withheld gas does not leak ---------- */

/// An interceptor's answer carries the forwarded gas back without running a frame: the caller
/// is held to the limit as it resumes. The call forwards what it forwards without the read — the
/// withheld part of the caller's gas is part of the 63/64 — and gets all of it back, so the
/// caller holds the same gas after the call with the read and without it.
///
/// What `remainingComputeGas()` answers is what the caller could still spend: without the read,
/// its own regular gas with the forward counted back, the gas `GAS` reads after the call plus the
/// `POP` and the `GAS` it costs to read it; after the read, the cap less what the caller spent
/// since the read — the read's `POP`, the calldata's `MSTORE` with its push and its memory, the
/// call's five pushes and its `GAS`, and the cold access to the contract: 2,631.
#[test]
fn test_an_interceptors_answer_does_not_lift_the_cap() {
    let calldata = IMegaLimitControl::remainingComputeGasCall {}.abi_encode();
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let code = op(BytecodeBuilder::default(), TIMESTAMP)
            .mstore(0, &calldata)
            .push_number(32_u8)
            .push_number(0_u8)
            .push_number(4_u8)
            .push_number(0_u8)
            .push_address(LIMIT_CONTROL_ADDRESS)
            .append(GAS)
            .append(STATICCALL)
            .append(POP)
            .push_number(0_u8)
            .append(MLOAD)
            .push_number(0_u8)
            .append(SSTORE);
        let code = spin(code);
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        // The 2,631 spent up to the call's cold access; the answer, which spends nothing; the
        // `POP`, the push, `MLOAD`, the push and a fresh, cold slot's write; then the loop, in a
        // memory of one word.
        let left = Charges::default().then(&[2_631, 2, 3, 3, 3, 100, 22_000]).spin(1).left(CAP);
        assert_stopped(&run, intrinsic, left);

        // The answer and the gas the caller holds after the call, read without the loop, after
        // the read and after a push in its place.
        let answer = |first: u8| {
            let code = op(BytecodeBuilder::default(), first)
                .mstore(0, &calldata)
                .push_number(32_u8)
                .push_number(0_u8)
                .push_number(4_u8)
                .push_number(0_u8)
                .push_address(LIMIT_CONTROL_ADDRESS)
                .append(GAS)
                .append(STATICCALL)
                .append(POP)
                .append(GAS)
                .push_number(32_u8)
                .append(MSTORE)
                .push_number(64_u8)
                .push_number(0_u8)
                .append(RETURN)
                .build();
            let run = execute(
                MemoryDatabase::default().account_code(CONTRACT, code),
                tx(CALLER, CONTRACT, gas_limit),
            );
            assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
            let output = run.outcome.result.output().unwrap().clone();
            (U256::from_be_slice(&output[..32]), U256::from_be_slice(&output[32..64]))
        };
        let (detained, held) = answer(TIMESTAMP);
        let (undetained, held_without_the_read) = answer(PUSH0);
        assert_eq!(held, held_without_the_read, "the forward came back whole");
        assert_eq!(undetained, held + U256::from(2 + 2), "the caller's own gas: POP and GAS");
        assert_eq!(detained, U256::from(CAP - 2_631), "what the cap leaves");
    }
}

/// A frame's return hands back what detention withheld, on success, revert and halt alike: the
/// transaction spends exactly what it spends without the read, which a child that stays within
/// the cap does not notice.
#[test]
fn test_a_frames_return_hands_back_what_was_withheld() {
    // The child reads (or pushes a word, for the same two gas), computes a little, then ends.
    let ends: [(&str, Append); 3] =
        [("success", |c| c.stop()), ("revert", |c| c.revert()), ("halt", |c| c.append(INVALID))];
    for gas_limit in TIERS {
        for (name, end) in ends {
            let used = |read: u8| {
                let child = end(work(op(BytecodeBuilder::default(), read), 50)).build();
                let parent = call(BytecodeBuilder::default(), CALL, CHILD).stop().build();
                let db = MemoryDatabase::default()
                    .account_code(CONTRACT, parent)
                    .account_code(CHILD, child);
                let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
                assert!(run.outcome.result.is_success(), "{name}: {:?}", run.outcome.result);
                (run.outcome.result.gas().tx_gas_used(), run.limit.is_some())
            };
            let (detained, limited) = used(TIMESTAMP);
            let (plain, unlimited) = used(PUSH0);
            assert!(limited && !unlimited);
            assert_eq!(detained, plain, "{name}: nothing withheld stays behind");
        }
    }
}

/// State and history gas are not compute, whether they come out of the reservoir or spill onto
/// regular gas: after the read, a frame spends more than the cap on fresh slots and still
/// completes, its compute within the cap and their state and history gas past it.
#[test]
fn test_state_and_history_gas_are_not_compute() {
    // As many fresh slots as the cap leaves compute for, and as a gas limit below the execution
    // cap pays for with a million to spare: at the spec's prices, more state gas than the cap.
    let spill = fresh_write_spill();
    let slots = (CAP / FRESH_WRITE).min((BELOW - 1_000_000) / (FRESH_WRITE + spill));
    assert!(slots * (FRESH_WRITE + spill) > CAP, "counted as compute, the writes cross the cap");
    let mut code = op(BytecodeBuilder::default(), TIMESTAMP);
    for slot in 1..=slots {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    let code = code.stop().build();
    for gas_limit in TIERS {
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code.clone()),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        let gas = run.outcome.gas;
        assert!(gas.state + gas.history >= slots * spill, "{gas:?}");
        assert!(gas.regular < CAP, "{gas:?}");
    }
}

/// State and history gas that spilled onto regular gas before the read are not compute either.
/// Below the execution cap there is no reservoir, so every fresh slot's state gas and its record's
/// history spill; above it the reservoir pays them. The limit is the same: the compute before the
/// read is the writes' regular gas alone.
#[test]
fn test_gas_spilled_before_the_read_is_not_compute() {
    let mut code = BytecodeBuilder::default();
    for slot in 1..=100_u64 {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    let code = spin(op(code, TIMESTAMP));
    for gas_limit in TIERS {
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code.clone()),
            tx(CALLER, CONTRACT, gas_limit),
        );
        let left = Charges::default().then(&[2]).spin(0).left(CAP);
        let limit = assert_stopped(&run, intrinsic(gas_limit), left);
        // Two pushes and a fresh slot's 22,100 of regular gas, a hundred times, then TIMESTAMP.
        assert_eq!(limit - CAP, 100 * (3 + 3 + 22_100) + 2, "{gas_limit}");
    }
}

/// A slot restored to its original value refills the state gas it spilled onto regular gas; the
/// spill here predates the read, and the refill must not give the frame compute past the cap.
#[test]
fn test_a_refill_after_the_read_does_not_lift_the_cap() {
    let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1));
    let code = op(code, TIMESTAMP).sstore(U256::from(1), U256::ZERO);
    let code = spin(code);
    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        tx(CALLER, CONTRACT, BELOW),
    );
    // The `POP`, then the restoring write — two pushes and `SSTORE`'s static gas, the whole of a
    // warm write that restores its slot's original value — then the loop.
    let left = Charges::default().then(&[2, 3, 3, 100]).spin(0).left(CAP);
    assert_stopped(&run, intrinsic(BELOW), left);
}

/// A chain's caps must be below [`MAX_TX_COMPUTE_GAS`], the most compute a transaction can
/// spend. The transaction that can spend it pays the least a frame that runs code can before its
/// first instruction — a call to its own sender, which EIP-2780 charges its base cost alone, whose
/// delegate, the block beneficiary, is warm — and is detained from its start, since its recipient
/// delegates to the beneficiary; above the execution cap its body's history is the reservoir's.
/// Its frame holds exactly the most compute, so a cap there never stops it, and it runs out of its
/// own gas; a cap below what it holds by more than any one charge of its loop stops it.
#[test]
fn test_the_most_compute_a_transaction_can_spend_bounds_the_caps() {
    use mega_evm::constants::MAX_TX_COMPUTE_GAS;
    let intrinsic = TX_GAS_LIMIT_CAP - MAX_TX_COMPUTE_GAS;
    let run_under = |cap: u64, code: Bytes| {
        let db = with_delegation(
            MemoryDatabase::default()
                .account_code(BENEFICIARY, code)
                .account_balance(DELEGATOR, U256::from(1_u64 << 60)),
            DELEGATOR,
            BENEFICIARY,
        );
        let limits = EvmTxRuntimeLimits::default()
            .with_block_env_access_compute_gas_limit(cap)
            .with_oracle_access_compute_gas_limit(cap);
        run_on(
            &mut MegaEvm::new(context(db).with_tx_runtime_limits(limits)),
            tx(DELEGATOR, DELEGATOR, ABOVE),
        )
    };

    // `GAS` answers what the frame holds, less its own 2.
    let answers_gas = BytecodeBuilder::default()
        .append(GAS)
        .append_many([PUSH0, MSTORE])
        .push_number(32_u8)
        .append_many([PUSH0, RETURN])
        .build();
    let run = run_under(MAX_TX_COMPUTE_GAS, answers_gas);
    assert_eq!(run.limit, Some(MAX_TX_COMPUTE_GAS), "detained from the start");
    let output = run.outcome.result.output().expect("the frame returns");
    assert_eq!(U256::from_be_slice(output), U256::from(MAX_TX_COMPUTE_GAS - 2));
    assert_eq!(run.outcome.gas.regular, intrinsic + 15, "the program's 15 is its only compute");

    let spinner = spin(BytecodeBuilder::default());
    let run = run_under(MAX_TX_COMPUTE_GAS, spinner.clone());
    assert_eq!(run.limit, Some(MAX_TX_COMPUTE_GAS));
    assert_eq!(run.outcome.limit_exceeded, None, "a cap at the most compute never stops it");
    assert!(
        matches!(run.outcome.result, ExecutionResult::Halt { .. }),
        "it runs out of its own gas: {:?}",
        run.outcome.result
    );

    let cap = MAX_TX_COMPUTE_GAS - 10_000;
    let run = run_under(cap, spinner);
    assert_eq!(assert_stopped(&run, intrinsic, Charges::default().spin(0).left(cap)), cap);
}
