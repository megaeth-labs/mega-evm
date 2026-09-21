//! The system-address transaction: how the protocol maintains its own state.
//!
//! The sequencer writes what the protocol owes the chain — the oracle values it served, for one
//! — through transactions sent from [`MEGA_SYSTEM_ADDRESS`]. They carry no signature and pay no
//! fee, so they are executed as deposit transactions: the engine stamps
//! [`MEGA_SYSTEM_TRANSACTION_SOURCE_HASH`] on one before it validates it, and op-revm's deposit
//! path takes it from there.
//!
//! A deposit is unvalidated by construction, so the engine validates what still matters itself
//! before promoting the transaction ([`validate_and_promote`]):
//!
//! - it may only call a contract on [`MEGA_SYSTEM_TX_WHITELIST`], never create one;
//! - its chain id must be the chain's, so a transaction cannot be replayed on another chain;
//! - its nonce must be the system address's, so it cannot be replayed on this one;
//! - the system address must have no code of its own (EIP-3607).
//!
//! The configuration switches that turn those checks off for a user transaction
//! (`tx_chain_id_check`, `disable_nonce_check`, `disable_eip3607`) turn them off here too: a
//! caller simulating a transaction sees one shape of validation, not two.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::string::ToString;

use alloy_evm::Database;
use alloy_primitives::{address, b256, Address, TxKind, B256};
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{
    context::{result::FromStringError, ContextTr, Transaction},
    context_interface::{cfg::Cfg, result::InvalidTransaction},
    handler::pre_execution::validate_account_nonce_and_code,
};

use crate::{
    system::ORACLE_CONTRACT_ADDRESS, types::MegaTransaction, ExternalEnvTypes, JournalInspectTr,
    MegaContext,
};

/// The address the sequencer sends the protocol's own transactions from.
///
/// A transaction from it is executed as a deposit: no signature, no nonce check of op-revm's own
/// and no fee. Which address it is can be rotated through the `SequencerRegistry`; reading the
/// rotated one out of the registry's storage belongs to system contract deployment, and until
/// then this is the address.
pub const MEGA_SYSTEM_ADDRESS: Address = address!("0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d");

/// The contracts a system-address transaction may call. It may call nothing else and create
/// nothing.
pub const MEGA_SYSTEM_TX_WHITELIST: &[Address] = &[ORACLE_CONTRACT_ADDRESS];

/// The `source_hash` of a promoted system-address transaction,
/// `keccak256("MEGA_SYSTEM_TRANSACTION")`.
///
/// It marks the transaction as the protocol's own: op-revm sees a deposit, and the engine can
/// tell a promoted system transaction from a user's deposit by the hash alone.
pub const MEGA_SYSTEM_TRANSACTION_SOURCE_HASH: B256 =
    b256!("852c082c0faff590c6300c2c34815d1f79882552fa95ba413cd5aeb1dba84957");

/// Whether `tx` was sent from `system_address`.
pub fn sent_from_system_address(tx: &MegaTransaction, system_address: Address) -> bool {
    tx.caller() == system_address
}

/// Whether `tx` is a system-address transaction: a legacy transaction from `system_address` to a
/// contract on [`MEGA_SYSTEM_TX_WHITELIST`].
pub fn is_mega_system_transaction_with(tx: &MegaTransaction, system_address: Address) -> bool {
    check_if_mega_system_transaction(tx.caller(), tx.tx_type(), tx.kind(), system_address)
}

/// Whether a transaction with these fields is a system-address transaction.
///
/// It is one when it was sent from `system_address` as a legacy transaction (type `0x0`, the
/// shape the sequencer builds) and calls a contract on [`MEGA_SYSTEM_TX_WHITELIST`]. A creation
/// is never one: the protocol maintains the contracts it deployed, it does not deploy new ones
/// this way.
pub fn check_if_mega_system_transaction(
    tx_signer: Address,
    tx_type: u8,
    tx_kind: TxKind,
    system_address: Address,
) -> bool {
    if tx_type != 0x0 || tx_signer != system_address {
        return false;
    }
    match tx_kind {
        TxKind::Create => false,
        TxKind::Call(address) => MEGA_SYSTEM_TX_WHITELIST.contains(&address),
    }
}

