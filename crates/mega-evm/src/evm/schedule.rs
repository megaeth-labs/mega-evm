//! The Satin gas schedule.
//!
//! Satin takes the Amsterdam schedule — the one that carries EIP-8037 state gas, EIP-8038's
//! state-access prices, the EIP-2780 intrinsic decomposition, the EIP-7976 calldata floor and
//! EIP-7981 access-list data — and changes exactly two things:
//!
//! 1. the EIP-8037 state-gas entries are rebuilt from `MegaETH`'s own cost per state byte
//!    ([`STATE_GAS_REPRICED`]), which is what makes the state dimension `MegaETH`'s rather than
//!    Glamsterdam's;
//! 2. the history price of deposited code ([`HISTORY_GAS_PRICED`]), the one entry outside
//!    Amsterdam's table: upstream leaves it at zero, and Satin gives it the cost of one history
//!    byte, which is what makes a deployment pay for the bytes every node has to carry.
//!
//! Every regular-gas entry is Amsterdam's, so state creation is split between the two dimensions
//! as Amsterdam splits it: the regular part is the access and write work EIP-8038 prices, and the
//! new state itself is state gas alone. A value `CALL` that creates its recipient pays the 11,300
//! of its value transfer, which carries the account write, and no new-account regular gas; a
//! `CREATE` opcode pays 12,000; a slot's first write 10,000 above its load; against 183,600,
//! 183,600 and 97,920 of state gas at the minimum bucket.
//!
//! The static price every opcode pays before its dynamic cost is not in the schedule: revm keeps it
//! in the instruction table, keyed on the spec. Satin's is Amsterdam's too ([`satin_gas_table`]),
//! which differs from its Osaka base's in EIP-8038's two-read base for `EXTCODESIZE` and
//! `EXTCODECOPY` alone.
//!
//! The state-gas entries are the price at the *minimum* SALT bucket. A charge is scaled from
//! there by the capacity of the bucket it lands in, which the pricing hook does
//! (`Host::state_gas_price`); the schedule holds the number that scaling starts from.
//!
//! The schedule is built once for the process and handed to every configuration as a shared
//! clone ([`MegaContext::with_cfg`](crate::MegaContext)); an opcode reads its price out of the
//! table revm already carries. Nothing here runs on the hot path.

use revm::{
    context_interface::cfg::{GasId, GasParams},
    interpreter::instructions::{gas_table_spec, GasTable},
    primitives::{eip8037, hardfork::SpecId, OnceLock},
};

use crate::evm::prices::{active_satin_prices, SatinPrices};

/// A schedule entry and the number of bytes it prices: the entry is that many bytes at a byte
/// price.
type ScheduleEntry = (fn() -> GasId, u64);

/// The EIP-8037 state-gas entries, rebuilt at `MegaETH`'s own cost per state byte.
///
/// Each entry is a byte count times the cost per state byte. The byte counts are EIP-8037's —
/// what a leaf, a storage slot or a delegation indicator actually occupies — and `MegaETH` keeps
/// them; only the price of a byte is `MegaETH`'s, which is the whole of the divergence from the
/// Amsterdam schedule here.
pub const STATE_GAS_REPRICED: &[ScheduleEntry] = &[
    (GasId::sstore_set_state_gas, eip8037::SSTORE_SET_BYTES),
    (GasId::new_account_state_gas, eip8037::NEW_ACCOUNT_BYTES),
    (GasId::create_state_gas, eip8037::NEW_ACCOUNT_BYTES),
    (GasId::code_deposit_state_gas, eip8037::CODE_DEPOSIT_PER_BYTE),
    (GasId::tx_eip7702_state_gas_bytecode, eip8037::AUTH_BASE_BYTES),
];

/// The history-gas entries, rebuilt at `MegaETH`'s own cost per history byte.
///
/// Deposited code is the one history charge the schedule prices, because it is the one revm makes
/// itself: `return_create` writes the code and charges the entry for its length. Every other
/// history charge is made by the engine, which prices its own byte counts (the `history` module).
pub const HISTORY_GAS_PRICED: &[ScheduleEntry] = &[(GasId::code_deposit_history_gas, 1)];

