//! What a byte of history costs, and how many bytes each site appends.
//!
//! History gas is the third ledger of a Satin transaction. It pays for the bytes the transaction
//! appends to the chain's history — its own body, its logs, the code it deploys and one record per
//! account or storage write it keeps — as opposed to the bytes it adds to the world state, which
//! EIP-8037 state gas pays for. Both draw on the same reservoir-first budget and unwind on the same
//! paths; they are counted apart so a node can report and limit them separately.
//!
//! Every charge is a byte count from the byte table (the `limit` module) times the cost per history
//! byte, and the byte counts are the ones the data-size limit meters, so a log costs history for
//! exactly the bytes that limit counts it at. Nothing here reads a SALT bucket: history bytes live
//! in no bucket, and the multiplier that scales the state dimension never touches this one.
//!
//! The one history charge that is not made here is the deployed code's: it is a schedule entry
//! ([`code_deposit_history_gas`](revm::context_interface::cfg::GasId::code_deposit_history_gas)),
//! so revm charges it where it writes the code. The schedule gives that entry the cost of one byte
//! (see the `schedule` module), which is what switches it on.

use revm::context::{transaction::AccessListItemTr, Transaction};

use crate::{
    evm::prices::active_satin_prices,
    limit::{
        ACCESS_LIST_ADDRESS_SIZE, ACCESS_LIST_SLOT_SIZE, AUTHORIZATION_SIZE, LOG_BASE_SIZE,
        LOG_TOPIC_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
    },
};

/// The gas `bytes` bytes of history cost, or `None` when the price does not fit in a `u64`.
///
/// A byte count that has no price is not a charge anyone can pay: the caller turns it into an
/// out-of-gas, which is what a count that large would be anyway.
#[inline]
pub fn history_gas(bytes: u64) -> Option<u64> {
    active_satin_prices().cphb.gas_for(bytes)
}

/// The gas `records` account or storage write records cost as history.
#[inline]
pub fn write_record_history_gas(records: u64) -> Option<u64> {
    history_gas(WRITE_RECORD_SIZE.checked_mul(records)?)
}

/// The history bytes a `LOG` with `topics` topics and `data_len` bytes of data appends: its own
/// record for the address that emitted it, one per topic, and the data.
#[inline]
pub const fn log_history_bytes(topics: u64, data_len: u64) -> u64 {
    LOG_BASE_SIZE.saturating_add(LOG_TOPIC_SIZE.saturating_mul(topics)).saturating_add(data_len)
}

/// The history bytes the allowance of a value-transferring call covers: one three-topic event
/// carrying a single word, which is what a `receive()` hook emits.
pub const STORAGE_CALL_STIPEND_BYTES: u64 = LOG_BASE_SIZE + 3 * LOG_TOPIC_SIZE + 32;

/// The history allowance a value-transferring `CALL` or `CALLCODE` grants the frame it starts.
///
/// EVM's own `CALL_STIPEND` buys the recipient of a transfer enough computation to notice it; on
/// a chain that prices the bytes a log appends it buys no log at all, so a `receive()` hook that
/// emits an event would be unreachable through `transfer()`. The allowance is that stipend's
/// counterpart on the history ledger: [`STORAGE_CALL_STIPEND_BYTES`] at the cost per history
/// byte, enough for one event and nothing else.
///
/// It is separate from the frame's gas in every sense. It never enters the frame's `Gas`, so no
/// settlement can hand it back as gas; it pays history charges and only those, so it cannot buy
/// computation or a write record; and what it pays for appears on no ledger, because no pool of
/// the transaction's gas paid it.
pub fn storage_call_stipend() -> u64 {
    history_gas(STORAGE_CALL_STIPEND_BYTES).unwrap_or(u64::MAX)
}

/// The data-size bytes of `tx`'s body: the fixed body, the calldata, one record per EIP-7702
/// authorization and the access list. The same count history gas prices.
///
/// A zero calldata byte counts the same as a non-zero one. The count saturates.
pub fn transaction_body_bytes(tx: &impl Transaction) -> u64 {
    let (addresses, slots) = tx
        .access_list()
        .map(|items| {
            items.fold((0_u64, 0_u64), |(addresses, slots), item| {
                (addresses + 1, slots + item.storage_slots().count() as u64)
            })
        })
        .unwrap_or_default();
    tx_body_history_bytes(
        tx.input().len() as u64,
        tx.authorization_list_len() as u64,
        addresses,
        slots,
    )
}

