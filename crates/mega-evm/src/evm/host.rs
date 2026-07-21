use core::cell::RefCell;

#[cfg(not(feature = "std"))]
use alloc as std;
use mega_system_contracts::access_control::IMegaAccessControl::VolatileDataAccessType;
use std::{format, rc::Rc};

use crate::{
    AdditionalLimit, ExternalEnvTypes, MegaContext, MegaSpecId, OracleEnv,
    VolatileDataAccessTracker, ORACLE_CONTRACT_ADDRESS,
};
use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, Log, B256, U256};
use delegate::delegate;
use revm::{
    context::{ContextTr, JournalTr},
    context_interface::{context::ContextError, journaled_state::AccountLoad},
    interpreter::{Host, SStoreResult, SelfDestructResult, StateLoad},
    primitives::{hash_map::Entry, StorageKey, KECCAK_EMPTY},
    state::{Account, Bytecode, EvmStorageSlot},
    Journal,
};

impl<DB: Database, ExtEnvs: ExternalEnvTypes> Host for MegaContext<DB, ExtEnvs> {
    // Block environment related methods - with tracking
    fn basefee(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::BaseFee);
        self.inner.basefee()
    }

    fn gas_limit(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::GasLimit);
        self.inner.gas_limit()
    }

    fn difficulty(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::Difficulty);
        self.inner.difficulty()
    }

    fn prevrandao(&self) -> Option<U256> {
        self.mark_block_env_accessed(VolatileDataAccessType::PrevRandao);
        self.inner.prevrandao()
    }

    fn block_number(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::BlockNumber);
        self.inner.block_number()
    }

    fn timestamp(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::Timestamp);
        self.inner.timestamp()
    }

    fn beneficiary(&self) -> Address {
        self.mark_block_env_accessed(VolatileDataAccessType::Coinbase);
        self.inner.beneficiary()
    }

    fn block_hash(&mut self, number: u64) -> Option<B256> {
        self.mark_block_env_accessed(VolatileDataAccessType::BlockHash);
        self.inner.block_hash(number)
    }

    // Blob-related block environment methods - with tracking
    fn blob_gasprice(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::BlobBaseFee);
        self.inner.blob_gasprice()
    }

    fn blob_hash(&self, number: usize) -> Option<U256> {
        self.mark_block_env_accessed(VolatileDataAccessType::BlobHash);
        self.inner.blob_hash(number)
    }

    delegate! {
        to self.inner {
            fn chain_id(&self) -> U256;
            fn effective_gas_price(&self) -> U256;
            fn log(&mut self, log: Log);
            fn caller(&self) -> Address;
            fn max_initcode_size(&self) -> usize;
            fn sstore(
                &mut self,
                address: Address,
                key: U256,
                value: U256,
            ) -> Option<StateLoad<SStoreResult>>;
            fn tstore(&mut self, address: Address, key: U256, value: U256);
            fn tload(&mut self, address: Address, key: U256) -> U256;
        }
    }

    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
    ) -> Option<StateLoad<SelfDestructResult>> {
        // Rex4+: Mark beneficiary balance access when SELFDESTRUCT targets the beneficiary.
        // This enables gas detention and the disableVolatileDataAccess check in the instruction
        // wrapper.
        if self.spec.is_enabled(MegaSpecId::REX4) {
            self.check_and_mark_beneficiary_balance_access(&target);
        }

        // Rex4+: Before inner selfdestruct mutates account status, inspect the account
        // to compute state growth refund for same-TX-created accounts (EIP-6780).
        // Uses non-delegating inspect_account to ensure we enumerate storage on the
        // actual selfdestructed address, not a delegation target.
        let selfdestruct_refund = if self.spec.is_enabled(MegaSpecId::REX4) {
            let journal = &mut self.inner.journaled_state;
            // inspect_account may fail if DB errors; treat as no refund.
            inspect_account(journal, address, false).ok().and_then(|account| {
                // Only refund if the account was created in this transaction (EIP-6780:
                // only same-TX-created accounts are actually destroyed by SELFDESTRUCT).
                // Use CreatedLocal flag which matches revm's is_created_locally() check.
                if !account.status.contains(revm::state::AccountStatus::CreatedLocal) {
                    return None;
                }
                // Count new storage slots: original was zero, current is non-zero.
                let slot_count = account
                    .storage
                    .values()
                    .filter(|slot| {
                        slot.original_value().is_zero() && !slot.present_value().is_zero()
                    })
                    .count() as u64;
                // +1 for the account itself (counted in before_frame_init) + slot count.
                Some(1 + slot_count)
            })
        } else {
            None
        };

        let result = self.inner.selfdestruct(address, target);

        // Record state growth refund only on the first effective destruction.
        // Repeated SELFDESTRUCT on the same account still returns a result but with
        // `previously_destroyed == true` — refunding again would double-count.
        if let Some(refund) = selfdestruct_refund {
            if let Some(ref state_load) = result {
                if !state_load.data.previously_destroyed {
                    self.additional_limit.borrow_mut().on_selfdestruct(refund);
                }
            }
        }

        result
    }

    fn sload(&mut self, address: Address, key: U256) -> Option<StateLoad<U256>> {
        if self.spec.is_enabled(MegaSpecId::MINI_REX) && address == ORACLE_CONTRACT_ADDRESS {
            // Rex3+: Mark oracle access for gas detention on SLOAD rather than CALL.
            // The actual gas limit enforcement happens in the SLOAD instruction wrapper
            // (detain_gas_ext::sload in instructions.rs).
            // Mega system address transactions are exempted from oracle gas detention.
            // Note: This checks the transaction sender (from TxEnv) via Host::caller(),
            // unlike the pre-Rex3 CALL-based path which checked the frame-level caller.
            if self.spec.is_enabled(MegaSpecId::REX3) && self.caller() != self.system_address {
                self.volatile_data_tracker.borrow_mut().check_and_mark_oracle_access(&address);
            }

            // if the oracle env provides a value, return it. Otherwise, fallback to the inner
            // context.
            if let Some(value) = self.oracle_env.borrow().get_oracle_storage(key) {
                // Accessing oracle contract storage is forced to be cold access, since it always
                // reads from the outside world (oracle_env).
                return Some(StateLoad::new(value, true));
            }
        }
        let state_load = self.inner.sload(address, key);
        state_load.map(|mut state_load| {
            if self.spec.is_enabled(MegaSpecId::MINI_REX) && address == ORACLE_CONTRACT_ADDRESS {
                // It is indistinguishable to tell whether a storage access of oracle contract is
                // warm or not even if it is loaded from the inner journal state. This is because
                // the current execution may be a replay of existing blocks and we cannot know
                // whether the payload builder read from the oracle_env or not. So we force such
                // sload always to be cold access to ensure consistent gas cost.
                state_load.is_cold = true;
            }
            state_load
        })
    }

    fn balance(&mut self, address: Address) -> Option<StateLoad<U256>> {
        self.check_and_mark_beneficiary_balance_access(&address);
        self.inner.balance(address)
    }

    fn load_account_delegated(&mut self, address: Address) -> Option<StateLoad<AccountLoad>> {
        self.check_and_mark_beneficiary_balance_access(&address);
        // Rex6+: also mark the EIP-7702 delegate of `address` if any, so a CALL whose
        // target delegates to the beneficiary triggers detention even though the raw stack
        // operand doesn't match. The Rex4 path only marked the raw input. A resolve DB error
        // falls back to the raw address (no delegate mark) — the `load_account_delegated` below
        // remains responsible for surfacing the failure.
        if self.spec.is_enabled(MegaSpecId::REX6) {
            let resolved = self.best_effort_resolve_eip7702_delegate_address(address);
            if resolved != address {
                self.check_and_mark_beneficiary_balance_access(&resolved);
            }
        }
        self.inner.load_account_delegated(address)
    }

    fn load_account_code(&mut self, address: Address) -> Option<StateLoad<Bytes>> {
        self.check_and_mark_beneficiary_balance_access(&address);
        self.inner.load_account_code(address)
    }

    fn load_account_code_hash(&mut self, address: Address) -> Option<StateLoad<B256>> {
        self.check_and_mark_beneficiary_balance_access(&address);
        self.inner.load_account_code_hash(address)
    }
}

