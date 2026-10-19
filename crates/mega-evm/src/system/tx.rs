//! The system-address transaction: how the protocol maintains its own state.
//!
//! The sequencer writes what the protocol owes the chain — the oracle values it served, for one
//! — through transactions sent from the system address the `SequencerRegistry` names. They pay
//! no fee, so they are executed as deposit transactions: the engine stamps
//! [`MEGA_SYSTEM_TRANSACTION_SOURCE_HASH`] on one before op-revm validates it, and op-revm's
//! deposit path takes it from there.
//!
//! A system-address transaction has a fixed shape
//! ([`has_system_transaction_shape`]): a legacy transaction calling a contract on
//! [`MEGA_SYSTEM_TX_WHITELIST`]. Every transaction reads the live system address out of the
//! registry when it is validated
//! ([`load_system_address`](crate::system::load_system_address)), and one of the system shape
//! whose caller is that address is a system-address transaction. A transaction from that address
//! of any other shape is invalid ([`SYSTEM_ADDRESS_WITHOUT_SYSTEM_SHAPE`]): the Oracle's
//! `onlySystemAddress` tests the sender alone, so the system key must send nothing the Oracle
//! would accept as an ordinary transaction. A deposit is never of the system shape, so a deposit
//! from the address is invalid too, and op-revm includes it as a failed deposit, which runs no
//! call. Every other sender is an ordinary one, an address a rotation retired included.
//!
//! A deposit is unvalidated by construction, so the engine validates what still matters itself
//! before promoting the transaction ([`validate_and_promote`]):
//!
//! - its chain id must be the chain's, so a transaction cannot be replayed on another chain;
//! - its nonce must be the system address's, so it cannot be replayed on this one;
//! - the system address must have no code of its own (EIP-3607).
//!
//! The chain id is checked whatever the configuration says, as it is for a user transaction: the
//! spec fixes the check on. A system-address transaction must carry one, which a user's legacy
//! transaction need not. The configuration switches that turn the other two checks off for a user
//! transaction (`disable_nonce_check`, `disable_eip3607`) turn them off here too: a caller
//! simulating a transaction sees one shape of validation, not two.

use alloy_evm::Database;
use alloy_primitives::{address, b256, Address, TxKind, B256};
use revm::{
    context::{ContextTr, JournalTr, Transaction},
    context_interface::{cfg::Cfg, result::InvalidTransaction},
    handler::pre_execution::validate_account_nonce_and_code,
};

use crate::{
    system::ORACLE_CONTRACT_ADDRESS, types::MegaTransaction, ExternalEnvTypes, MegaContext,
};

/// The system address the unknown-chain placeholder schedule seeds the `SequencerRegistry` with.
///
/// A system-address transaction is executed as a deposit: no nonce check of op-revm's own and no
/// fee. Which address sends them is the registry's to say, and it can be rotated through the
/// registry: the engine reads the live one out of the registry for every transaction, and never
/// falls back to this address.
pub const MEGA_SYSTEM_ADDRESS: Address = address!("0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d");

/// The contracts a system-address transaction may call. It may call nothing else and create
/// nothing.
pub const MEGA_SYSTEM_TX_WHITELIST: &[Address] = &[ORACLE_CONTRACT_ADDRESS];

/// The message of the [`InvalidTransaction::Str`] a transaction is refused with when it is sent
/// from the live system address without the system shape ([`has_system_transaction_shape`]).
///
/// A transaction other than a deposit is rejected with it, by a pool as by block execution; a
/// deposit, which op-revm does not refuse for a transaction error, is included as a failed
/// deposit instead, which bumps its sender's nonce, keeps its mint and runs nothing.
pub const SYSTEM_ADDRESS_WITHOUT_SYSTEM_SHAPE: &str =
    "a transaction from the system address must be a legacy call to a whitelisted contract";