/// The Satin gas schedule at the prices in effect.
///
/// See the module documentation for what it changes; [`satin_gas_params_at`] is the same schedule
/// with the byte prices taken as given rather than read from the process.
///
/// Built once and handed out as a shared clone, the way revm hands out its own spec schedules:
/// the prices are fixed for the process, so every configuration gets the same table.
pub fn satin_gas_params() -> GasParams {
    static TABLE: OnceLock<GasParams> = OnceLock::new();
    TABLE.get_or_init(|| satin_gas_params_at(active_satin_prices())).clone()
}

/// The Satin gas schedule with no price on a history byte, for a transaction exempt from history
/// gas.
///
/// The one history charge revm makes itself reads the schedule, so the way to keep it off an
/// exempt transaction is to hand that transaction a schedule that prices it at zero. Built once
/// and handed out as a shared clone, like [`satin_gas_params`]; every other entry is the same.
pub fn satin_gas_params_history_exempt() -> GasParams {
    static TABLE: OnceLock<GasParams> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            let mut params = satin_gas_params();
            params.override_gas(HISTORY_GAS_PRICED.iter().map(|&(id, _)| (id(), 0)));
            params
        })
        .clone()
}

/// The Satin static opcode prices: Amsterdam's.
///
/// Amsterdam's table differs from the Osaka base's in EIP-8038's base for `EXTCODESIZE` and
/// `EXTCODECOPY` alone: two warm accesses, 200, because each reads the account and then its code.
/// The dynamic cold premium is the schedule's, as for every account access.
pub(crate) const fn satin_gas_table() -> GasTable {
    gas_table_spec(SpecId::AMSTERDAM)
}