/// Extension trait for the `Host` trait that provides additional functionality for `MegaETH`.
///
/// Gas cost methods (`sstore_set_storage_gas`, `new_account_storage_gas`,
/// `create_contract_storage_gas`) follow the same error-handling pattern as revm's `Host` trait:
/// on error, stash the error in `self.error()` and return `None`.
/// This ensures that `FatalExternalError` always has a stashed error for revm to drain.
pub trait HostExt: Host {
    /// Gets the `MegaSpecId` of the current execution context.
    fn spec_id(&self) -> MegaSpecId;

    /// Gets the `AdditionalLimit` instance. Only used when the `MINI_REX` spec is enabled.
    fn additional_limit(&self) -> &Rc<RefCell<AdditionalLimit>>;

    /// Gets the gas cost for setting a storage slot to a non-zero value. Only used when the
    /// `MINI_REX` spec is enabled.
    ///
    /// Returns `None` if the underlying SALT environment returns an error (the error is stashed
    /// in `self.error()`).
    fn sstore_set_storage_gas(&mut self, address: Address, key: U256) -> Option<u64>;

    /// Gets the gas cost for creating a new account. Only used when the `MINI_REX` spec is enabled.
    ///
    /// Returns `None` if the underlying SALT environment returns an error (the error is stashed
    /// in `self.error()`).
    fn new_account_storage_gas(&mut self, address: Address) -> Option<u64>;

    /// Gets the gas cost for creating a new contract. Only used when the `REX` spec is
    /// enabled.
    ///
    /// Returns `None` if the underlying SALT environment returns an error (the error is stashed
    /// in `self.error()`).
    fn create_contract_storage_gas(&mut self, address: Address) -> Option<u64>;

    /// Gets the volatile data tracker. Only used when the `MINI_REX` spec is enabled.
    fn volatile_data_tracker(&self) -> &Rc<RefCell<VolatileDataAccessTracker>>;

    /// Checks if volatile data access should cause a revert at the current call depth.
    /// Returns `true` if `disableVolatileDataAccess()` was called and the current
    /// journal depth is deeper than the activation depth.
    fn volatile_access_disabled(&self) -> bool;

    /// Returns the block beneficiary address without triggering volatile data tracking.
    /// Used by instruction handlers to pre-check whether an opcode targets the beneficiary.
    fn beneficiary_address(&self) -> Address;

    /// Resolves the EIP-7702 delegate of `address` one hop on a best-effort basis, returning
    /// `address` itself when there is no delegate or when the resolve hits a DB error.
    ///
    /// Unlike [`JournalInspectTr::resolve_eip7702_delegate_address`], a DB failure here is NOT
    /// stashed in `self.error()`. This is for prechecks and side-marks (the beneficiary-volatile
    /// guard, the detention side-mark) that compare the delegate against a known address and must
    /// never turn a transaction into an error on their own — e.g. a malformed CALL that underflows
    /// before it ever runs the target must keep its `StackUnderflow`, not a spurious DB error from
    /// eagerly loading a delegate's code it never needs. The opcode's real execution path reads the
    /// account again and owns surfacing any genuine DB error.
    fn best_effort_resolve_eip7702_delegate_address(&mut self, address: Address) -> Address;
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> HostExt for MegaContext<DB, ExtEnvs> {
    #[inline]
    fn spec_id(&self) -> MegaSpecId {
        self.spec
    }

    #[inline]
    fn additional_limit(&self) -> &Rc<RefCell<AdditionalLimit>> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        &self.additional_limit
    }