/// The `source_hash` of a promoted system-address transaction,
/// `keccak256("MEGA_SYSTEM_TRANSACTION")`.
///
/// It marks the transaction as the protocol's own: op-revm sees a deposit, and the engine can
/// tell a promoted system transaction from a user's deposit by the hash alone.
pub const MEGA_SYSTEM_TRANSACTION_SOURCE_HASH: B256 =
    b256!("852c082c0faff590c6300c2c34815d1f79882552fa95ba413cd5aeb1dba84957");

/// Whether `tx` is a system-address transaction when `system_address` is the live system
/// address: a legacy transaction from it to a contract on [`MEGA_SYSTEM_TX_WHITELIST`].
pub fn is_mega_system_transaction_with(tx: &MegaTransaction, system_address: Address) -> bool {
    check_if_mega_system_transaction(tx.caller(), tx.tx_type(), tx.kind(), system_address)
}

/// Whether a transaction with these fields is a system-address transaction when `system_address`
/// is the live system address: it was sent from it and has the system shape
/// ([`has_system_transaction_shape`]).
pub fn check_if_mega_system_transaction(
    tx_signer: Address,
    tx_type: u8,
    tx_kind: TxKind,
    system_address: Address,
) -> bool {
    tx_signer == system_address && has_system_transaction_shape(tx_type, tx_kind)
}

/// Whether a transaction with these fields has the shape every system-address transaction has: a
/// legacy transaction (type `0x0`, the shape the sequencer builds) calling a contract on
/// [`MEGA_SYSTEM_TX_WHITELIST`]. A creation never has it: the protocol maintains the contracts it
/// deployed, it does not deploy new ones this way.
///
/// It looks at the transaction's own fields only: the type, then the destination against the
/// one-entry whitelist.
pub fn has_system_transaction_shape(tx_type: u8, tx_kind: TxKind) -> bool {
    tx_type == 0x0 &&
        match tx_kind {
            TxKind::Create => false,
            TxKind::Call(address) => MEGA_SYSTEM_TX_WHITELIST.contains(&address),
        }
}

/// Whether `tx` is a system-address transaction when `system_address` is the live system address,
/// or the error it is refused with when it is sent from the live system address without the
/// system shape ([`SYSTEM_ADDRESS_WITHOUT_SYSTEM_SHAPE`]).
///
/// The system key sends the system shape and nothing else: whatever else it signs — a typed
/// transaction, a call off the whitelist, a creation — is invalid, and so is a deposit from the
/// address, which is never of the system shape, and which op-revm then includes as a failed
/// deposit. An address the registry no longer names, or never named, is an ordinary sender.
pub(crate) fn classify_system_transaction(
    tx: &MegaTransaction,
    system_address: Option<Address>,
) -> Result<bool, InvalidTransaction> {
    if system_address != Some(tx.caller()) {
        return Ok(false);
    }
    if !has_system_transaction_shape(tx.tx_type(), tx.kind()) {
        return Err(InvalidTransaction::Str(SYSTEM_ADDRESS_WITHOUT_SYSTEM_SHAPE.into()));
    }
    Ok(true)
}

/// Whether `tx` was produced by the protocol itself rather than by a user, when `system_address`
/// is the live system address.
///
/// That is the case for a call the engine makes on the chain's behalf (the EIP-2935 and EIP-4788
/// pre-block calls, whose caller is the EIP-4788 system address) and for a system-address
/// transaction, before or after it was promoted. A user's deposit transaction is not
/// system-originated, however deposit-like it is: it carries another source hash and another
/// caller. A deposit that carries the promotion's source hash is system-originated whoever sends
/// it; from the live system address it is also invalid, as every deposit from that address is
/// ([`SYSTEM_ADDRESS_WITHOUT_SYSTEM_SHAPE`]), so a transaction handed to the engine already
/// promoted is a failed deposit.
///
/// It is what exempts the protocol's own work from the metering `MegaETH` holds users to — the
/// history gas of a system transaction is not charged, and no per-transaction limit stops it, so
/// the protocol's maintenance cannot fail on a resource limit. The engine makes the same decision
/// for the running transaction with the address it read itself
/// ([`MegaContext::is_system_originated`]).
pub fn is_system_originated(tx: &MegaTransaction, system_address: Address) -> bool {
    originates_from_the_protocol(tx, is_mega_system_transaction_with(tx, system_address))
}

