//! The Satin gas schedule.
//!
//! Satin takes the Amsterdam schedule — the one that carries EIP-8037 state gas, the EIP-2780
//! intrinsic decomposition, the EIP-7976 calldata floor and EIP-7981 access-list data — and
//! changes exactly two things:
//!
//! 1. every regular-gas entry the Amsterdam table changed for EIP-8037 or EIP-8038 goes back to its
//!    Osaka value ([`EIP8038_REPRICED`]) — every one but the state-gas, EIP-2780, floor and
//!    EIP-7702 entries, which stay Amsterdam's;
//! 2. the EIP-8037 state-gas entries are rebuilt from `MegaETH`'s own cost per state byte
//!    ([`STATE_GAS_REPRICED`]), which is what makes the state dimension `MegaETH`'s rather than
//!    Glamsterdam's.
//!
//! The first group includes the four entries EIP-8037 moved onto the state dimension, so at these
//! prices state creation is charged on both dimensions: a value `CALL` that creates its recipient
//! pays 25,000 regular and 183,600 state, a `CREATE` opcode 32,000 and 183,600, a slot's first
//! write 22,100 and 97,920, where the Amsterdam schedule charges 0, 12,000 and 12,100 regular
//! against the same state charge. Whether creation keeps its Osaka regular price is an open
//! pricing question; the schedule states the prices, it does not settle them.
//!
//! The state-gas entries are the price at the *minimum* SALT bucket. A charge is scaled from
//! there by the capacity of the bucket it lands in, which the pricing hook does
//! (`Host::state_gas_price`); the schedule holds the number that scaling starts from.
//!
//! The one entry outside Amsterdam's table is the history price of deposited code
//! ([`HISTORY_GAS_PRICED`]): upstream leaves it at zero, and Satin gives it the cost of one
//! history byte, which is what makes a deployment pay for the bytes every node has to carry.
//!
//! Every other entry is Amsterdam's, including the EIP-2780 decomposition entries
//! (`tx_account_write_cost`, `tx_create_access_cost`), the zero `code_deposit_cost` and the floor.
//!
//! The schedule is built once for the process and handed to every configuration as a shared
//! clone ([`MegaContext::with_cfg`](crate::MegaContext)); an opcode reads its price out of the
//! table revm already carries. Nothing here runs on the hot path.

use revm::{
    context_interface::cfg::{GasId, GasParams},
    primitives::{eip8037, hardfork::SpecId, OnceLock},
};

use crate::evm::prices::{active_satin_prices, SatinPrices};

/// The regular-gas entries Amsterdam repriced for EIP-8037 and EIP-8038, which Satin keeps at
/// their Osaka values.
///
/// Amsterdam moves them for two reasons. EIP-8038 raises the price of *reaching* state — a cold
/// account, a cold slot, a storage write. EIP-8037 lowers the regular price of *creating* state,
/// having moved that charge onto the state dimension; `new_account_cost`, `create`,
/// `tx_create_cost` and `sstore_set_without_load_cost` are that second group. Satin takes neither
/// move, so state creation keeps its Osaka regular price on top of the state charge at
/// [`COST_PER_STATE_BYTE`](crate::constants::COST_PER_STATE_BYTE). The list is named for EIP-8038
/// because that is the repricing it was drawn up against.
///
/// `tx_create_cost` is the one of those four that prices nothing: under EIP-2780 the intrinsic
/// phase charges a creation through `tx_create_access_cost`, so pressing it back moves no
/// transaction. What the other three cost is in the module documentation.
///
/// The entries Amsterdam introduced for EIP-8037's state gas, EIP-2780, EIP-7976, EIP-7981 and
/// EIP-7702 are not in this list and keep their Amsterdam values, even where the number itself is
/// derived from an EIP-8038 constant.
pub const EIP8038_REPRICED: &[fn() -> GasId] = &[
    GasId::warm_storage_read_cost,
    GasId::cold_account_additional_cost,
    GasId::cold_storage_additional_cost,
    GasId::cold_storage_cost,
    GasId::transfer_value_cost,
    GasId::new_account_cost,
    GasId::new_account_cost_for_selfdestruct,
    GasId::sstore_static,
    GasId::sstore_set_without_load_cost,
    GasId::sstore_reset_without_cold_load_cost,
    GasId::sstore_set_refund,
    GasId::sstore_reset_refund,
    GasId::sstore_clearing_slot_refund,
    GasId::create,
    GasId::tx_create_cost,
    GasId::tx_access_list_address_cost,
    GasId::tx_access_list_storage_key_cost,
];

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