    #[inline]
    fn sstore_set_storage_gas(&mut self, address: Address, key: U256) -> Option<u64> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        // System-tx exemption (REX6+ `LimitCheck::Exempt` stamp): charge un-scaled (min-bucket)
        // storage gas so the write never depends on SALT bucket capacity and can never OOG as
        // buckets grow. This path also avoids querying the SALT env.
        if self.additional_limit.borrow().has_exceeded_limit.is_exempt() {
            return Some(self.dynamic_storage_gas_cost.borrow().sstore_set_gas_unscaled());
        }
        let result = self.dynamic_storage_gas_cost.borrow_mut().sstore_set_gas(address, key);
        result
            .map_err(|e| {
                *self.error() = Err(ContextError::Custom(format!("{e}")));
            })
            .ok()
    }

    #[inline]
    fn new_account_storage_gas(&mut self, address: Address) -> Option<u64> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        if self.additional_limit.borrow().has_exceeded_limit.is_exempt() {
            return Some(self.dynamic_storage_gas_cost.borrow().new_account_gas_unscaled());
        }
        let result = self.dynamic_storage_gas_cost.borrow_mut().new_account_gas(address);
        result
            .map_err(|e| {
                *self.error() = Err(ContextError::Custom(format!("{e}")));
            })
            .ok()
    }

    #[inline]
    fn create_contract_storage_gas(&mut self, address: Address) -> Option<u64> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::REX));
        if self.additional_limit.borrow().has_exceeded_limit.is_exempt() {
            return Some(self.dynamic_storage_gas_cost.borrow().create_contract_gas_unscaled());
        }
        let result = self.dynamic_storage_gas_cost.borrow_mut().create_contract_gas(address);
        result
            .map_err(|e| {
                *self.error() = Err(ContextError::Custom(format!("{e}")));
            })
            .ok()
    }

    #[inline]
    fn volatile_data_tracker(&self) -> &Rc<RefCell<VolatileDataAccessTracker>> {
        &self.volatile_data_tracker
    }

    #[inline]
    fn volatile_access_disabled(&self) -> bool {
        let current_depth = self.journal_ref().depth();
        self.volatile_data_tracker.borrow().volatile_access_disabled(current_depth)
    }

    #[inline]
    fn beneficiary_address(&self) -> Address {
        self.inner.block.beneficiary
    }

    #[inline]
    fn best_effort_resolve_eip7702_delegate_address(&mut self, address: Address) -> Address {
        // Resolve through the journal directly so a DB error propagates as `Err` here (and is
        // discarded) rather than being stashed into `self.error()` by
        // `MegaContext::inspect_account`.
        let spec = self.spec;
        self.inner
            .journaled_state
            .resolve_eip7702_delegate_address(spec, address)
            .unwrap_or(address)
    }
}

/// Trait to inspect the journal's internal state without marking any accounts or storage slots as
/// warm.
///
/// # EIP-7702 address semantics
///
/// Address handling is intentionally split by call-site purpose. The guiding rule: follow the
/// delegate ONLY to (1) execute the delegate's code and (2) decide whether an operand observes the
/// block beneficiary's state. Everything that *attributes* balance / nonce / storage / state-growth
/// / data-size uses the ORIGINAL address — a delegated account's state is its own, not the
/// delegate's. Before adding a new 7702-touching call site, find its row in this table and use the
/// listed address; do not re-derive the raw-vs-delegate decision ad-hoc.
///
/// | Purpose | Address | Mechanism | Spec gate |
/// | --- | --- | --- | --- |
/// | Code / execution target | delegate | revm's `load_account_delegated` (not these primitives) | revm-owned |
/// | Storage / SALT / state-growth / account-write accounting | original | `inspect_account` | REX5+ original; pre-REX5 followed the delegate (frozen) |
/// | CALL-family beneficiary / volatile-access check | delegate | `resolve_eip7702_delegate_address` | REX6+ only; `<=` REX5 compares the raw operand (frozen) |
/// | Validate-time authorization accounting | original authority | `inspect_account` with `load_code = true` (EIP-7702 / EIP-3607 detection) | scan-wide |
///
/// `inspect_account_delegated` follows the delegate; it backs the frozen pre-REX5 accounting path
/// and any caller that genuinely needs the delegate's account. New accounting call sites should use
/// `inspect_account` (original address), not this.
///
/// To improve performance, when journal does not have the account or storage slot, it will be
/// loaded from the database and cached in the journal.
/// However, since we explicitly mark the account or storage slot as cold, this pre-loading before
/// executing the original instruction will make no difference on gas cost.
///
/// Both `Journal<DB>` and `MegaContext` implement this trait:
/// - `Journal<DB>`: `DBError = DB::Error` — returns DB errors for propagation.
/// - `MegaContext`: `DBError = ()` — stashes errors in `self.error()` and returns `Err(())`.
pub trait JournalInspectTr {
    /// The error type returned on DB failures.
    type DBError: core::fmt::Debug;

    /// Inspect the account at the given address without marking it as warm and without
    /// following EIP-7702 delegation.
    ///
    /// Loads the account from the database into the journal cache (so subsequent
    /// in-block reads see this committed state), then explicitly marks it cold so the
    /// inspection does not show up in EIP-2929's access list and produces no
    /// `account_warmed` journal entry. Use this for metering inspections where the
    /// authority's own state matters (e.g., new-account storage-gas premium, SALT
    /// bucket lookup, state-growth emptiness check) rather than the delegate's state,
    /// and for validate-path reads (nonce, code) that must not participate in the
    /// access-list accounting the execution path will perform later.
    ///
    /// When `load_code` is `true`, additionally invokes `code_by_hash` if the database
    /// left `info.code` lazy (production reth-style `StateProviderDatabase::basic`
    /// returns `code: None` for accounts with on-chain bytecode, deferring code load).
    /// Set this only on call sites that read `info.code` (EIP-7702 detection, EIP-3607
    /// check) — the cheaper `false` path skips `code_by_hash` for every other cold
    /// first-touch. Parallels revm's `JournalTr::load_account_optional(.., load_code,
    /// ..)` shape.
    fn inspect_account(
        &mut self,
        address: Address,
        load_code: bool,
    ) -> Result<&mut Account, Self::DBError>;

    /// Inspect the account at the given address without marking it as warm.
    /// If the account is EIP-7702 type, follows delegation.
    ///
    /// Starting from REX4, resolves exactly one hop (matching upstream revm behavior).
    /// Pre-REX4, follows delegation recursively but detects cycles to prevent stack overflow.
    fn inspect_account_delegated(
        &mut self,
        spec: MegaSpecId,
        address: Address,
    ) -> Result<&mut Account, Self::DBError>;