/// [`is_system_originated`], given whether `tx` is a system-address transaction.
pub(crate) fn originates_from_the_protocol(tx: &MegaTransaction, system_transaction: bool) -> bool {
    system_transaction ||
        tx.caller() == alloy_eips::eip4788::SYSTEM_ADDRESS ||
        tx.deposit.source_hash == MEGA_SYSTEM_TRANSACTION_SOURCE_HASH
}

/// Validates a system-address transaction and promotes it to a deposit.
///
/// Called for the running transaction once validation found it is one, before
/// op-revm validates it, so op-revm sees the promoted shape: a deposit, whose signature, nonce and
/// fee it does not check. The checks the deposit path drops and this chain still wants are made
/// here (see the module documentation): the chain id always, the nonce and the code each under
/// the configuration switch a user transaction obeys.
///
/// The system address's nonce and code are the state's: they are checked when `check_account`
/// says so, which is always in execution. Validation without state leaves them to the state, as
/// it leaves every sender's.
pub(crate) fn validate_and_promote<DB, ExtEnvs, ERROR>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    check_account: bool,
) -> Result<(), ERROR>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    ERROR: From<InvalidTransaction> + From<DB::Error>,
{
    let cfg = ctx.cfg();
    let chain_id = cfg.chain_id();
    let (eip3607_disabled, nonce_check_disabled) =
        (cfg.is_eip3607_disabled(), cfg.is_nonce_check_disabled());
    match ctx.tx().chain_id() {
        None => return Err(InvalidTransaction::MissingChainId.into()),
        Some(id) if id != chain_id => return Err(InvalidTransaction::InvalidChainId.into()),
        Some(_) => {}
    }

    // The system address is the transaction's caller, which the transaction starts warm, so the
    // load changes nothing it pays. Its code is loaded, so EIP-3607 sees a lazy database's code
    // too.
    if check_account {
        let (system_address, tx_nonce) = (ctx.tx().caller(), ctx.tx().nonce());
        let account = ctx.journal_mut().load_account_with_code(system_address)?.data;
        validate_account_nonce_and_code(
            &account.info,
            tx_nonce,
            eip3607_disabled,
            nonce_check_disabled,
        )?;
    }

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
    use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;

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

    /// A user's deposit transaction is not system-originated: otherwise a user could buy the
    /// protocol's exemptions by sending one.
    #[test]
    fn test_is_system_originated_rejects_user_deposit_tx() {
        let mut tx = legacy_call_tx(USER, ORACLE_CONTRACT_ADDRESS);
        tx.deposit.source_hash = B256::repeat_byte(0x11);
        assert_eq!(tx.tx_type(), DEPOSIT_TRANSACTION_TYPE);
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

    /// The shape is the transaction's own fields alone: a legacy call to a whitelisted contract,
    /// whoever sends it. A creation, another type and a promoted transaction (a deposit by type)
    /// do not have it.
    #[test]
    fn test_the_system_shape_is_a_legacy_call_to_a_whitelisted_contract() {
        let oracle = TxKind::Call(ORACLE_CONTRACT_ADDRESS);
        assert!(has_system_transaction_shape(0, oracle));
        assert!(!has_system_transaction_shape(0, TxKind::Call(NOT_WHITELISTED)));
        assert!(!has_system_transaction_shape(0, TxKind::Create));
        for tx_type in [1, 2, 3, 4, DEPOSIT_TRANSACTION_TYPE] {
            assert!(!has_system_transaction_shape(tx_type, oracle), "type {tx_type}");
        }
        let mut promoted = legacy_call_tx(USER, ORACLE_CONTRACT_ADDRESS);
        promoted.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
        assert!(!has_system_transaction_shape(promoted.tx_type(), promoted.kind()));
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