/// Whether `tx` is executed on the deposit path: a deposit transaction, or a system-address
/// transaction, which is promoted to one.
pub fn is_deposit_like_transaction(tx: &MegaTransaction, system_address: Address) -> bool {
    tx.tx_type() == DEPOSIT_TRANSACTION_TYPE || is_mega_system_transaction_with(tx, system_address)
}

/// Whether `tx` was produced by the protocol itself rather than by a user.
///
/// That is the case for a call the engine makes on the chain's behalf (the EIP-2935 and EIP-4788
/// pre-block calls, whose caller is the EIP-4788 system address) and for a system-address
/// transaction, before or after it was promoted. A user's deposit transaction is not
/// system-originated, however deposit-like it is: it carries another source hash and another
/// caller.
///
/// It is what exempts the protocol's own work from the metering `MegaETH` holds users to — the
/// history gas of a system transaction is not charged, so the protocol's maintenance cannot fail
/// on a resource limit. The mechanisms that meter read it as they land.
pub fn is_system_originated(tx: &MegaTransaction, system_address: Address) -> bool {
    tx.caller() == alloy_eips::eip4788::SYSTEM_ADDRESS ||
        tx.deposit.source_hash == MEGA_SYSTEM_TRANSACTION_SOURCE_HASH ||
        is_mega_system_transaction_with(tx, system_address)
}