    /// Inspect the storage at the given address and key without marking it as warm.
    ///
    /// Starting from REX4, storage is always loaded from the original address without following
    /// EIP-7702 delegation (matching upstream revm's sload behavior).
    /// Pre-REX4 specs retain the original behavior that follows delegation.
    fn inspect_storage(
        &mut self,
        spec: MegaSpecId,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, Self::DBError>;

    /// Resolve the EIP-7702 delegate of `address` one hop and return the target address.
    ///
    /// Returns `address` itself when the account is not EIP-7702-delegated. Hydrates `info.code`
    /// on REX5+ so EIP-7702 detection works against lazy-code databases (mirrors
    /// `inspect_account_delegated`'s rule for stable-spec freeze).
    ///
    /// Callers gate this on REX6 (the only place it is used). EIP-7702 does not exist on earlier
    /// specs, so an account there simply has no delegate code and resolves to `address` — no
    /// pre-REX4 special case is needed.
    ///
    /// Useful for instruction-wrapper checks that need to compare the effective code-running
    /// address against a known target (e.g., the block beneficiary) before delegating to revm.
    fn resolve_eip7702_delegate_address(
        &mut self,
        spec: MegaSpecId,
        address: Address,
    ) -> Result<Address, Self::DBError> {
        let load_code = spec.is_enabled(MegaSpecId::REX5);
        let account = self.inspect_account(address, load_code)?;
        let delegate = account.info.code.as_ref().and_then(|code| match code {
            Bytecode::Eip7702(c) => Some(c.address()),
            _ => None,
        });
        Ok(delegate.unwrap_or(address))
    }
}

/// Load an account into the journal cache without following EIP-7702 delegation
/// and mark it cold. When `load_code` is `true`, additionally invokes
/// `code_by_hash` if the database left `info.code` lazy.
///
/// The occupied branch's `code_by_hash` hydration is load-bearing and must
/// stay: removing it would shift observable behavior on stable specs (a second
/// `inspect_account` against the same lazy-code DB would no longer see hydrated
/// code).
fn inspect_account<DB: revm::Database>(
    journal: &mut Journal<DB>,
    address: Address,
    load_code: bool,
) -> Result<&mut Account, <DB as revm::Database>::Error> {
    let transaction_id = journal.transaction_id;
    match journal.inner.state.entry(address) {
        Entry::Occupied(entry) => {
            let account = entry.into_mut();
            if account.info.code_hash != KECCAK_EMPTY && account.info.code.is_none() {
                // Load code if not loaded before
                account.info.code = Some(journal.database.code_by_hash(account.info.code_hash)?);
            }
            Ok(account)
        }
        Entry::Vacant(entry) => {
            let mut account = journal
                .database
                .basic(address)?
                .map(|info| info.into())
                .unwrap_or_else(|| Account::new_not_existing(transaction_id));
            if load_code && account.info.code_hash != KECCAK_EMPTY && account.info.code.is_none() {
                account.info.code = Some(journal.database.code_by_hash(account.info.code_hash)?);
            }
            // deliberately mark the account as cold since we are only inspecting it, not warming
            // it.
            account.mark_cold();
            Ok(entry.insert(account))
        }
    }
}

/// Cold read of an account's balance and emptiness that loads the account into the journal (so it
/// appears in the returned `EvmState` / witness) but — unlike [`inspect_account`] — never hydrates
/// `info.code`, even when the account is already resident. The REX6 fee-reward snapshots need only
/// balance / emptiness (`AccountInfo::is_empty` decides via `code_hash`, not the bytecode);
/// forcing `code_by_hash` on a contract fee recipient would require its bytecode in a stateless
/// witness that need only carry the account proof, failing a valid fee payment on replay.
pub(crate) fn inspect_account_balance_and_emptiness<DB: revm::Database>(
    journal: &mut Journal<DB>,
    address: Address,
) -> Result<(U256, bool), <DB as revm::Database>::Error> {
    let transaction_id = journal.transaction_id;
    let info = match journal.inner.state.entry(address) {
        Entry::Occupied(entry) => &entry.into_mut().info,
        Entry::Vacant(entry) => {
            let mut account = journal
                .database
                .basic(address)?
                .map(|info| info.into())
                .unwrap_or_else(|| Account::new_not_existing(transaction_id));
            account.mark_cold();
            &entry.insert(account).info
        }
    };
    Ok((info.balance, info.is_empty()))
}

/// Cold occupancy read that returns an account's `code_hash` and loads it into the journal (so it
/// appears in the returned `EvmState` / witness) but — unlike [`inspect_account`] — never hydrates
/// `info.code`, even when the account is already resident (`inspect_account`'s already-loaded
/// branch force-loads code via `code_by_hash` regardless of its `load_code` argument). An occupancy
/// check needs only the hash; forcing `code_by_hash` on an already-warmed occupied address would
/// require its bytecode in a stateless witness that need only carry the account proof, turning the
/// expected revert into a spurious DB error on replay.
///
/// Only [`Journal`] needs this (the REX6 keyless-deploy occupancy check calls it on
/// `ctx.journal_mut()`), so it is a free function rather than a `JournalInspectTr` method.
pub(crate) fn inspect_account_code_hash<DB: revm::Database>(
    journal: &mut Journal<DB>,
    address: Address,
) -> Result<B256, <DB as revm::Database>::Error> {
    let transaction_id = journal.transaction_id;
    match journal.inner.state.entry(address) {
        Entry::Occupied(entry) => Ok(entry.get().info.code_hash),
        Entry::Vacant(entry) => {
            let mut account = journal
                .database
                .basic(address)?
                .map(|info| info.into())
                .unwrap_or_else(|| Account::new_not_existing(transaction_id));
            account.mark_cold();
            Ok(entry.insert(account).info.code_hash)
        }
    }
}

impl<DB: revm::Database> JournalInspectTr for Journal<DB> {
    type DBError = <DB as revm::Database>::Error;

    fn inspect_account(
        &mut self,
        address: Address,
        load_code: bool,
    ) -> Result<&mut Account, Self::DBError> {
        inspect_account(self, address, load_code)
    }

    fn inspect_account_delegated(
        &mut self,
        spec: MegaSpecId,
        address: Address,
    ) -> Result<&mut Account, Self::DBError> {
        // REX5+ hydrates code before the 7702 detection below; pre-REX5 must not —
        // stable specs preserve the latent lazy-DB EIP-7702 detection gap.
        let is_rex5_enabled = spec.is_enabled(MegaSpecId::REX5);

        let account = inspect_account(self, address, is_rex5_enabled)?;

        let delegated_address = account.info.code.as_ref().and_then(|code| match code {
            Bytecode::Eip7702(code) => Some(code.address()),
            _ => None,
        });
        let Some(delegated_address) = delegated_address else {
            // Not delegated — reload to satisfy borrow checker and return.
            let account = self.inner.state.get_mut(&address).unwrap();
            return Ok(account);
        };

        if spec.is_enabled(MegaSpecId::REX4) {
            // REX4+: resolve exactly one hop (matching upstream revm behavior).
            return inspect_account(self, delegated_address, is_rex5_enabled);
        }

        // Pre-REX4: follow delegation recursively with cycle detection.
        // Stays on non-hydrating `inspect_account` (load_code = false) deliberately —
        // pre-REX4 is pre-REX5, so the lazy-DB EIP-7702 detection gap is the frozen
        // behavior on these specs.
        let mut current = delegated_address;
        let mut visited = std::vec![address];
        loop {
            let account = inspect_account(self, current, false)?;
            let next = account.info.code.as_ref().and_then(|code| match code {
                Bytecode::Eip7702(code) => Some(code.address()),
                _ => None,
            });
            let Some(next) = next else {
                // End of chain — reload and return.
                let account = self.inner.state.get_mut(&current).unwrap();
                return Ok(account);
            };
            if visited.contains(&next) {
                // Cycle detected — stop here.
                let account = self.inner.state.get_mut(&current).unwrap();
                return Ok(account);
            }
            visited.push(current);
            current = next;
        }
    }