/// The Satin gas schedule at `prices`.
pub fn satin_gas_params_at(prices: SatinPrices) -> GasParams {
    let mut params = GasParams::new_spec(SpecId::AMSTERDAM);
    params.override_gas(
        STATE_GAS_REPRICED.iter().map(|&(id, bytes)| (id(), prices.cpsb.fixed_size_gas(bytes))),
    );
    params.override_gas(
        HISTORY_GAS_PRICED.iter().map(|&(id, bytes)| (id(), prices.cphb.fixed_size_gas(bytes))),
    );
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        constants::{
            ACCOUNT_STATE_GAS, COST_PER_HISTORY_BYTE, COST_PER_STATE_BYTE, SLOT_STATE_GAS,
        },
        evm::prices::{runs_at_measurement_prices, BytePrice},
    };

    fn osaka() -> GasParams {
        GasParams::new_spec(SpecId::OSAKA)
    }

    fn amsterdam() -> GasParams {
        GasParams::new_spec(SpecId::AMSTERDAM)
    }

    /// Every regular-gas entry is Amsterdam's: every entry but the byte-priced ones. The
    /// state-gas entries are rebuilt from the cost per state byte rather than copied, so they are
    /// checked separately; at the provisional price they land on Amsterdam's numbers too, which
    /// [`test_other_prices_move_only_the_byte_priced_entries`] shows is a coincidence of the
    /// price and not a copy.
    #[test]
    fn test_every_regular_entry_is_amsterdam() {
        let (satin, amsterdam) = (satin_gas_params(), amsterdam());
        for slot in 0..=u8::MAX {
            let id = GasId::new(slot);
            let rebuilt = STATE_GAS_REPRICED.iter().any(|&(entry, _)| entry() == id) ||
                HISTORY_GAS_PRICED.iter().any(|&(entry, _)| entry() == id);
            if rebuilt {
                continue;
            }
            assert_eq!(satin.get(id), amsterdam.get(id), "{} must be Amsterdam's", id.name());
        }
        // The entries Amsterdam introduced for EIP-2780, EIP-7976 and EIP-7981, and the zero
        // regular code-deposit cost, spelled out beside the EIP-8038 ones below.
        for (name, value) in [
            ("tx_account_write_cost", 9_000),
            ("tx_create_access_cost", 12_000),
            ("code_deposit_cost", 0),
            ("tx_floor_cost_base_gas", 12_000),
            ("tx_floor_cost_per_token", 16),
            ("tx_floor_token_zero_byte_multiplier", 4),
            ("tx_access_list_floor_byte_multiplier", 4),
        ] {
            let id = GasId::from_name(name).expect("a known gas id");
            assert_eq!(satin.get(id), value, "{name}");
        }
    }

    /// The seventeen regular-gas entries Amsterdam repriced for EIP-8038, and for EIP-8037's move
    /// of the cost of creating state onto the state dimension, at their Osaka and their Satin
    /// (Amsterdam) numbers, written out so a change in either base schedule shows up here rather
    /// than silently moving Satin.
    ///
    /// The Amsterdam numbers are EIP-8038's constants (glamsterdam devnet-8): a cold account
    /// access of 3,000 (2,900 above the warm 100), an account write of 9,000 (with the 2,300
    /// stipend, a value transfer of 11,300), a cold slot access of 2,100 with the warm base folded
    /// in, a storage write of 10,000, a clearing refund of 11,616, a creation access of 12,000,
    /// and access-list items at the cold premium plus EIP-7981's 64 gas per byte.
    #[test]
    fn test_the_eip8038_entries_carry_the_amsterdam_numbers() {
        let satin = satin_gas_params();
        for (name, osaka_value, satin_value) in [
            ("warm_storage_read_cost", 100, 100),
            ("cold_account_additional_cost", 2_500, 2_900),
            ("cold_storage_additional_cost", 2_000, 2_000),
            ("cold_storage_cost", 2_100, 2_000),
            ("transfer_value_cost", 9_000, 11_300),
            ("new_account_cost", 25_000, 0),
            ("new_account_cost_for_selfdestruct", 25_000, 9_000),
            ("sstore_static", 100, 100),
            ("sstore_set_without_load_cost", 19_900, 10_000),
            ("sstore_reset_without_cold_load_cost", 2_800, 10_000),
            ("sstore_set_refund", 19_900, 10_000),
            ("sstore_reset_refund", 2_800, 10_000),
            ("sstore_clearing_slot_refund", 4_800, 11_616),
            ("create", 32_000, 12_000),
            ("tx_create_cost", 32_000, 12_000),
            ("tx_access_list_address_cost", 2_400, 2_900 + 20 * 64),
            ("tx_access_list_storage_key_cost", 1_900, 2_000 + 32 * 64),
        ] {
            let id = GasId::from_name(name).expect("a known gas id");
            assert_eq!(osaka().get(id), osaka_value, "{name} at Osaka");
            assert_eq!(satin.get(id), satin_value, "{name} at Satin");
        }
    }

    /// The static opcode prices are Amsterdam's, which part from the Osaka base's in EIP-8038's
    /// two-read base for `EXTCODESIZE` and `EXTCODECOPY` alone.
    #[test]
    fn test_the_static_prices_are_amsterdams() {
        use revm::bytecode::opcode::{EXTCODECOPY, EXTCODESIZE};
        let (satin, osaka) = (satin_gas_table(), gas_table_spec(SpecId::OSAKA));
        let changed: Vec<_> =
            (0..=u8::MAX).filter(|&op| satin[op as usize] != osaka[op as usize]).collect();
        assert_eq!(changed, vec![EXTCODESIZE, EXTCODECOPY]);
        for opcode in changed {
            assert_eq!(osaka[opcode as usize], 100, "opcode {opcode:#04x} at Osaka");
            assert_eq!(satin[opcode as usize], 200, "opcode {opcode:#04x} at Satin");
        }
    }

    /// The schedule an exempt transaction runs prices no history byte and is otherwise the same
    /// table.
    #[test]
    fn test_the_exempt_schedule_only_drops_the_history_price() {
        let exempt = satin_gas_params_history_exempt();
        let satin = satin_gas_params();
        assert_eq!(exempt.get(GasId::code_deposit_history_gas()), 0);
        for slot in 0..=u8::MAX {
            let id = GasId::new(slot);
            if HISTORY_GAS_PRICED.iter().any(|&(entry, _)| entry() == id) {
                continue;
            }
            assert_eq!(exempt.get(id), satin.get(id), "{} must not move", id.name());
        }
    }

    /// Deposited code costs one history byte per byte, which is the whole of Satin's history
    /// schedule. Osaka and Amsterdam price no history byte at all.
    #[test]
    fn test_deposited_code_is_priced_per_history_byte() {
        let id = GasId::code_deposit_history_gas();
        let satin = satin_gas_params_at(SatinPrices::CONSTANTS);
        assert_eq!(satin.get(id), COST_PER_HISTORY_BYTE);
        assert_eq!(satin.get(id), 88);
        assert_eq!(osaka().get(id), 0);
        assert_eq!(amsterdam().get(id), 0);
    }

    /// The state entries are EIP-8037's byte counts at `MegaETH`'s price. Dividing an entry by
    /// the price recovers the byte count the EIP names, which is the property that survives a
    /// repricing.
    ///
    /// Built at the constants rather than read off the process, so the numbers below hold in a
    /// measurement build too; that the process runs this schedule is
    /// [`test_the_schedule_at_the_active_prices_is_the_schedule_at_the_constants`].
    #[test]
    fn test_state_entries_recover_the_eip8037_byte_counts() {
        let satin = satin_gas_params_at(SatinPrices::CONSTANTS);
        for &(id, bytes) in STATE_GAS_REPRICED {
            let id = id();
            assert_eq!(satin.get(id), bytes * COST_PER_STATE_BYTE, "{} at the price", id.name());
            assert_eq!(satin.get(id) / COST_PER_STATE_BYTE, bytes, "{} in bytes", id.name());
        }
        assert_eq!(satin.get(GasId::sstore_set_state_gas()), SLOT_STATE_GAS);
        assert_eq!(satin.get(GasId::sstore_set_state_gas()), 97_920);
        assert_eq!(satin.get(GasId::new_account_state_gas()), ACCOUNT_STATE_GAS);
        assert_eq!(satin.get(GasId::new_account_state_gas()), 183_600);
        assert_eq!(satin.get(GasId::create_state_gas()), 183_600);
        assert_eq!(satin.get(GasId::code_deposit_state_gas()), 1_530);
        assert_eq!(satin.get(GasId::tx_eip7702_state_gas_bytecode()), 35_190);
    }

    /// Other prices move the five state entries and the history entry, and nothing else, which
    /// is what makes a measurement build a repricing rather than a different schedule.
    #[test]
    fn test_other_prices_move_only_the_byte_priced_entries() {
        let constants = satin_gas_params_at(SatinPrices::CONSTANTS);
        let prices = SatinPrices { cpsb: "312.5".parse().unwrap(), cphb: BytePrice::from_gas(400) };
        let repriced = satin_gas_params_at(prices);
        let expected = [
            (GasId::sstore_set_state_gas(), 20_000),
            (GasId::new_account_state_gas(), 37_500),
            (GasId::create_state_gas(), 37_500),
            (GasId::code_deposit_state_gas(), 313),
            (GasId::tx_eip7702_state_gas_bytecode(), 7_188),
            (GasId::code_deposit_history_gas(), 400),
        ];
        for slot in 0..=u8::MAX {
            let id = GasId::new(slot);
            match expected.iter().find(|(moved, _)| *moved == id) {
                Some((_, value)) => assert_eq!(repriced.get(id), *value, "{}", id.name()),
                None => {
                    assert_eq!(repriced.get(id), constants.get(id), "{} must not move", id.name())
                }
            }
        }
    }

    /// The prices in effect are the constants, so the schedule the engine installs is the one
    /// built at them. A measurement build is the one case where they part, and it skips.
    #[test]
    fn test_the_schedule_at_the_active_prices_is_the_schedule_at_the_constants() {
        if runs_at_measurement_prices() {
            return;
        }
        assert_eq!(satin_gas_params().table(), satin_gas_params_at(SatinPrices::CONSTANTS).table());
    }
}