/// Validates a transaction sent from the system address and promotes it to a deposit; does
/// nothing to any other transaction.
///
/// Runs before op-revm validates the transaction, so op-revm sees the promoted shape: a deposit,
/// whose signature, nonce and fee it does not check. The checks the deposit path drops and this
/// chain still wants are made here (see the module documentation), each under the configuration
/// switch a user transaction obeys.
///
/// A transaction from the system address that is not a system transaction is rejected outright:
/// the sequencer has no business sending it, and promoting it would give it the deposit path's
/// exemptions. That covers a creation, a call to a contract that is not on the whitelist, and
/// any shape that is not a legacy transaction — an EIP-1559 one, or one that already carries a
/// source hash, which has skipped the validation the promotion runs.
pub(crate) fn validate_and_promote<DB, ExtEnvs, ERROR>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    system_address: Address,
) -> Result<(), ERROR>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    ERROR: From<InvalidTransaction> + From<DB::Error> + FromStringError,
{
    if !sent_from_system_address(ctx.tx(), system_address) {
        return Ok(());
    }
    if !is_mega_system_transaction_with(ctx.tx(), system_address) {
        return Err(ERROR::from_string(
            "a transaction from the system address must be a legacy call to a whitelisted \
             contract"
                .to_string(),
        ));
    }

    let cfg = ctx.cfg();
    let (chain_id, chain_id_check) = (cfg.chain_id(), cfg.tx_chain_id_check);
    let (eip3607_disabled, nonce_check_disabled) =
        (cfg.is_eip3607_disabled(), cfg.is_nonce_check_disabled());
    if chain_id_check {
        match ctx.tx().chain_id() {
            None => return Err(InvalidTransaction::MissingChainId.into()),
            Some(id) if id != chain_id => return Err(InvalidTransaction::InvalidChainId.into()),
            Some(_) => {}
        }
    }

    // Read the system address without warming it: validation must not leave the account in the
    // EIP-2929 access list, which would make the transaction's first touch of it cheaper than
    // the same transaction from anyone else. Its code is loaded, so EIP-3607 sees a lazy
    // database's code too.
    let tx_nonce = ctx.tx().nonce();
    let account = ctx.journal_mut().inspect_account(system_address, true)?;
    validate_account_nonce_and_code(
        &account.info,
        tx_nonce,
        eip3607_disabled,
        nonce_check_disabled,
    )?;

    // The deposit shape: op-revm skips the signature, the nonce and every fee for it. The gas
    // price goes to zero with it, so the fee accounting of the block degenerates to nothing.
    let tx = &mut ctx.inner.tx;
    tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
    tx.base.gas_price = 0;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::TxKind;

    const EIP_SYSTEM_ADDRESS: Address = alloy_eips::eip4788::SYSTEM_ADDRESS;
    const USER: Address = address!("0x0000000000000000000000000000000000009999");
    const NOT_WHITELISTED: Address = address!("0x00000000000000000000000000000000000000ab");

    /// A legacy call transaction from `caller` to `to`.
    fn legacy_call_tx(caller: Address, to: Address) -> MegaTransaction {
        let mut tx = MegaTransaction::default();
        tx.base.tx_type = 0;
        tx.base.caller = caller;
        tx.base.kind = TxKind::Call(to);
        tx
    }

    /// The pre-block calls the engine makes are system-originated whatever the system address
    /// is: their caller is the EIP-4788 system address, which no key can sign for.
    #[test]
    fn test_is_system_originated_matches_eip_system_address_caller() {
        let tx = legacy_call_tx(EIP_SYSTEM_ADDRESS, USER);
        assert!(is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
        assert!(is_system_originated(&tx, USER));
    }

    #[test]
    fn test_is_system_originated_matches_mega_system_tx() {
        let tx = legacy_call_tx(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS);
        assert!(is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    #[test]
    fn test_is_system_originated_rejects_system_caller_to_non_whitelist() {
        let tx = legacy_call_tx(MEGA_SYSTEM_ADDRESS, NOT_WHITELISTED);
        assert!(!is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    /// The promotion stamps the source hash and flips the transaction's type to a deposit; the
    /// promoted shape is still system-originated, through the source hash.
    #[test]
    fn test_is_system_originated_matches_promoted_mega_system_tx() {
        let mut tx = legacy_call_tx(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS);
        tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
        assert_eq!(tx.tx_type(), DEPOSIT_TRANSACTION_TYPE, "the promotion makes it a deposit");
        assert!(
            !is_mega_system_transaction_with(&tx, MEGA_SYSTEM_ADDRESS),
            "the legacy-typed check no longer matches after the promotion",
        );
        assert!(is_system_originated(&tx, MEGA_SYSTEM_ADDRESS), "the source hash does");
    }

    #[test]
    fn test_is_system_originated_rejects_user_tx() {
        let tx = legacy_call_tx(USER, ORACLE_CONTRACT_ADDRESS);
        assert!(!is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    /// A user's deposit transaction is deposit-like but not system-originated: otherwise a user
    /// could buy the protocol's exemptions by sending one.
    #[test]
    fn test_is_system_originated_rejects_user_deposit_tx() {
        let mut tx = legacy_call_tx(USER, ORACLE_CONTRACT_ADDRESS);
        tx.deposit.source_hash = B256::repeat_byte(0x11);
        assert_eq!(tx.tx_type(), DEPOSIT_TRANSACTION_TYPE);
        assert!(is_deposit_like_transaction(&tx, MEGA_SYSTEM_ADDRESS));
        assert!(!is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    /// A system transaction is a legacy call to a whitelisted contract, and nothing else.
    #[test]
    fn test_only_a_whitelisted_legacy_call_is_a_system_transaction() {
        assert!(is_mega_system_transaction_with(
            &legacy_call_tx(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS),
            MEGA_SYSTEM_ADDRESS,
        ));
        assert!(!is_mega_system_transaction_with(
            &legacy_call_tx(MEGA_SYSTEM_ADDRESS, NOT_WHITELISTED),
            MEGA_SYSTEM_ADDRESS,
        ));
        assert!(!is_mega_system_transaction_with(
            &legacy_call_tx(USER, ORACLE_CONTRACT_ADDRESS),
            MEGA_SYSTEM_ADDRESS,
        ));
        assert!(
            !check_if_mega_system_transaction(
                MEGA_SYSTEM_ADDRESS,
                0,
                TxKind::Create,
                MEGA_SYSTEM_ADDRESS,
            ),
            "the protocol creates no contract this way",
        );
        assert!(
            !check_if_mega_system_transaction(
                MEGA_SYSTEM_ADDRESS,
                2,
                TxKind::Call(ORACLE_CONTRACT_ADDRESS),
                MEGA_SYSTEM_ADDRESS,
            ),
            "the sequencer builds a legacy transaction",
        );
    }

    /// The source hash is the hash of the name it stands for.
    #[test]
    fn test_the_source_hash_is_the_hash_of_its_name() {
        assert_eq!(
            MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
            alloy_primitives::keccak256("MEGA_SYSTEM_TRANSACTION"),
        );
    }
}