    fn inspect_storage(
        &mut self,
        spec: MegaSpecId,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, Self::DBError> {
        let transaction_id = self.transaction_id;
        let is_rex4_enabled = spec.is_enabled(MegaSpecId::REX4);
        // EIP-7702 storage semantics: storage belongs to the original address (delegator),
        // not the delegate. So `is_created` must be checked on the original address — an
        // EOA delegating via 7702 is never CREATEd, so its flag is always false. Checking
        // the delegate's flag instead would mistakenly short-circuit storage reads when the
        // delegate happens to be a freshly-CREATEd contract in the same tx, corrupting
        // SSTORE accounting (gas / kv_updates / data_size) on the delegator's slots.
        // Fold two inspect_account calls into one hydrating load on REX4 path.
        // inspect_account's occupied branch hydrates lazy code unconditionally,
        // so the old second (always-occupied) pass hydrated the same code that
        // load_code=true hydrates inline here — identical final account state,
        // identical DB-call sequence and error position, one fewer state.entry lookup.
        // Newly-created accounts must short-circuit storage misses to ZERO before any DB call.
        // Querying here would otherwise trigger a witness lookup for a slot with no meaningful
        // pre-state value, which breaks stateless replay when CREATE lands on a pre-funded
        // address: its `Loaded` cache status bypasses revm's `State::storage` short-circuit and
        // exposes the call to the witness backend.
        let is_newly_created;
        let account = if is_rex4_enabled {
            let account = inspect_account(self, address, true)?;
            is_newly_created = account.is_created();
            debug_assert!(account.info.code_hash == KECCAK_EMPTY || account.info.code.is_some());
            account
        } else {
            // Non-REX4: is_created must be read on the original address (an EOA delegating
            // via 7702 is never CREATEd), but the storage account follows delegation —
            // genuinely two different accounts, so the two loads cannot be folded.
            is_newly_created = inspect_account(self, address, false)?.is_created();
            self.inspect_account_delegated(spec, address)?
        };
        // REX4/REX5 hot path: use entry() API for a single HashMap probe.
        // The prologue guarantees the account is in inner.state; reload via get_mut
        // to narrow the borrow from &mut self to &mut inner.state, keeping
        // self.database reachable for the miss path.
        if is_rex4_enabled {
            let account = self.inner.state.get_mut(&address).unwrap();
            return match account.storage.entry(key) {
                Entry::Occupied(entry) => Ok(entry.into_mut()),
                Entry::Vacant(entry) => {
                    let slot_value = if is_newly_created {
                        U256::ZERO
                    } else {
                        self.database.storage(address, key)?
                    };
                    let mut slot = EvmStorageSlot::new(slot_value, transaction_id);
                    slot.mark_cold();
                    Ok(entry.insert(slot))
                }
            };
        }

        // Pre-REX4: original contains_key + reload pattern (genuinely two different accounts).
        if account.storage.contains_key(&key) {
            // Need to reload account to satisfy borrow checker.
            let account = self.inspect_account_delegated(spec, address)?;
            return Ok(account.storage.get(&key).unwrap());
        }
        // Slot doesn't exist. For newly-created accounts, post-CREATE storage is
        // guaranteed empty (EIP-161 / EIP-6780), so return ZERO without touching the DB.
        let slot_value =
            if is_newly_created { U256::ZERO } else { self.database.storage(address, key)? };
        let mut slot = EvmStorageSlot::new(slot_value, transaction_id);
        // deliberately mark the slot as cold since we are only inspecting it, not warming it
        slot.mark_cold();
        // Load account again to bypass the borrow checker and insert the slot
        let account = self.inspect_account_delegated(spec, address)?;
        account.storage.insert(key, slot);
        // Return reference to the newly inserted slot
        Ok(account.storage.get(&key).expect("slot should exist"))
    }
}

/// `MegaContext` delegates to `Journal<DB>` and stashes DB errors via `self.error()`.
///
/// On DB error, the real error is stashed as `ContextError::Custom` and `Err(())` is returned.
/// Callers should halt with `FatalExternalError` when receiving `Err`.
impl<DB: Database, ExtEnvs: ExternalEnvTypes> JournalInspectTr for MegaContext<DB, ExtEnvs> {
    type DBError = ();

    fn inspect_account(&mut self, address: Address, load_code: bool) -> Result<&mut Account, ()> {
        let journal = &mut self.inner.journaled_state;
        let error = &mut self.inner.error;
        journal.inspect_account(address, load_code).map_err(|e| {
            *error = Err(ContextError::Custom(format!("{e}")));
        })
    }

    fn inspect_account_delegated(
        &mut self,
        spec: MegaSpecId,
        address: Address,
    ) -> Result<&mut Account, ()> {
        // Split borrow: `journaled_state` and `error` are sibling fields on the inner context,
        // so we can borrow them independently to avoid the double-call workaround.
        let journal = &mut self.inner.journaled_state;
        let error = &mut self.inner.error;
        journal.inspect_account_delegated(spec, address).map_err(|e| {
            *error = Err(ContextError::Custom(format!("{e}")));
        })
    }

