//! What the scenarios are charged, worked out from the Satin schedule and the byte prices in
//! effect rather than read back from the engine.
//!
//! Every figure goes through the schedule's entries and the engine's price functions, so it holds
//! at any byte price a measurement build runs.

use mega_evm::{history_gas, satin_gas_params, tx_body_history_bytes, write_record_history_gas};
use revm::{
    context_interface::cfg::{gas::LOG, GasId},
    primitives::{eip2780, eip8038},
};

/// The schedule's entry `id`.
pub(crate) fn entry(id: GasId) -> u64 {
    satin_gas_params().get(id)
}

/// The history gas of `bytes` bytes.
pub(crate) fn history(bytes: u64) -> u64 {
    history_gas(bytes).expect("a byte count has a price")
}

/// The history gas of `records` write records.
pub(crate) fn records(records: u64) -> u64 {
    write_record_history_gas(records).expect("a record has a price")
}

/// The history gas of the body of a transaction carrying `calldata_len` bytes of calldata and no
/// access list or authorization.
pub(crate) fn body(calldata_len: usize) -> u64 {
    history(tx_body_history_bytes(calldata_len as u64, 0, 0, 0))
}

/// The state gas of one fresh storage slot, in the minimum bucket.
pub(crate) fn slot_state() -> u64 {
    entry(GasId::sstore_set_state_gas())
}

/// The regular gas of an `SSTORE` that fills a fresh, cold slot: its static cost, the cost of a
/// fresh slot before the load, and the cold load.
pub(crate) fn sstore_fresh_cold() -> u64 {
    entry(GasId::sstore_static()) +
        entry(GasId::sstore_set_without_load_cost()) +
        entry(GasId::cold_storage_cost())
}

/// The bytes in one EVM word.
const WORD: u64 = 32;

/// The ledgers a scenario's transaction is billed, worked out from the schedule, and the
/// EIP-7623 floor its receipt cannot fall below.
///
/// No scenario earns a refund. The floor raises the receipt and nothing else: where the three
/// ledgers sum to less, the receipt reports the floor, while each ledger still reports what was
/// spent on it. At the spec's prices the floor never binds, history gas outpricing it on the same
/// bytes; at a byte price low enough it does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ledgers {
    /// Regular gas: the intrinsic regular gas and what the frames ran.
    pub(crate) regular: u64,
    /// State gas the transaction kept.
    pub(crate) state: u64,
    /// History gas the transaction kept, its body included.
    pub(crate) history: u64,
    /// The EIP-7623 floor.
    pub(crate) floor: u64,
}

impl Ledgers {
    /// The raw spend: the three ledgers' sum.
    pub(crate) const fn spent(self) -> u64 {
        self.regular + self.state + self.history
    }

    /// What the receipt reports: the raw spend, at least the floor.
    pub(crate) const fn receipt(self) -> u64 {
        if self.spent() > self.floor {
            self.spent()
        } else {
            self.floor
        }
    }
}

/// The gas `data` costs as calldata: one token per zero byte and the schedule's multiplier per
/// non-zero byte, at the schedule's cost per token.
pub(crate) fn calldata(data: &[u8]) -> u64 {
    let multiplier = entry(GasId::tx_token_non_zero_byte_multiplier());
    let tokens: u64 = data.iter().map(|byte| if *byte == 0 { 1 } else { multiplier }).sum();
    tokens * entry(GasId::tx_token_cost())
}

/// The floor tokens of `data` (EIP-7623, as EIP-7976 amends it): the schedule's zero-byte
/// multiplier per zero byte and its non-zero multiplier per non-zero byte.
fn floor_tokens(data: &[u8]) -> u64 {
    let zero = entry(GasId::tx_floor_token_zero_byte_multiplier());
    let non_zero = entry(GasId::tx_token_non_zero_byte_multiplier());
    data.iter().map(|byte| if *byte == 0 { zero } else { non_zero }).sum()
}

