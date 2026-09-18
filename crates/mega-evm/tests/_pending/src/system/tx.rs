//! Unit tests extracted from `crates/mega-evm/src/system/tx.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/system/tx.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::TxKind;

    const EIP_SYSTEM_ADDRESS: Address = alloy_eips::eip4788::SYSTEM_ADDRESS;
    const USER: Address = address!("0000000000000000000000000000000000009999");
    const NON_WHITELIST: Address = address!("00000000000000000000000000000000000000ab");

    /// Builds a legacy (type 0x0) call transaction with the given caller and callee.
    fn legacy_call_tx(caller: Address, to: Address) -> MegaTransaction {
        let mut tx = MegaTransaction::default();
        tx.base.tx_type = 0;
        tx.base.caller = caller;
        tx.base.kind = TxKind::Call(to);
        tx
    }

    #[test]
    fn test_is_system_originated_matches_eip_system_address_caller() {
        // The protocol's own pre-block system calls (EIP-2935 / EIP-4788 / SequencerRegistry)
        // use `0xff..fe` as caller; it matches regardless of the resolved system address or target.
        let tx = legacy_call_tx(EIP_SYSTEM_ADDRESS, USER);
        assert!(is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
        assert!(is_system_originated(&tx, USER));
    }

    #[test]
    fn test_is_system_originated_matches_mega_system_tx() {
        // Sequencer mega system tx: legacy tx from the system address to a whitelisted contract.
        let tx = legacy_call_tx(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS);
        assert!(is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    #[test]
    fn test_is_system_originated_rejects_system_caller_to_non_whitelist() {
        // System address calling a non-whitelisted contract is not a mega system tx.
        let tx = legacy_call_tx(MEGA_SYSTEM_ADDRESS, NON_WHITELIST);
        assert!(!is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    #[test]
    fn test_is_system_originated_matches_promoted_mega_system_tx() {
        // `before_run` promotes a mega system tx to a deposit (stamping the source hash and
        // flipping `tx_type()`) *before* `on_new_tx` evaluates the exemption. The promoted shape
        // must still be recognized as system-originated.
        let mut tx = legacy_call_tx(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS);
        tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
        assert_eq!(tx.tx_type(), DEPOSIT_TRANSACTION_TYPE, "promotion flips tx_type to deposit");
        assert!(
            !is_mega_system_transaction_with(&tx, MEGA_SYSTEM_ADDRESS),
            "the legacy-typed check no longer matches after promotion",
        );
        assert!(is_system_originated(&tx, MEGA_SYSTEM_ADDRESS), "but the source-hash branch does");
    }

    #[test]
    fn test_is_system_originated_rejects_user_tx() {
        let tx = legacy_call_tx(USER, ORACLE_CONTRACT_ADDRESS);
        assert!(!is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    #[test]
    fn test_is_system_originated_rejects_user_deposit_tx() {
        // Anti-bypass: a user deposit transaction (non-system caller) must NOT be treated as
        // system-originated, even though `is_deposit_like_transaction` classifies any deposit as
        // deposit-like. Otherwise a user could craft a deposit to escape SALT-scaled storage gas.
        let mut tx = legacy_call_tx(USER, ORACLE_CONTRACT_ADDRESS);
        // A non-zero deposit source hash makes `tx_type()` report `DEPOSIT_TRANSACTION_TYPE`.
        tx.deposit.source_hash = B256::repeat_byte(0x11);
        assert_eq!(tx.tx_type(), DEPOSIT_TRANSACTION_TYPE);
        assert!(is_deposit_like_transaction(&tx, MEGA_SYSTEM_ADDRESS));
        assert!(!is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }
}