    fn inspect_storage(
        &mut self,
        spec: MegaSpecId,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, ()> {
        let journal = &mut self.inner.journaled_state;
        let error = &mut self.inner.error;
        journal.inspect_storage(spec, address, key).map_err(|e| {
            *error = Err(ContextError::Custom(format!("{e}")));
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};
    use core::cell::Cell;
    use revm::{
        primitives::HashMap,
        state::{AccountInfo, Bytecode},
        Database,
    };

    /// Minimal `revm::Database` implementation that mimics the production
    /// `reth`-style `StateProviderDatabase` contract: `basic()` returns
    /// `AccountInfo { code: None, code_hash: <real hash> }` for accounts with
    /// on-chain bytecode, and the bytecode itself is lazy-loaded on demand via
    /// `code_by_hash()`. The workspace's `MemoryDatabase` cannot model this —
    /// it eagerly populates `AccountInfo.code` inside `basic()`, so any cache
    /// miss against it would always see the code already hydrated.
    #[derive(Default)]
    struct LazyCodeDatabase {
        accounts: HashMap<Address, AccountInfo>,
        codes: HashMap<B256, Bytecode>,
        storage_calls: Cell<usize>,
    }

    impl LazyCodeDatabase {
        fn with_account_code(mut self, address: Address, bytecode: Bytes) -> Self {
            let code = Bytecode::new_raw(bytecode);
            let code_hash = code.hash_slow();
            self.accounts.insert(
                address,
                AccountInfo { balance: U256::ZERO, nonce: 0, code_hash, code: None },
            );
            self.codes.insert(code_hash, code);
            self
        }

        fn with_eip7702_delegation(mut self, address: Address, delegate: Address) -> Self {
            let code = Bytecode::new_eip7702(delegate);
            let code_hash = code.hash_slow();
            self.accounts.insert(
                address,
                AccountInfo { balance: U256::ZERO, nonce: 0, code_hash, code: None },
            );
            self.codes.insert(code_hash, code);
            self
        }

        fn storage_calls(&self) -> usize {
            self.storage_calls.get()
        }
    }

    impl revm::Database for LazyCodeDatabase {
        type Error = core::convert::Infallible;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            // Mirror reth's `StateProviderDatabase::basic`: return AccountInfo without
            // populating `code`, even when the account has on-chain bytecode.
            Ok(self.accounts.get(&address).cloned())
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(self.codes.get(&code_hash).cloned().unwrap_or_default())
        }

        fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            self.storage_calls.set(self.storage_calls.get() + 1);
            Ok(U256::ZERO)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    /// `inspect_account(addr, false)` must not hydrate `info.code` on the vacant
    /// branch — callers that need it pass `load_code = true`.
    #[test]
    fn test_inspect_account_vacant_path_does_not_hydrate_code() {
        const ADDR: Address = address!("00000000000000000000000000000000000000aa");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]); // PUSH1 1 PUSH1 1 ADD
        let expected_hash = keccak256(&bytecode);

        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let account =
            inspect_account(&mut journal, ADDR, false).expect("inspect_account must succeed");

        assert_eq!(
            account.info.code_hash, expected_hash,
            "code_hash must propagate from the database's `basic()` result",
        );
        assert!(
            account.info.code.is_none(),
            "`load_code = false` must leave `info.code` as-is on the vacant branch",
        );
    }

    /// `inspect_account(addr, true)` hydrates `info.code` from `code_by_hash` on
    /// first cold inspection against a lazy-code database.
    #[test]
    fn test_inspect_account_with_load_code_hydrates_lazy_bytecode_on_first_touch() {
        const ADDR: Address = address!("00000000000000000000000000000000000000aa");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);

        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode.clone());
        let mut journal = Journal::new(db);

        let account = inspect_account(&mut journal, ADDR, true)
            .expect("inspect_account must succeed on first cold-touch");
        let hydrated = account
            .info
            .code
            .as_ref()
            .expect("`load_code = true` must populate `info.code` from code_by_hash");
        assert_eq!(
            hydrated.original_bytes().as_ref(),
            bytecode.as_ref(),
            "hydrated bytecode must match what `code_by_hash` would return",
        );
    }

    /// The occupied-branch hydration must keep firing — a second `inspect_account`
    /// against the same lazy-DB address must observe hydrated `info.code` even with
    /// `load_code = false`.
    #[test]
    fn test_inspect_account_occupied_branch_hydrates_on_second_inspection() {
        const ADDR: Address = address!("00000000000000000000000000000000000000bb");
        let bytecode = Bytes::from_static(&[0x5b]); // JUMPDEST
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let first_code_hash = inspect_account(&mut journal, ADDR, false)
            .expect("first inspection must succeed")
            .info
            .code_hash;
        let second =
            inspect_account(&mut journal, ADDR, false).expect("second inspection must succeed");

        assert_eq!(
            second.info.code_hash, first_code_hash,
            "code_hash must be identical across cache miss and cache hit",
        );
        assert!(
            second.info.code.is_some(),
            "second inspection must observe the hydrated code via the occupied-branch \
             `code_by_hash` load",
        );
    }

    /// `inspect_account(addr, true)` must short-circuit on EOAs — the `code_hash !=
    /// KECCAK_EMPTY` guard keeps `code_by_hash` off the hot path.
    #[test]
    fn test_inspect_account_with_load_code_leaves_eoa_code_empty() {
        const EOA: Address = address!("00000000000000000000000000000000000000cc");

        let mut db = LazyCodeDatabase::default();
        db.accounts.insert(
            EOA,
            AccountInfo {
                balance: U256::from(1_000_000u64),
                nonce: 5,
                code_hash: KECCAK_EMPTY,
                code: None,
            },
        );
        let mut journal = Journal::new(db);

        let account = inspect_account(&mut journal, EOA, true)
            .expect("inspect_account must succeed and be a no-op on EOAs");
        assert_eq!(account.info.code_hash, KECCAK_EMPTY, "EOA code_hash must remain KECCAK_EMPTY");
        assert!(
            account.info.code.is_none(),
            "EOA code must stay `None`; the `code_hash != KECCAK_EMPTY` guard keeps \
             `code_by_hash` off the hot path for accounts without on-chain code",
        );
    }