/// The EIP-7623 floor of a call to another account carrying `data`: under EIP-2780 the decomposed
/// base the intrinsic gas starts from (the base, the recipient's access and the value's charge),
/// plus the calldata's floor tokens at the floor's cost per token.
pub(crate) fn call_floor(data: &[u8], carries_value: bool) -> u64 {
    let value = if carries_value { eip2780::TX_VALUE_COST } else { 0 };
    eip2780::TX_BASE_COST +
        eip8038::COLD_ACCOUNT_ACCESS +
        value +
        floor_tokens(data) * entry(GasId::tx_floor_cost_per_token())
}

/// The EIP-7623 floor of a creation running `init_code`: the base and the creation's access, plus
/// the init code's floor tokens at the floor's cost per token. EIP-3860's cost per word is not in
/// it.
pub(crate) fn create_floor(init_code: &[u8]) -> u64 {
    eip2780::TX_BASE_COST +
        entry(GasId::tx_create_access_cost()) +
        floor_tokens(init_code) * entry(GasId::tx_floor_cost_per_token())
}

/// The EIP-2780 intrinsic regular gas of a call to another account carrying `data`: the base,
/// the recipient's access, the value's charge when it carries value, and the calldata.
pub(crate) fn call_intrinsic(data: &[u8], carries_value: bool) -> u64 {
    let value = if carries_value { eip2780::TX_VALUE_COST } else { 0 };
    eip2780::TX_BASE_COST + eip8038::COLD_ACCOUNT_ACCESS + value + calldata(data)
}

/// The EIP-2780 intrinsic regular gas of a creation running `init_code`: the base, the creation's
/// access, EIP-3860's cost per word of init code, and the calldata.
pub(crate) fn create_intrinsic(init_code: &[u8]) -> u64 {
    eip2780::TX_BASE_COST +
        entry(GasId::tx_create_access_cost()) +
        satin_gas_params().initcode_cost(init_code.len()) +
        calldata(init_code)
}

/// The words `bytes` bytes occupy.
pub(crate) const fn words(bytes: u64) -> u64 {
    bytes.div_ceil(WORD)
}

/// What expanding memory from nothing to `words` words costs.
pub(crate) fn memory(words: u64) -> u64 {
    entry(GasId::memory_linear_cost()) * words +
        words * words / entry(GasId::memory_quadratic_reduction())
}

/// The regular gas of a `LOG` with `topics` topics and `len` bytes of data, its memory aside.
pub(crate) fn log(topics: u64, len: u64) -> u64 {
    LOG + entry(GasId::logtopic()) * topics + entry(GasId::logdata()) * len
}

/// The access of an account the transaction has not touched.
pub(crate) fn cold_account() -> u64 {
    entry(GasId::warm_storage_read_cost()) + entry(GasId::cold_account_additional_cost())
}

/// The access of an account already warm, such as a precompile.
pub(crate) fn warm_account() -> u64 {
    entry(GasId::warm_storage_read_cost())
}

/// The regular gas a creation's deposit of `len` bytes costs: the regular per-byte cost and the
/// hashing of the code.
pub(crate) fn deposit_regular(len: usize) -> u64 {
    entry(GasId::code_deposit_cost()) * len as u64 +
        entry(GasId::keccak256_per_word()) * words(len as u64)
}

/// Everything a creation's deposit of `len` bytes costs, which `return_create` charges after the
/// creation's last step: its regular part, the code's state gas and its history gas.
pub(crate) fn deposit(len: usize) -> u64 {
    deposit_regular(len) + satin_gas_params().code_deposit_state_gas(len) + history(len as u64)
}

/// The state gas of a created account and `len` bytes of code deposited in it.
pub(crate) fn created_state(len: usize) -> u64 {
    entry(GasId::create_state_gas()) + satin_gas_params().code_deposit_state_gas(len)
}

/// The state gas of one new account, in the minimum bucket.
pub(crate) fn account_state() -> u64 {
    entry(GasId::new_account_state_gas())
}

/// The compute a frame that loops on `cycle` reaches before a charge would take it past `limit`,
/// starting from `compute`: gas detention stops it on the first charge its spendable gas cannot
/// pay, without making it.
pub(crate) fn compute_at_crossing(mut compute: u64, limit: u64, cycle: &[u64]) -> u64 {
    for charge in cycle.iter().cycle() {
        if compute + charge > limit {
            return compute;
        }
        compute += charge;
    }
    unreachable!("the cycle never ends")
}