/// The history bytes a transaction's body appends before it runs: the fixed body
/// ([`TX_BODY_SIZE`]), the calldata, one record per EIP-7702 authorization, and the access list at
/// the size of its addresses and keys.
///
/// A zero calldata byte costs the same as a non-zero one: both occupy the same space in a block.
/// The count saturates, and a saturated count has no price, so a transaction that would reach it
/// is rejected for not covering its own intrinsic gas.
#[inline]
pub const fn tx_body_history_bytes(
    calldata_len: u64,
    authorizations: u64,
    access_list_addresses: u64,
    access_list_slots: u64,
) -> u64 {
    TX_BODY_SIZE
        .saturating_add(calldata_len)
        .saturating_add(AUTHORIZATION_SIZE.saturating_mul(authorizations))
        .saturating_add(ACCESS_LIST_ADDRESS_SIZE.saturating_mul(access_list_addresses))
        .saturating_add(ACCESS_LIST_SLOT_SIZE.saturating_mul(access_list_slots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{constants::COST_PER_HISTORY_BYTE, evm::prices::runs_at_measurement_prices};

    /// The byte counts of the three sites that count bytes rather than records.
    #[test]
    fn test_the_byte_counts_follow_the_byte_table() {
        assert_eq!(log_history_bytes(0, 0), LOG_BASE_SIZE);
        assert_eq!(log_history_bytes(3, 32), LOG_BASE_SIZE + 3 * LOG_TOPIC_SIZE + 32);
        assert_eq!(log_history_bytes(3, 32), 160);
        assert_eq!(log_history_bytes(1, u64::MAX), u64::MAX, "the count saturates");

        assert_eq!(tx_body_history_bytes(0, 0, 0, 0), TX_BODY_SIZE);
        assert_eq!(tx_body_history_bytes(0, 0, 0, 0), 310);
        assert_eq!(
            tx_body_history_bytes(100, 2, 3, 4),
            TX_BODY_SIZE + 100 + 2 * AUTHORIZATION_SIZE + 3 * 20 + 4 * 32
        );
        assert_eq!(tx_body_history_bytes(u64::MAX, 1, 0, 0), u64::MAX, "the count saturates");
    }

    /// The allowance is one three-topic event carrying one word, at the price of a history byte.
    #[test]
    fn test_the_allowance_is_one_three_topic_event() {
        if runs_at_measurement_prices() {
            return;
        }
        assert_eq!(STORAGE_CALL_STIPEND_BYTES, log_history_bytes(3, 32));
        assert_eq!(STORAGE_CALL_STIPEND_BYTES, 160);
        assert_eq!(storage_call_stipend(), 160 * COST_PER_HISTORY_BYTE);
        assert_eq!(storage_call_stipend(), history_gas(log_history_bytes(3, 32)).unwrap());
    }

    /// Every way a transaction moves value, as a name, a database, the transaction at a gas limit
    /// and the transfer logs it keeps.
    #[allow(clippy::type_complexity)]
    fn value_movements() -> Vec<(
        &'static str,
        crate::test_utils::MemoryDatabase,
        fn(u64) -> crate::MegaTransaction,
        u64,
    )> {
        use crate::test_utils::{BytecodeBuilder, MemoryDatabase};
        use alloy_primitives::{address, Address, Bytes, TxKind, U256};
        use revm::bytecode::opcode::{
            CALL, CREATE, CREATE2, GAS, LOG3, POP, PUSH0, PUSH1, REVERT, SELFDESTRUCT, STOP,
        };

        const CALLER: Address = address!("00000000000000000000000000000000000c0001");
        const ACTOR: Address = address!("00000000000000000000000000000000000c0002");
        const RECEIVER: Address = address!("00000000000000000000000000000000000c0003");
        const IDENTITY: Address = address!("0000000000000000000000000000000000000004");

        fn tx(to: TxKind, value: u64, gas_limit: u64) -> crate::MegaTransaction {
            alloy_op_evm::OpTx(crate::test_utils::op_transaction(revm::context::TxEnv {
                caller: CALLER,
                kind: to,
                value: U256::from(value),
                gas_limit,
                ..Default::default()
            }))
        }
        let calling = |target: Address, gas: Option<u64>| {
            let code = BytecodeBuilder::default()
                .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
                .append(PUSH1)
                .append(1)
                .push_address(target);
            let code = match gas {
                Some(gas) => code.push_number(gas),
                None => code.append(GAS),
            };
            code.append(CALL).append(POP)
        };
        let funded = || {
            MemoryDatabase::default()
                .account_balance(CALLER, U256::from(10u64.pow(18)))
                .account_balance(ACTOR, U256::from(1_000))
        };
        let actor = |code: BytecodeBuilder| funded().account_code(ACTOR, code.stop().build());
        let event = BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0])
            .push_number(32_u8)
            .append(PUSH0)
            .append(LOG3)
            .stop()
            .build();
        let endowing = |opcode| {
            let code = BytecodeBuilder::default();
            let code = if opcode == CREATE2 { code.append(PUSH0) } else { code };
            code.append_many([PUSH0, PUSH0, PUSH1, 1, opcode, POP])
        };
        vec![
            ("the transaction's value", funded(), |gas| tx(TxKind::Call(RECEIVER), 5, gas), 1),
            ("a creation transaction's endowment", funded(), |gas| tx(TxKind::Create, 5, gas), 1),
            (
                "a value CALL revm builds a frame for",
                actor(calling(RECEIVER, None)).account_code(RECEIVER, Bytes::from_static(&[STOP])),
                |gas| tx(TxKind::Call(ACTOR), 0, gas),
                1,
            ),
            (
                "a value CALL revm answers, to an account with no code",
                actor(calling(RECEIVER, None)),
                |gas| tx(TxKind::Call(ACTOR), 0, gas),
                1,
            ),
            (
                "a value CALL to a precompile",
                actor(calling(IDENTITY, None)),
                |gas| tx(TxKind::Call(ACTOR), 0, gas),
                1,
            ),
            (
                "a CREATE's and a CREATE2's endowments",
                actor(BytecodeBuilder::default().append_many(
                    endowing(CREATE).append_many(endowing(CREATE2).build_vec()).build_vec(),
                )),
                |gas| tx(TxKind::Call(ACTOR), 0, gas),
                2,
            ),
            (
                "a SELFDESTRUCT's balance",
                funded().account_code(
                    ACTOR,
                    BytecodeBuilder::default().push_address(RECEIVER).append(SELFDESTRUCT).build(),
                ),
                |gas| tx(TxKind::Call(ACTOR), 0, gas),
                1,
            ),
            (
                "a transfer passed on to a hook whose event its allowance pays",
                actor(calling(RECEIVER, Some(2_300))).account_code(RECEIVER, event),
                |gas| tx(TxKind::Call(ACTOR), 7, gas),
                2,
            ),
            (
                "a value CALL whose callee reverts",
                actor(calling(RECEIVER, None))
                    .account_code(RECEIVER, Bytes::from_static(&[PUSH0, PUSH0, REVERT])),
                |gas| tx(TxKind::Call(ACTOR), 0, gas),
                0,
            ),
        ]
    }

    /// A transfer log costs nothing: every way a transaction moves value spends the same gas on
    /// every ledger with EIP-7708 switched on as with it off, reports the same history bytes and
    /// write records, and ends in the same state — below the execution cap and above it. What
    /// differs is the log in the receipt, beside the events the contracts emitted, and its 160
    /// bytes of data size, one per movement the transaction keeps. The allowance a value call
    /// grants pays for its callee's event either way: the transfer log draws nothing from it.
    #[test]
    fn test_a_transfer_log_costs_no_gas_on_any_ledger() {
        use crate::{
            constants::TX_GAS_LIMIT_CAP,
            test_utils::{is_transfer_log, zero_fee_l1_block_info},
            MegaContext, MegaEvm, MegaSpecId, TRANSFER_LOG_SIZE,
        };
        use revm::context::BlockEnv;

        let block = BlockEnv { gas_limit: 10_000_000_000, ..Default::default() };
        for (name, db, tx, moves) in value_movements() {
            for gas_limit in [20_000_000, TX_GAS_LIMIT_CAP + 100_000_000] {
                let context = || {
                    MegaContext::new(db.clone(), MegaSpecId::SATIN)
                        .with_block(block.clone())
                        .with_chain(zero_fee_l1_block_info())
                };
                let run = |ctx| MegaEvm::new(ctx).execute_transaction(tx(gas_limit)).unwrap();
                let on = run(context());
                let off = run(context().without_transfer_logs());
                let case = format!("{name} at {gas_limit}");

                assert!(on.result.is_success(), "{case}: {:?}", on.result);
                assert_eq!(on.gas, off.gas, "{case}: every ledger");
                assert_eq!(on.state, off.state, "{case}: the state");
                assert_eq!(on.usage.write_records, off.usage.write_records, "{case}: the records");
                assert_eq!(
                    on.usage.data_size,
                    off.usage.data_size + moves * TRANSFER_LOG_SIZE,
                    "{case}: the data size",
                );
                let (logs, events): (Vec<_>, Vec<_>) =
                    on.result.logs().iter().cloned().partition(is_transfer_log);
                assert_eq!(logs.len() as u64, moves, "{case}: the transfer logs");
                assert_eq!(events, off.result.logs(), "{case}: the events");
                assert!(!off.result.logs().iter().any(is_transfer_log), "{case}: none without");
            }
        }
    }

    /// A charge is its byte count at the cost per history byte, and a count with no price
    /// reports none.
    #[test]
    fn test_a_charge_is_its_byte_count_at_the_price() {
        if runs_at_measurement_prices() {
            return;
        }
        assert_eq!(history_gas(0), Some(0));
        assert_eq!(history_gas(1), Some(COST_PER_HISTORY_BYTE));
        assert_eq!(history_gas(160), Some(160 * COST_PER_HISTORY_BYTE));
        assert_eq!(history_gas(u64::MAX), None, "an overflowing count has no price");

        assert_eq!(write_record_history_gas(0), Some(0));
        assert_eq!(write_record_history_gas(1), Some(WRITE_RECORD_SIZE * COST_PER_HISTORY_BYTE));
        assert_eq!(
            write_record_history_gas(3),
            Some(3 * WRITE_RECORD_SIZE * COST_PER_HISTORY_BYTE)
        );
        assert_eq!(write_record_history_gas(u64::MAX), None, "the byte count overflows");
    }
}