    /// `inspect_account_code_hash` returns the account's `code_hash` but must NEVER hydrate
    /// `info.code` — on either branch. In particular the occupied branch must not fall through to
    /// `code_by_hash` the way `inspect_account(.., false)` does (see
    /// `test_inspect_account_occupied_branch_hydrates_on_second_inspection`). This is what lets the
    /// REX6 keyless-deploy occupancy check decide on the hash alone, without demanding an
    /// already-warmed occupied address's bytecode in a stateless witness carrying only its proof.
    #[test]
    fn test_inspect_account_code_hash_never_hydrates_code() {
        const ADDR: Address = address!("00000000000000000000000000000000000000dd");
        let bytecode = Bytes::from_static(&[0x5b]); // JUMPDEST
        let expected_hash = keccak256(&bytecode);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        // Vacant cache-miss: returns the hash from `basic()` without hydrating code.
        let vacant_hash =
            inspect_account_code_hash(&mut journal, ADDR).expect("vacant read must succeed");
        assert_eq!(vacant_hash, expected_hash, "vacant branch must return the code_hash");
        assert!(
            journal.inner.state.get(&ADDR).is_some_and(|a| a.info.code.is_none()),
            "vacant branch must not hydrate info.code",
        );

        // The address is now resident with `code == None`. `inspect_account` would hydrate on this
        // occupied branch; `inspect_account_code_hash` must not.
        let occupied_hash =
            inspect_account_code_hash(&mut journal, ADDR).expect("occupied read must succeed");
        assert_eq!(
            occupied_hash, expected_hash,
            "occupied branch must return the cached code_hash"
        );
        assert!(
            journal.inner.state.get(&ADDR).is_some_and(|a| a.info.code.is_none()),
            "inspect_account_code_hash must NOT hydrate info.code on the occupied branch",
        );
    }

    /// `resolve_eip7702_delegate_address` hydrates lazy delegation bytecode only from REX5:
    /// pin both sides of the boundary so a shifted gate (e.g. REX4) fails here. At REX4 a
    /// lazy-code delegator resolves to itself (its `0xef0100…` code is never loaded); at REX5
    /// the same first cold touch loads the code and follows the hop.
    #[test]
    fn test_resolve_eip7702_delegate_loads_code_from_rex5_exactly() {
        const DELEGATOR: Address = address!("00000000000000000000000000000000000000e1");
        const DELEGATE: Address = address!("00000000000000000000000000000000000000e2");

        // REX4: no hydration — the delegation is invisible and resolution degrades to identity.
        let db = LazyCodeDatabase::default().with_eip7702_delegation(DELEGATOR, DELEGATE);
        let mut journal = Journal::new(db);
        let resolved = journal
            .resolve_eip7702_delegate_address(MegaSpecId::REX4, DELEGATOR)
            .expect("resolve must succeed");
        assert_eq!(resolved, DELEGATOR, "REX4 must not load code, so the hop is not followed");
        assert!(
            journal.inner.state.get(&DELEGATOR).is_some_and(|a| a.info.code.is_none()),
            "REX4 resolution must leave the delegator's code lazy",
        );

        // REX5: the first cold touch hydrates the delegation bytecode and follows the hop.
        let db = LazyCodeDatabase::default().with_eip7702_delegation(DELEGATOR, DELEGATE);
        let mut journal = Journal::new(db);
        let resolved = journal
            .resolve_eip7702_delegate_address(MegaSpecId::REX5, DELEGATOR)
            .expect("resolve must succeed");
        assert_eq!(resolved, DELEGATE, "REX5 must hydrate the code and follow the hop");
    }

    /// On REX5+, `inspect_account_delegated` must follow the EIP-7702 hop on the
    /// very first cold inspection against a lazy-code database. Regression guard:
    /// any refactor that re-introduces a code-None branch silently degrades the
    /// walk to "treat the delegator as a regular EOA".
    #[test]
    fn test_inspect_account_delegated_follows_eip7702_on_cold_first_touch() {
        use revm::context::JournalTr;

        const DELEGATOR: Address = address!("00000000000000000000000000000000000000d1");
        const DELEGATE: Address = address!("00000000000000000000000000000000000000d2");
        let delegate_bytecode = Bytes::from_static(&[0x60, 0x42, 0x60, 0x00, 0x55]); // PUSH1 0x42 PUSH1 0 SSTORE

        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(DELEGATOR, DELEGATE)
            .with_account_code(DELEGATE, delegate_bytecode.clone());

        let mut journal = Journal::new(db);

        let resolved = journal
            .inspect_account_delegated(MegaSpecId::REX5, DELEGATOR)
            .expect("inspect_account_delegated must succeed on a cold-cache first touch");

        // The resolved account must be the delegate, not the delegator. The only way
        // to distinguish them is the code: the delegator's code is the EIP-7702
        // designation pointing at DELEGATE; the delegate's code is the raw bytecode.
        let hydrated = resolved.info.code.as_ref().expect(
            "delegate's bytecode must be hydrated by the inner inspect_account call — \
             without the vacant-path hydration, the cold-touch on DELEGATE would leave \
             code as None and any subsequent EIP-7702 walk would see a wrongly-empty target",
        );
        assert!(
            !matches!(hydrated, Bytecode::Eip7702(_)),
            "resolved account must NOT be the delegator (whose code is the EIP-7702 \
             designation); got: {hydrated:?}",
        );
        assert_eq!(
            hydrated.original_bytes().as_ref(),
            delegate_bytecode.as_ref(),
            "resolved account's code must match the delegate's raw bytecode — confirms \
             the delegation was followed exactly one hop",
        );
    }

    /// Pre-REX5 `inspect_account_delegated` must NOT hydrate `info.code` — the
    /// latent EIP-7702 lazy-DB detection gap is the frozen observable behavior of
    /// stable specs (hydrating would flip `state_clear_aware_is_empty` and the
    /// SALT-bucket nonce in CALL/CREATE/state-growth, breaking spec immutability).
    #[test]
    fn test_inspect_account_delegated_does_not_hydrate_pre_rex5() {
        use revm::context::JournalTr;

        const DELEGATOR: Address = address!("00000000000000000000000000000000000000d1");
        const DELEGATE: Address = address!("00000000000000000000000000000000000000d2");
        let delegate_bytecode = Bytes::from_static(&[0x60, 0x42, 0x60, 0x00, 0x55]);

        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(DELEGATOR, DELEGATE)
            .with_account_code(DELEGATE, delegate_bytecode);

        let mut journal = Journal::new(db);

        // REX4 (pre-REX5): the one-hop walk runs but `code = None` against the
        // lazy DB hides the 7702 designation, so the walk returns the delegator.
        let resolved = journal
            .inspect_account_delegated(MegaSpecId::REX4, DELEGATOR)
            .expect("inspect_account_delegated must succeed on pre-REX5");

        assert!(
            resolved.info.code.is_none(),
            "pre-REX5: `inspect_account_delegated` must NOT hydrate `info.code` — the \
             latent lazy-DB EIP-7702 detection gap on stable specs is intentionally \
             preserved. Resolved account's code: {:?}",
            resolved.info.code,
        );
    }