/// The Satin gas schedule at `prices`.
pub fn satin_gas_params_at(prices: SatinPrices) -> GasParams {
    let osaka = GasParams::new_spec(SpecId::OSAKA);
    let mut params = GasParams::new_spec(SpecId::AMSTERDAM);
    params.override_gas(EIP8038_REPRICED.iter().map(|id| {
        let id = id();
        (id, osaka.get(id))
    }));
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

    /// The seventeen entries Amsterdam repriced are back at their Osaka values, written out so a
    /// change to the list is a visible diff.
    #[test]
    fn test_the_eip8038_repricing_is_pressed_back_to_osaka() {
        let satin = satin_gas_params();
        let names = [
            "warm_storage_read_cost",
            "cold_account_additional_cost",
            "cold_storage_additional_cost",
            "cold_storage_cost",
            "transfer_value_cost",
            "new_account_cost",
            "new_account_cost_for_selfdestruct",
            "sstore_static",
            "sstore_set_without_load_cost",
            "sstore_reset_without_cold_load_cost",
            "sstore_set_refund",
            "sstore_reset_refund",
            "sstore_clearing_slot_refund",
            "create",
            "tx_create_cost",
            "tx_access_list_address_cost",
            "tx_access_list_storage_key_cost",
        ];
        assert_eq!(EIP8038_REPRICED.len(), names.len(), "the list and its names agree");
        for (id, name) in EIP8038_REPRICED.iter().zip(names) {
            assert_eq!(id().name(), name);
        }
        for id in EIP8038_REPRICED {
            let id = id();
            assert_eq!(satin.get(id), osaka().get(id), "{} must be Osaka-priced", id.name());
        }
    }

    /// The Osaka numbers those seventeen entries carry, so a change in the base schedule shows up
    /// here rather than silently moving Satin.
    #[test]
    fn test_the_pressed_back_entries_carry_the_osaka_numbers() {
        let satin = satin_gas_params();
        for (name, value) in [
            ("warm_storage_read_cost", 100),
            ("cold_account_additional_cost", 2_500),
            ("cold_storage_additional_cost", 2_000),
            ("cold_storage_cost", 2_100),
            ("transfer_value_cost", 9_000),
            ("new_account_cost", 25_000),
            ("new_account_cost_for_selfdestruct", 25_000),
            ("sstore_static", 100),
            ("sstore_set_without_load_cost", 19_900),
            ("sstore_reset_without_cold_load_cost", 2_800),
            ("sstore_set_refund", 19_900),
            ("sstore_reset_refund", 2_800),
            ("sstore_clearing_slot_refund", 4_800),
            ("create", 32_000),
            ("tx_create_cost", 32_000),
            ("tx_access_list_address_cost", 2_400),
            ("tx_access_list_storage_key_cost", 1_900),
        ] {
            let id = GasId::from_name(name).expect("a known gas id");
            assert_eq!(satin.get(id), value, "{name}");
        }
    }

    /// Every entry outside the seventeen is Amsterdam's. The state-gas entries are rebuilt from
    /// the cost per state byte rather than copied, so they are checked separately; at the
    /// provisional price they land on Amsterdam's numbers too, which
    /// [`test_other_prices_move_only_the_byte_priced_entries`] shows is a coincidence of the
    /// price and not a copy.
    #[test]
    fn test_every_other_entry_is_amsterdam() {
        let (satin, amsterdam) = (satin_gas_params(), amsterdam());
        for slot in 0..=u8::MAX {
            let id = GasId::new(slot);
            let rebuilt = EIP8038_REPRICED.iter().any(|entry| entry() == id) ||
                STATE_GAS_REPRICED.iter().any(|&(entry, _)| entry() == id) ||
                HISTORY_GAS_PRICED.iter().any(|&(entry, _)| entry() == id);
            if rebuilt {
                continue;
            }
            assert_eq!(satin.get(id), amsterdam.get(id), "{} must be Amsterdam's", id.name());
        }
        // The entries Amsterdam introduced whose value is derived from an EIP-8038 constant stay
        // Amsterdam's all the same; spelled out because pressing them back would be plausible.
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
            assert_eq!(amsterdam.get(id), value, "{name} is Amsterdam's");
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