    /// Pins the `LazyCodeDatabase` fixture's contract against the production
    /// `revm::Database` shape it is modeling: `basic()` returns
    /// `code: None` for known accounts, `None` for unknown addresses;
    /// `code_by_hash()` falls back to empty bytecode for an unknown hash;
    /// `storage()` and `block_hash()` are inert stubs (no tests exercise them via
    /// `inspect_account`, but they must remain wired so the fixture is a complete
    /// `revm::Database`). If the fixture ever drifts (e.g. someone "helpfully"
    /// makes `basic()` eagerly populate `code` like `MemoryDatabase` does), the
    /// inspect-account tests above silently lose their load-bearing property —
    /// this test fails fast in that case.
    #[test]
    fn test_lazy_code_database_fixture_pins_reth_style_contract() {
        const KNOWN: Address = address!("00000000000000000000000000000000000000ee");
        let bytecode = Bytes::from_static(&[0x00]);
        let mut db = LazyCodeDatabase::default().with_account_code(KNOWN, bytecode);

        let known = db.basic(KNOWN).unwrap().expect("known account must resolve");
        assert!(
            known.code.is_none(),
            "LazyCodeDatabase::basic must NOT pre-populate code — that is the \
             behavior `inspect_account` is being tested against",
        );
        assert!(
            db.basic(Address::ZERO).unwrap().is_none(),
            "unknown address must return None from basic()",
        );

        let unknown_hash = keccak256([0xffu8]);
        assert_eq!(
            db.code_by_hash(unknown_hash).unwrap().original_bytes().len(),
            0,
            "unknown code_hash must fall back to empty bytecode",
        );

        assert_eq!(db.storage(KNOWN, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(db.block_hash(0).unwrap(), B256::ZERO);
    }

    // === inspect_storage coverage tests (PR #334) ===

    #[test]
    fn test_inspect_storage_rex4_slot_hit_returns_existing_value() {
        const ADDR: Address = address!("00000000000000000000000000000000000000bb");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let key = U256::from(3);
        let expected_value = U256::from(42);
        {
            let tid = journal.transaction_id;
            let account = inspect_account(&mut journal, ADDR, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected_value, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let spec = MegaSpecId::REX4;
        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed on existing slot");

        assert_eq!(
            slot.present_value, expected_value,
            "REX4 slot hit must return the pre-seeded value"
        );
        assert!(slot.is_cold, "inspected slot must remain cold");
    }

    #[test]
    fn test_inspect_storage_rex4_slot_miss_inserts_and_returns_db_value() {
        const ADDR: Address = address!("00000000000000000000000000000000000000cc");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let key = U256::from(7);
        let spec = MegaSpecId::REX4;

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed on absent slot");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "absent slot on non-created account must return ZERO from database"
        );
        assert!(slot.is_cold, "newly inserted slot must be marked cold");

        let calls_after_first = journal.database.storage_calls();
        let slot2 =
            journal.inspect_storage(spec, ADDR, key).expect("second inspect_storage must succeed");

        assert_eq!(slot2.present_value, U256::ZERO, "second call must return the same value");
        assert_eq!(
            journal.database.storage_calls(),
            calls_after_first,
            "second inspect_storage on the same slot must hit the cache, not the DB",
        );
    }

    #[test]
    fn test_inspect_storage_rex4_newly_created_short_circuits_db() {
        const ADDR: Address = address!("00000000000000000000000000000000000000dd");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        {
            let account = inspect_account(&mut journal, ADDR, false).unwrap();
            account.mark_created();
        }

        let key = U256::from(1);
        let spec = MegaSpecId::REX4;

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed on newly-created account");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "newly-created account must return ZERO without querying database"
        );
        assert!(slot.is_cold, "slot must be marked cold");
    }

    #[test]
    fn test_inspect_storage_pre_rex4_uses_delegation_path() {
        const ADDR: Address = address!("00000000000000000000000000000000000000ee");
        const DELEGATE: Address = address!("00000000000000000000000000000000000000ed");
        let delegate_code = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(ADDR, DELEGATE)
            .with_account_code(DELEGATE, delegate_code);
        let mut journal = Journal::new(db);

        let key = U256::from(5);
        let expected = U256::from(123);
        let spec = MegaSpecId::MINI_REX;

        // Preload the delegator so the subsequent occupied-branch inspection hydrates the
        // EIP-7702 designation and the pre-REX4 delegated walk can follow it.
        let _ = inspect_account(&mut journal, ADDR, false).unwrap();
        {
            let tid = journal.transaction_id;
            let account = inspect_account(&mut journal, DELEGATE, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed pre-REX4");

        assert_eq!(
            slot.present_value, expected,
            "pre-REX4 storage inspection must follow the delegator to the delegate's slot",
        );
        assert!(slot.is_cold, "slot must be marked cold");
        assert_eq!(
            journal.database.storage_calls(),
            0,
            "delegate slot pre-seeded in the cache must be returned without hitting the DB",
        );
    }

    #[test]
    fn test_inspect_storage_pre_rex4_newly_created_short_circuits_db() {
        const ADDR: Address = address!("00000000000000000000000000000000000000ef");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        {
            let account = inspect_account(&mut journal, ADDR, false).unwrap();
            account.mark_created();
        }

        let key = U256::from(6);
        let spec = MegaSpecId::MINI_REX;

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed pre-REX4 on newly-created account");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "pre-REX4 newly-created account must return ZERO without querying database"
        );
        assert!(slot.is_cold, "slot must be marked cold");
        assert_eq!(
            journal.database.storage_calls(),
            0,
            "newly-created pre-REX4 path must not hit the database storage lookup",
        );
    }

    #[test]
    fn test_inspect_storage_rex4_ignores_eip7702_delegation() {
        const DELEGATOR: Address = address!("0000000000000000000000000000000000000d01");
        const DELEGATE: Address = address!("0000000000000000000000000000000000000d02");
        let delegate_code = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(DELEGATOR, DELEGATE)
            .with_account_code(DELEGATE, delegate_code);
        let mut journal = Journal::new(db);

        let key = U256::from(2);
        let expected = U256::from(99);
        {
            let tid = journal.transaction_id;
            let account = inspect_account(&mut journal, DELEGATOR, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let spec = MegaSpecId::REX4;
        let slot = journal
            .inspect_storage(spec, DELEGATOR, key)
            .expect("inspect_storage must succeed for REX4 delegator");

        assert_eq!(
            slot.present_value, expected,
            "REX4 must read storage from delegator (original address), not delegate"
        );
    }
}
