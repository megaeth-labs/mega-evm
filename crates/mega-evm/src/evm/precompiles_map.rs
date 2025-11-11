//! Helpers for dealing with Precompiles.
//!
//! This module is copied from alloy-evm to provide PrecompilesMap and related types.

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::{borrow::Cow, boxed::Box, string::String, sync::Arc};
use alloy_consensus::transaction::Either;
use alloy_evm::Database;
use alloy_primitives::{
    map::{HashMap, HashSet},
    Address, Bytes, Log, B256, U256,
};
use core::{error::Error, fmt, fmt::Debug};
use revm::{
    context::{Block, DBErrorMarker, JournalTr, LocalContextTr},
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{
        CallInput, Gas, InputsImpl, InstructionResult, InterpreterResult, SStoreResult, StateLoad,
    },
    precompile::{PrecompileError, PrecompileFn, PrecompileResult, Precompiles},
    primitives::{StorageKey, StorageValue},
    state::{Account, AccountInfo, Bytecode},
    Context, Journal,
};
#[cfg(feature = "std")]
use std::{borrow::Cow, boxed::Box, string::String, sync::Arc};

use crate::MegaSpecId;

/// Erased error type.
#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub struct ErasedError(Box<dyn Error + Send + Sync + 'static>);

impl ErasedError {
    /// Creates a new [`ErasedError`].
    pub fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self(Box::new(error))
    }
}

impl DBErrorMarker for ErasedError {}

/// Errors returned by [`EvmInternals`].
#[derive(Debug, thiserror::Error)]
pub enum EvmInternalsError {
    /// Database error.
    #[error(transparent)]
    Database(ErasedError),
}

impl EvmInternalsError {
    /// Creates a new [`EvmInternalsError::Database`]
    pub fn database(err: impl Error + Send + Sync + 'static) -> Self {
        Self::Database(ErasedError::new(err))
    }
}

/// dyn-compatible trait for accessing and modifying EVM internals, particularly the journal.
///
/// This trait provides an abstraction over journal operations without exposing
/// associated types, making it object-safe and suitable for dynamic dispatch.
trait EvmInternalsTr: Database<Error = ErasedError> + Debug {
    fn load_account(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, EvmInternalsError>;

    fn load_account_code(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, EvmInternalsError>;

    fn sload(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, EvmInternalsError>;

    fn touch_account(&mut self, address: Address);

    fn set_code(&mut self, address: Address, code: Bytecode);

    fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, EvmInternalsError>;

    fn log(&mut self, log: Log);
}

/// Helper internal struct for implementing [`EvmInternals`].
#[derive(Debug)]
struct EvmInternalsImpl<'a, T>(&'a mut T);

impl<T> revm::Database for EvmInternalsImpl<'_, T>
where
    T: JournalTr<Database: Database>,
{
    type Error = ErasedError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.db_mut().basic(address).map_err(ErasedError::new)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.0.db_mut().code_by_hash(code_hash).map_err(ErasedError::new)
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.0.db_mut().storage(address, index).map_err(ErasedError::new)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.0.db_mut().block_hash(number).map_err(ErasedError::new)
    }
}

impl<T> EvmInternalsTr for EvmInternalsImpl<'_, T>
where
    T: JournalTr<Database: Database> + Debug,
{
    fn load_account(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, EvmInternalsError> {
        self.0.load_account(address).map_err(EvmInternalsError::database)
    }

    fn load_account_code(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, EvmInternalsError> {
        self.0.load_account_code(address).map_err(EvmInternalsError::database)
    }

    fn sload(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, EvmInternalsError> {
        self.0.sload(address, key).map_err(EvmInternalsError::database)
    }

    fn touch_account(&mut self, address: Address) {
        self.0.touch_account(address);
    }

    fn set_code(&mut self, address: Address, code: Bytecode) {
        self.0.set_code(address, code);
    }

    fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, EvmInternalsError> {
        self.0.sstore(address, key, value).map_err(EvmInternalsError::database)
    }

    fn log(&mut self, log: Log) {
        self.0.log(log);
    }
}

/// Helper type exposing hooks into EVM and access to evm internal settings.
pub struct EvmInternals<'a> {
    internals: Box<dyn EvmInternalsTr + 'a>,
    block_env: &'a (dyn Block + 'a),
}

impl<'a> EvmInternals<'a> {
    /// Creates a new [`EvmInternals`] instance.
    pub(crate) fn new<T>(journal: &'a mut T, block_env: &'a dyn Block) -> Self
    where
        T: JournalTr<Database: Database> + Debug,
    {
        Self { internals: Box::new(EvmInternalsImpl(journal)), block_env }
    }

    /// Returns the  evm's block information.
    pub const fn block_env(&self) -> impl Block + 'a {
        self.block_env
    }

    /// Returns the current block number.
    pub fn block_number(&self) -> U256 {
        self.block_env.number()
    }

    /// Returns the current block timestamp.
    pub fn block_timestamp(&self) -> U256 {
        self.block_env.timestamp()
    }

    /// Returns a mutable reference to [`Database`] implementation with erased error type.
    ///
    /// Users should prefer using other methods for accessing state that rely on cached state in the
    /// journal instead.
    pub fn db_mut(&mut self) -> impl Database<Error = ErasedError> + '_ {
        &mut *self.internals
    }

    /// Loads an account.
    pub fn load_account(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, EvmInternalsError> {
        self.internals.load_account(address)
    }

    /// Loads code of an account.
    pub fn load_account_code(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, EvmInternalsError> {
        self.internals.load_account_code(address)
    }

    /// Loads a storage slot.
    pub fn sload(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, EvmInternalsError> {
        self.internals.sload(address, key)
    }

    /// Touches the account.
    pub fn touch_account(&mut self, address: Address) {
        self.internals.touch_account(address);
    }

    /// Sets bytecode to the account.
    pub fn set_code(&mut self, address: Address, code: Bytecode) {
        self.internals.set_code(address, code);
    }

    /// Stores the storage value in Journal state.
    pub fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, EvmInternalsError> {
        self.internals.sstore(address, key, value)
    }

    /// Logs the log in Journal state.
    pub fn log(&mut self, log: Log) {
        self.internals.log(log);
    }
}

impl<'a> fmt::Debug for EvmInternals<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvmInternals")
            .field("internals", &self.internals)
            .field("block_env", &"{{}}")
            .finish_non_exhaustive()
    }
}

/// A mapping of precompile contracts that can be either static (builtin) or dynamic.
///
/// This is an optimization that allows us to keep using the static precompiles
/// until we need to modify them, at which point we convert to the dynamic representation.
#[derive(Clone)]
pub struct PrecompilesMap {
    /// FPVM-accelerated precompiles that take priority over regular precompiles.
    accelerated_precompiles: HashMap<Address, DynPrecompile>,
    /// The wrapped precompiles in their current representation.
    precompiles: PrecompilesKind,
    /// An optional dynamic precompile loader that can lookup precompiles dynamically.
    lookup: Option<Arc<dyn PrecompileLookup>>,
}

impl PrecompilesMap {
    /// Creates the [`PrecompilesMap`] from a static reference.
    pub fn from_static(precompiles: &'static Precompiles) -> Self {
        Self::new(Cow::Borrowed(precompiles))
    }

    /// Creates a new set of precompiles for a spec.
    pub fn new(precompiles: Cow<'static, Precompiles>) -> Self {
        Self {
            accelerated_precompiles: HashMap::default(),
            precompiles: PrecompilesKind::Builtin(precompiles),
            lookup: None,
        }
    }

    /// Maps a precompile at the given address using the provided function.
    pub fn map_precompile<F>(&mut self, address: &Address, f: F)
    where
        F: FnOnce(DynPrecompile) -> DynPrecompile + Send + Sync + 'static,
    {
        let dyn_precompiles = self.ensure_dynamic_precompiles();

        // get the current precompile at the address
        if let Some(dyn_precompile) = dyn_precompiles.inner.remove(address) {
            // apply the transformation function
            let transformed = f(dyn_precompile);

            // update the precompile at the address
            dyn_precompiles.inner.insert(*address, transformed);
        }
    }

    /// Maps all precompiles using the provided function.
    pub fn map_precompiles<F>(&mut self, mut f: F)
    where
        F: FnMut(&Address, DynPrecompile) -> DynPrecompile,
    {
        let dyn_precompiles = self.ensure_dynamic_precompiles();

        // apply the transformation to each precompile
        let entries = dyn_precompiles.inner.drain();
        let mut new_map =
            HashMap::with_capacity_and_hasher(entries.size_hint().0, Default::default());
        for (addr, precompile) in entries {
            let transformed = f(&addr, precompile);
            new_map.insert(addr, transformed);
        }

        dyn_precompiles.inner = new_map;
    }

    /// Applies a transformation to the precompile at the given address.
    ///
    /// This method allows you to add, update, or remove a precompile by applying a closure
    /// to the existing precompile (if any) at the specified address.
    ///
    /// # Behavior
    ///
    /// The closure receives:
    /// - `Some(precompile)` if a precompile exists at the address
    /// - `None` if no precompile exists at the address
    ///
    /// Based on what the closure returns:
    /// - `Some(precompile)` - Insert or replace the precompile at the address
    /// - `None` - Remove the precompile from the address (if it exists)
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Add a new precompile
    /// precompiles.apply_precompile(&address, |_| Some(my_precompile));
    ///
    /// // Update an existing precompile
    /// precompiles.apply_precompile(&address, |existing| {
    ///     existing.map(|p| wrap_with_logging(p))
    /// });
    ///
    /// // Remove a precompile
    /// precompiles.apply_precompile(&address, |_| None);
    ///
    /// // Conditionally update
    /// precompiles.apply_precompile(&address, |existing| {
    ///     if let Some(p) = existing {
    ///         Some(modify_precompile(p))
    ///     } else {
    ///         Some(create_default_precompile())
    ///     }
    /// });
    /// ```
    pub fn apply_precompile<F>(&mut self, address: &Address, f: F)
    where
        F: FnOnce(Option<DynPrecompile>) -> Option<DynPrecompile>,
    {
        let dyn_precompiles = self.ensure_dynamic_precompiles();
        let current = dyn_precompiles.inner.get(address).cloned();

        // apply the transformation function
        let result = f(current);

        match result {
            Some(transformed) => {
                // insert the transformed precompile
                dyn_precompiles.inner.insert(*address, transformed);
                dyn_precompiles.addresses.insert(*address);
            }
            None => {
                // remove the precompile if the transformation returned None
                dyn_precompiles.inner.remove(address);
                dyn_precompiles.addresses.remove(address);
            }
        }
    }

    /// Builder-style method that maps a precompile at the given address using the provided
    /// function.
    ///
    /// This is a consuming version of [`map_precompile`](Self::map_precompile) that returns `Self`.
    pub fn with_mapped_precompile<F>(mut self, address: &Address, f: F) -> Self
    where
        F: FnOnce(DynPrecompile) -> DynPrecompile + Send + Sync + 'static,
    {
        self.map_precompile(address, f);
        self
    }

    /// Builder-style method that maps all precompiles using the provided function.
    ///
    /// This is a consuming version of [`map_precompiles`](Self::map_precompiles) that returns
    /// `Self`.
    pub fn with_mapped_precompiles<F>(mut self, f: F) -> Self
    where
        F: FnMut(&Address, DynPrecompile) -> DynPrecompile,
    {
        self.map_precompiles(f);
        self
    }

    /// Builder-style method that applies a transformation to the precompile at the given address.
    ///
    /// This is a consuming version of [`apply_precompile`](Self::apply_precompile) that returns
    /// `Self`. See [`apply_precompile`](Self::apply_precompile) for detailed behavior and
    /// examples.
    pub fn with_applied_precompile<F>(mut self, address: &Address, f: F) -> Self
    where
        F: FnOnce(Option<DynPrecompile>) -> Option<DynPrecompile>,
    {
        self.apply_precompile(address, f);
        self
    }

    /// Sets a dynamic precompile lookup function that is called for addresses not found
    /// in the static precompile map.
    ///
    /// This method allows you to provide runtime-resolved precompiles that aren't known
    /// at initialization time. The lookup function is called whenever a precompile check
    /// is performed for an address that doesn't exist in the main precompile map.
    ///
    /// # Important Notes
    ///
    /// - **Priority**: Static precompiles take precedence. The lookup function is only called if
    ///   the address is not found in the main precompile map.
    /// - **Gas accounting**: Addresses resolved through this lookup are always treated as cold,
    ///   meaning they incur cold access costs even on repeated calls within the same transaction.
    ///   See also [`PrecompileProvider::warm_addresses`].
    /// - **Performance**: The lookup function is called on every precompile check for
    ///   non-registered addresses, so it should be efficient.
    ///
    /// # Example
    ///
    /// ```ignore
    /// precompiles.set_precompile_lookup(|address| {
    ///     // Dynamically resolve precompiles based on address pattern
    ///     if address.as_slice().starts_with(&[0xDE, 0xAD]) {
    ///         Some(DynPrecompile::new(|input| {
    ///             // Custom precompile logic
    ///             Ok(PrecompileOutput {
    ///                 gas_used: 100,
    ///                 bytes: Bytes::from("dynamic precompile"),
    ///             })
    ///         }))
    ///     } else {
    ///         None
    ///     }
    /// });
    /// ```
    pub fn set_precompile_lookup<L>(&mut self, lookup: L)
    where
        L: PrecompileLookup + 'static,
    {
        self.lookup = Some(Arc::new(lookup));
    }

    /// Builder-style method to set a dynamic precompile lookup function.
    ///
    /// This is a consuming version of [`set_precompile_lookup`](Self::set_precompile_lookup)
    /// that returns `Self` for method chaining.
    ///
    /// See [`set_precompile_lookup`](Self::set_precompile_lookup) for detailed behavior,
    /// important notes, and examples.
    pub fn with_precompile_lookup<L>(mut self, lookup: L) -> Self
    where
        L: PrecompileLookup + 'static,
    {
        self.set_precompile_lookup(lookup);
        self
    }

    /// Builder-style method to add an accelerated precompile.
    ///
    /// This method adds an FPVM-accelerated precompile that takes priority over regular
    /// precompiles. Accelerated precompiles are checked first when resolving precompile calls.
    ///
    /// Returns `Self` for method chaining, allowing multiple accelerated precompiles to be
    /// added in a single expression.
    ///
    /// # Example
    ///
    /// ```ignore
    /// precompiles
    ///     .with_accelerated_precompile(addr1, precompile1)
    ///     .with_accelerated_precompile(addr2, precompile2)
    ///     .with_accelerated_precompile(addr3, precompile3)
    /// ```
    pub fn with_accelerated_precompile(
        mut self,
        address: Address,
        precompile: DynPrecompile,
    ) -> Self {
        self.accelerated_precompiles.insert(address, precompile);
        self
    }

    /// Ensures that precompiles are in their dynamic representation.
    /// If they are already dynamic, this is a no-op.
    /// Returns a mutable reference to the dynamic precompiles.
    pub fn ensure_dynamic_precompiles(&mut self) -> &mut DynPrecompiles {
        if let PrecompilesKind::Builtin(ref precompiles_cow) = self.precompiles {
            let mut dynamic = DynPrecompiles::default();

            let static_precompiles = match precompiles_cow {
                Cow::Borrowed(static_ref) => static_ref,
                Cow::Owned(owned) => owned,
            };

            for (addr, precompile_fn) in
                static_precompiles.inner().iter().map(|(addr, f)| (addr, *f))
            {
                let precompile =
                    move |input: PrecompileInput<'_>| precompile_fn(input.data, input.gas);
                dynamic.inner.insert(*addr, precompile.into());
                dynamic.addresses.insert(*addr);
            }

            self.precompiles = PrecompilesKind::Dynamic(dynamic);
        }

        match &mut self.precompiles {
            PrecompilesKind::Dynamic(dynamic) => dynamic,
            _ => unreachable!("We just ensured that this is a Dynamic variant"),
        }
    }

    /// Returns an iterator over references to precompile addresses.
    pub fn addresses(&self) -> impl Iterator<Item = &Address> {
        match &self.precompiles {
            PrecompilesKind::Builtin(precompiles) => Either::Left(precompiles.addresses()),
            PrecompilesKind::Dynamic(dyn_precompiles) => {
                Either::Right(dyn_precompiles.addresses.iter())
            }
        }
    }

    /// Gets a reference to the precompile at the given address.
    ///
    /// Priority order:
    /// 1. Accelerated precompiles (highest priority)
    /// 2. Regular precompiles (static or dynamic)
    /// 3. Dynamic lookup function (lowest priority)
    pub fn get(&self, address: &Address) -> Option<impl Precompile + '_> {
        // First check accelerated precompiles (highest priority)
        if let Some(accelerated) = self.accelerated_precompiles.get(address) {
            return Some(Either::Right(accelerated.clone()));
        }

        // Then check regular precompiles
        let static_result = match &self.precompiles {
            PrecompilesKind::Builtin(precompiles) => precompiles
                .get(address)
                .map(|f| Either::Left(|input: PrecompileInput<'_>| f(input.data, input.gas))),
            PrecompilesKind::Dynamic(dyn_precompiles) => {
                dyn_precompiles.inner.get(address).map(Either::Right)
            }
        };

        // If found in regular precompiles, return it
        if let Some(precompile) = static_result {
            return Some(Either::Left(precompile));
        }

        // Finally, try the lookup function if available
        // Lookup returns owned DynPrecompile, wrap it in Either::Right
        let lookup = self.lookup.as_ref()?;
        lookup.lookup(address).map(Either::Right)
    }

    /// Sets the accelerated precompiles.
    /// This will overwrite the existing accelerated precompiles.
    pub fn set_accelerated_precompiles(
        &mut self,
        accelerated_precompiles: HashMap<Address, DynPrecompile>,
    ) -> &mut Self {
        self.accelerated_precompiles = accelerated_precompiles;
        self
    }
}

impl From<EthPrecompiles> for PrecompilesMap {
    fn from(value: EthPrecompiles) -> Self {
        Self::from_static(value.precompiles)
    }
}

impl core::fmt::Debug for PrecompilesMap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.precompiles {
            PrecompilesKind::Builtin(_) => {
                let mut debug_struct = f.debug_struct("PrecompilesMap::Builtin");
                if !self.accelerated_precompiles.is_empty() {
                    let accelerated_addresses: Vec<_> =
                        self.accelerated_precompiles.keys().collect();
                    debug_struct.field("accelerated_addresses", &accelerated_addresses);
                }
                debug_struct.finish()
            }
            PrecompilesKind::Dynamic(precompiles) => {
                let addresses = &precompiles.addresses;
                let accelerated_addresses: Option<Vec<Address>> =
                    if !self.accelerated_precompiles.is_empty() {
                        Some(self.accelerated_precompiles.keys().copied().collect())
                    } else {
                        None
                    };
                let mut debug_struct = f.debug_struct("PrecompilesMap::Dynamic");
                debug_struct.field("addresses", addresses);
                if let Some(ref accelerated) = accelerated_addresses {
                    debug_struct.field("accelerated_addresses", accelerated);
                }
                debug_struct.finish()
            }
        }
    }
}

impl<BlockEnv, TxEnv, CfgEnv, DB, Chain>
    PrecompileProvider<Context<BlockEnv, TxEnv, CfgEnv, DB, Journal<DB>, Chain>> for PrecompilesMap
where
    BlockEnv: revm::context::Block,
    TxEnv: revm::context::Transaction,
    CfgEnv: revm::context::Cfg,
    DB: Database,
{
    type Output = InterpreterResult;

    fn set_spec(&mut self, _spec: CfgEnv::Spec) -> bool {
        false
    }

    fn run(
        &mut self,
        context: &mut Context<BlockEnv, TxEnv, CfgEnv, DB, Journal<DB>, Chain>,
        address: &Address,
        inputs: &InputsImpl,
        _is_static: bool,
        gas_limit: u64,
    ) -> Result<Option<InterpreterResult>, String> {
        // Priority:
        // 1. If the precompile has an accelerated version, use that.
        // 2. If the precompile is not accelerated, use the default version.
        // 3. If the precompile is not found, return None.
        // Get the precompile at the address (get() already checks accelerated_precompiles first)
        let Some(precompile) = self.get(address) else {
            return Ok(None);
        };

        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(gas_limit),
            output: Bytes::new(),
        };

        let (local, journal) = (&context.local, &mut context.journaled_state);

        // Execute the precompile
        let r;
        let input_bytes = match &inputs.input {
            CallInput::SharedBuffer(range) => {
                // `map_or` does not work here as we use `r` to extend lifetime of the slice
                // and return it.
                #[allow(clippy::option_if_let_else)]
                if let Some(slice) = local.shared_memory_buffer_slice(range.clone()) {
                    r = slice;
                    &*r
                } else {
                    &[]
                }
            }
            CallInput::Bytes(bytes) => bytes.as_ref(),
        };

        let precompile_result = precompile.call(PrecompileInput {
            data: input_bytes,
            gas: gas_limit,
            caller: inputs.caller_address,
            value: inputs.call_value,
            internals: EvmInternals::new(journal, &context.block),
        });

        match precompile_result {
            Ok(output) => {
                let underflow = result.gas.record_cost(output.gas_used);
                assert!(underflow, "Gas underflow is not possible");
                result.result = InstructionResult::Return;
                result.output = output.bytes;
            }
            Err(PrecompileError::Fatal(e)) => return Err(e),
            Err(e) => {
                result.result = if e.is_oog() {
                    InstructionResult::PrecompileOOG
                } else {
                    InstructionResult::PrecompileError
                };
            }
        };

        Ok(Some(result))
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        Box::new(self.addresses().copied())
    }

    fn contains(&self, address: &Address) -> bool {
        self.get(address).is_some()
    }
}

/// A function that creates a mapping of accelerated precompiles for a given [`MegaSpecId`].
#[derive(Clone)]
pub struct AcceleratedPrecompileCreator(
    Arc<dyn Fn(MegaSpecId) -> HashMap<Address, DynPrecompile> + Send + Sync>,
);

impl AcceleratedPrecompileCreator {
    /// Creates a new [`AcceleratedPrecompileCreator`] with the given function.
    ///
    /// # Parameters
    ///
    /// - `f`: The function to create the accelerated precompiles for
    ///
    /// # Returns
    ///
    /// A new [`AcceleratedPrecompileCreator`] with the given function.
    pub fn new(
        f: Arc<dyn Fn(MegaSpecId) -> HashMap<Address, DynPrecompile> + Send + Sync>,
    ) -> Self {
        Self(f)
    }

    /// Calls the accelerated precompile creator with the given [`MegaSpecId`].
    ///
    /// # Parameters
    ///
    /// - `spec`: The [`MegaSpecId`] to create the accelerated precompiles for
    ///
    /// # Returns
    ///
    /// A mapping of accelerated precompiles for the given [`MegaSpecId`].
    pub fn call(&self, spec: MegaSpecId) -> HashMap<Address, DynPrecompile> {
        self.0(spec)
    }
}

impl core::fmt::Debug for AcceleratedPrecompileCreator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AcceleratedPrecompileCreator").finish()
    }
}

/// A mapping of precompile contracts that can be either static (builtin) or dynamic.
///
/// This is an optimization that allows us to keep using the static precompiles
/// until we need to modify them, at which point we convert to the dynamic representation.
#[derive(Clone)]
enum PrecompilesKind {
    /// Static builtin precompiles.
    Builtin(Cow<'static, Precompiles>),
    /// Dynamic precompiles that can be modified at runtime.
    Dynamic(DynPrecompiles),
}

/// A dynamic precompile implementation that can be modified at runtime.
#[derive(Clone)]
pub struct DynPrecompile(pub(crate) Arc<dyn Precompile + Send + Sync>);

impl DynPrecompile {
    /// Creates a new [`DynPrecompiles`] with the given closure.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(PrecompileInput<'_>) -> PrecompileResult + Send + Sync + 'static,
    {
        Self(Arc::new(f))
    }

    /// Creates a new [`DynPrecompiles`] with the given closure and [`Precompile::is_pure`]
    /// returning `false`.
    pub fn new_stateful<F>(f: F) -> Self
    where
        F: Fn(PrecompileInput<'_>) -> PrecompileResult + Send + Sync + 'static,
    {
        Self(Arc::new(StatefulPrecompile(f)))
    }

    /// Flips [`Precompile::is_pure`] to `false`.
    pub fn stateful(self) -> Self {
        Self(Arc::new(StatefulPrecompile(self.0)))
    }
}

impl core::fmt::Debug for DynPrecompile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DynPrecompile").finish()
    }
}

/// A mutable representation of precompiles that allows for runtime modification.
///
/// This structure stores dynamic precompiles that can be modified at runtime,
/// unlike the static `Precompiles` struct from revm.
#[derive(Clone, Default)]
pub struct DynPrecompiles {
    /// Precompiles
    inner: HashMap<Address, DynPrecompile>,
    /// Addresses of precompile
    addresses: HashSet<Address>,
}

impl core::fmt::Debug for DynPrecompiles {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DynPrecompiles").field("addresses", &self.addresses).finish()
    }
}

/// Input for a precompile call.
#[derive(Debug)]
pub struct PrecompileInput<'a> {
    /// Input data bytes.
    pub data: &'a [u8],
    /// Gas limit.
    pub gas: u64,
    /// Caller address.
    pub caller: Address,
    /// Value sent with the call.
    pub value: U256,
    /// Various hooks for interacting with the EVM state.
    pub internals: EvmInternals<'a>,
}

/// Trait for implementing precompiled contracts.
#[auto_impl::auto_impl(Arc)]
pub trait Precompile {
    /// Execute the precompile with the given input data, gas limit, and caller address.
    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult;

    /// Returns whether the precompile is pure.
    ///
    /// A pure precompile has deterministic output based solely on its input.
    /// Non-pure precompiles may produce different outputs for the same input
    /// based on the current state or other external factors.
    ///
    /// # Default
    ///
    /// Returns `true` by default, indicating the precompile is pure
    /// and its results should be cached as this is what most of the precompiles are.
    ///
    /// # Examples
    ///
    /// Override this method to return `false` for non-deterministic precompiles:
    ///
    /// ```ignore
    /// impl Precompile for MyDeterministicPrecompile {
    ///     fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
    ///         // non-deterministic computation dependent on state
    ///     }
    ///
    ///     fn is_pure(&self) -> bool {
    ///         false // This precompile might produce different output for the same input
    ///     }
    /// }
    /// ```
    fn is_pure(&self) -> bool {
        true
    }
}

impl<F> Precompile for F
where
    F: Fn(PrecompileInput<'_>) -> PrecompileResult + Send + Sync,
{
    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        self(input)
    }
}

impl<F> From<F> for DynPrecompile
where
    F: Fn(PrecompileInput<'_>) -> PrecompileResult + Send + Sync + 'static,
{
    fn from(f: F) -> Self {
        Self(Arc::new(f))
    }
}

impl From<PrecompileFn> for DynPrecompile {
    fn from(f: PrecompileFn) -> Self {
        let p = move |input: PrecompileInput<'_>| f(input.data, input.gas);
        p.into()
    }
}

impl Precompile for DynPrecompile {
    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        self.0.call(input)
    }

    fn is_pure(&self) -> bool {
        self.0.is_pure()
    }
}

impl Precompile for &DynPrecompile {
    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        self.0.call(input)
    }

    fn is_pure(&self) -> bool {
        self.0.is_pure()
    }
}

impl<A: Precompile, B: Precompile> Precompile for Either<A, B> {
    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        match self {
            Self::Left(p) => p.call(input),
            Self::Right(p) => p.call(input),
        }
    }

    fn is_pure(&self) -> bool {
        match self {
            Self::Left(p) => p.is_pure(),
            Self::Right(p) => p.is_pure(),
        }
    }
}

struct StatefulPrecompile<P>(P);

impl<P: Precompile> Precompile for StatefulPrecompile<P> {
    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        self.0.call(input)
    }

    fn is_pure(&self) -> bool {
        false
    }
}

/// Trait for dynamically resolving precompile contracts.
///
/// This trait allows for runtime resolution of precompiles that aren't known
/// at initialization time.
pub trait PrecompileLookup: Send + Sync {
    /// Looks up a precompile at the given address.
    ///
    /// Returns `Some(precompile)` if a precompile exists at the address,
    /// or `None` if no precompile is found.
    fn lookup(&self, address: &Address) -> Option<DynPrecompile>;
}

/// Implement PrecompileLookup for closure types
impl<F> PrecompileLookup for F
where
    F: Fn(&Address) -> Option<DynPrecompile> + Send + Sync,
{
    fn lookup(&self, address: &Address) -> Option<DynPrecompile> {
        self(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefaultExternalEnvs, MegaContext, MegaSpecId};
    use alloy_primitives::{address, Bytes};
    use revm::{context::Block, database::EmptyDB, precompile::PrecompileOutput};

    #[test]
    fn test_map_precompile() {
        let eth_precompiles = EthPrecompiles::default();
        let mut spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let ext_envs = DefaultExternalEnvs::default();
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE, &ext_envs);

        // create a test input for the precompile (identity precompile)
        let identity_address = address!("0x0000000000000000000000000000000000000004");
        let test_input = Bytes::from_static(b"test data");
        let gas_limit = 1000;

        // Ensure we're using dynamic precompiles
        spec_precompiles.ensure_dynamic_precompiles();

        // using the dynamic precompiles interface
        let dyn_precompile = match &spec_precompiles.precompiles {
            PrecompilesKind::Dynamic(dyn_precompiles) => {
                dyn_precompiles.inner.get(&identity_address).unwrap()
            }
            _ => panic!("Expected dynamic precompiles"),
        };

        let result = dyn_precompile
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(result.bytes, test_input, "Identity precompile should return the input data");

        // define a function to modify the precompile
        // this will change the identity precompile to always return a fixed value
        let constant_bytes = Bytes::from_static(b"constant value");

        // define a function to modify the precompile to always return a constant value
        spec_precompiles.map_precompile(&identity_address, move |_original_dyn| {
            // create a new DynPrecompile that always returns our constant
            |_input: PrecompileInput<'_>| -> PrecompileResult {
                Ok(PrecompileOutput {
                    gas_used: 10,
                    bytes: Bytes::from_static(b"constant value"),
                    reverted: false,
                })
            }
            .into()
        });

        // get the modified precompile and check it
        let dyn_precompile = match &spec_precompiles.precompiles {
            PrecompilesKind::Dynamic(dyn_precompiles) => {
                dyn_precompiles.inner.get(&identity_address).unwrap()
            }
            _ => panic!("Expected dynamic precompiles"),
        };

        let result = dyn_precompile
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(
            result.bytes, constant_bytes,
            "Modified precompile should return the constant value"
        );
    }

    #[test]
    fn test_closure_precompile() {
        let test_input = Bytes::from_static(b"test data");
        let expected_output = Bytes::from_static(b"processed: test data");
        let gas_limit = 1000;

        let ext_envs = DefaultExternalEnvs::default();
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE, &ext_envs);

        // define a closure that implements the precompile functionality
        let closure_precompile = |input: PrecompileInput<'_>| -> PrecompileResult {
            let _timestamp = input.internals.block_env().timestamp();
            let mut output = b"processed: ".to_vec();
            output.extend_from_slice(input.data.as_ref());
            Ok(PrecompileOutput { gas_used: 15, bytes: Bytes::from(output), reverted: false })
        };

        let dyn_precompile: DynPrecompile = closure_precompile.into();

        let result = dyn_precompile
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(result.gas_used, 15);
        assert_eq!(result.bytes, expected_output);
    }

    #[test]
    fn test_is_pure() {
        // Test default behavior (should be false)
        let closure_precompile = |_input: PrecompileInput<'_>| -> PrecompileResult {
            Ok(PrecompileOutput {
                gas_used: 10,
                bytes: Bytes::from_static(b"output"),
                reverted: false,
            })
        };

        let dyn_precompile: DynPrecompile = closure_precompile.into();
        assert!(dyn_precompile.is_pure(), "should be pure by default");

        // Test custom precompile with overridden is_pure
        let stateful_precompile = DynPrecompile::new_stateful(closure_precompile);
        assert!(!stateful_precompile.is_pure(), "PurePrecompile should return true for is_pure");

        let either_left = Either::<DynPrecompile, DynPrecompile>::Left(stateful_precompile);
        assert!(!either_left.is_pure(), "Either::Left with non-pure should return false");

        let either_right = Either::<DynPrecompile, DynPrecompile>::Right(dyn_precompile);
        assert!(either_right.is_pure(), "Either::Right with pure should return true");
    }

    #[test]
    fn test_precompile_lookup() {
        let eth_precompiles = EthPrecompiles::default();
        let mut spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let ext_envs = DefaultExternalEnvs::default();
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE, &ext_envs);

        // Define a custom address pattern for dynamic precompiles
        let dynamic_prefix = [0xDE, 0xAD];

        // Set up the lookup function
        spec_precompiles.set_precompile_lookup(move |address: &Address| {
            if address.as_slice().starts_with(&dynamic_prefix) {
                Some(DynPrecompile::new(|_input| {
                    Ok(PrecompileOutput {
                        gas_used: 100,
                        bytes: Bytes::from("dynamic precompile response"),
                        reverted: false,
                    })
                }))
            } else {
                None
            }
        });

        // Test that static precompiles still work
        let identity_address = address!("0x0000000000000000000000000000000000000004");
        assert!(spec_precompiles.get(&identity_address).is_some());

        // Test dynamic lookup for matching address
        let dynamic_address = address!("0xDEAD000000000000000000000000000000000001");
        let dynamic_precompile = spec_precompiles.get(&dynamic_address);
        assert!(dynamic_precompile.is_some(), "Dynamic precompile should be found");

        // Execute the dynamic precompile
        let result = dynamic_precompile
            .unwrap()
            .call(PrecompileInput {
                data: &[],
                gas: 1000,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(result.gas_used, 100);
        assert_eq!(result.bytes, Bytes::from("dynamic precompile response"));

        // Test non-matching address returns None
        let non_matching_address = address!("0x1234000000000000000000000000000000000001");
        assert!(spec_precompiles.get(&non_matching_address).is_none());
    }

    #[test]
    fn test_get_precompile() {
        let eth_precompiles = EthPrecompiles::default();
        let spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let ext_envs = DefaultExternalEnvs::default();
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE, &ext_envs);

        let identity_address = address!("0x0000000000000000000000000000000000000004");
        let test_input = Bytes::from_static(b"test data");
        let gas_limit = 1000;

        let precompile = spec_precompiles.get(&identity_address);
        assert!(precompile.is_some(), "Identity precompile should exist");

        let result = precompile
            .unwrap()
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(result.bytes, test_input, "Identity precompile should return the input data");

        let nonexistent_address = address!("0x0000000000000000000000000000000000000099");
        assert!(
            spec_precompiles.get(&nonexistent_address).is_none(),
            "Non-existent precompile should not be found"
        );

        let mut dynamic_precompiles = spec_precompiles;
        dynamic_precompiles.ensure_dynamic_precompiles();

        let dyn_precompile = dynamic_precompiles.get(&identity_address);
        assert!(
            dyn_precompile.is_some(),
            "Identity precompile should exist after conversion to dynamic"
        );

        let result = dyn_precompile
            .unwrap()
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(
            result.bytes, test_input,
            "Identity precompile should return the input data after conversion to dynamic"
        );
    }

    #[test]
    fn test_with_accelerated_precompile() {
        let eth_precompiles = EthPrecompiles::default();
        let custom_address1 = address!("0x0000000000000000000000000000000000000001");
        let custom_address2 = address!("0x0000000000000000000000000000000000000002");

        // Create accelerated precompiles
        let accelerated_precompile1 = DynPrecompile::new(|_input| {
            Ok(PrecompileOutput {
                gas_used: 50,
                bytes: Bytes::from_static(b"accelerated1"),
                reverted: false,
            })
        });

        let accelerated_precompile2 = DynPrecompile::new(|_input| {
            Ok(PrecompileOutput {
                gas_used: 60,
                bytes: Bytes::from_static(b"accelerated2"),
                reverted: false,
            })
        });

        // Test chainable builder method
        let spec_precompiles = PrecompilesMap::from(eth_precompiles)
            .with_accelerated_precompile(custom_address1, accelerated_precompile1.clone())
            .with_accelerated_precompile(custom_address2, accelerated_precompile2.clone());

        // Verify accelerated precompiles are added
        let ext_envs = DefaultExternalEnvs::default();
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE, &ext_envs);

        let precompile1 = spec_precompiles.get(&custom_address1);
        assert!(precompile1.is_some(), "Accelerated precompile 1 should be found");

        let result1 = precompile1
            .unwrap()
            .call(PrecompileInput {
                data: &[],
                gas: 1000,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(result1.bytes, Bytes::from_static(b"accelerated1"));
        assert_eq!(result1.gas_used, 50);

        let precompile2 = spec_precompiles.get(&custom_address2);
        assert!(precompile2.is_some(), "Accelerated precompile 2 should be found");

        let result2 = precompile2
            .unwrap()
            .call(PrecompileInput {
                data: &[],
                gas: 1000,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(result2.bytes, Bytes::from_static(b"accelerated2"));
        assert_eq!(result2.gas_used, 60);
    }

    #[test]
    fn test_accelerated_precompile_priority() {
        let eth_precompiles = EthPrecompiles::default();
        let identity_address = address!("0x0000000000000000000000000000000000000004");
        let test_input = Bytes::from_static(b"test data");

        // Create an accelerated precompile that overrides the identity precompile
        let accelerated_identity = DynPrecompile::new(|_input| {
            Ok(PrecompileOutput {
                gas_used: 20,
                bytes: Bytes::from_static(b"accelerated identity"),
                reverted: false,
            })
        });

        let spec_precompiles = PrecompilesMap::from(eth_precompiles)
            .with_accelerated_precompile(identity_address, accelerated_identity);

        let ext_envs = DefaultExternalEnvs::default();
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE, &ext_envs);

        // Get the precompile - should return accelerated version
        let precompile = spec_precompiles.get(&identity_address);
        assert!(precompile.is_some(), "Precompile should be found");

        let result = precompile
            .unwrap()
            .call(PrecompileInput {
                data: &test_input,
                gas: 1000,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();

        // Should return accelerated version, not the regular identity precompile
        assert_eq!(result.bytes, Bytes::from_static(b"accelerated identity"));
        assert_eq!(result.gas_used, 20);
    }

    #[test]
    fn test_accelerated_precompile_priority_over_lookup() {
        let eth_precompiles = EthPrecompiles::default();
        let custom_address = address!("0xDEAD000000000000000000000000000000000001");

        // Create accelerated precompile
        let accelerated_precompile = DynPrecompile::new(|_input| {
            Ok(PrecompileOutput {
                gas_used: 30,
                bytes: Bytes::from_static(b"accelerated"),
                reverted: false,
            })
        });

        // Create lookup function that would return a different precompile
        let lookup_precompile = DynPrecompile::new(|_input| {
            Ok(PrecompileOutput {
                gas_used: 40,
                bytes: Bytes::from_static(b"lookup"),
                reverted: false,
            })
        });

        let spec_precompiles = PrecompilesMap::from(eth_precompiles)
            .with_accelerated_precompile(custom_address, accelerated_precompile)
            .with_precompile_lookup(move |address: &Address| {
                if address == &custom_address {
                    Some(lookup_precompile.clone())
                } else {
                    None
                }
            });

        let ext_envs = DefaultExternalEnvs::default();
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE, &ext_envs);

        // Get the precompile - should return accelerated version, not lookup version
        let precompile = spec_precompiles.get(&custom_address);
        assert!(precompile.is_some(), "Precompile should be found");

        let result = precompile
            .unwrap()
            .call(PrecompileInput {
                data: &[],
                gas: 1000,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();

        // Should return accelerated version, not lookup version
        assert_eq!(result.bytes, Bytes::from_static(b"accelerated"));
        assert_eq!(result.gas_used, 30);
    }

    #[test]
    fn test_accelerated_precompile_with_regular_precompile() {
        let eth_precompiles = EthPrecompiles::default();
        let identity_address = address!("0x0000000000000000000000000000000000000004");
        let new_address = address!("0x0000000000000000000000000000000000000005");

        // Create accelerated precompile for a new address
        let accelerated_precompile = DynPrecompile::new(|input| {
            let mut output = b"accelerated: ".to_vec();
            output.extend_from_slice(input.data);
            Ok(PrecompileOutput { gas_used: 25, bytes: Bytes::from(output), reverted: false })
        });

        let spec_precompiles = PrecompilesMap::from(eth_precompiles)
            .with_accelerated_precompile(new_address, accelerated_precompile);

        let ext_envs = DefaultExternalEnvs::default();
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE, &ext_envs);

        // Test that regular precompiles still work
        let identity_precompile = spec_precompiles.get(&identity_address);
        assert!(identity_precompile.is_some(), "Regular identity precompile should still work");

        let test_input = Bytes::from_static(b"test");
        let result = identity_precompile
            .unwrap()
            .call(PrecompileInput {
                data: &test_input,
                gas: 1000,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(result.bytes, test_input, "Regular precompile should work normally");

        // Test that accelerated precompile works
        let accelerated = spec_precompiles.get(&new_address);
        assert!(accelerated.is_some(), "Accelerated precompile should be found");

        let result = accelerated
            .unwrap()
            .call(PrecompileInput {
                data: &test_input,
                gas: 1000,
                caller: Address::ZERO,
                value: U256::ZERO,
                internals: EvmInternals::new(&mut ctx.inner.journaled_state, &ctx.inner.block),
            })
            .unwrap();
        assert_eq!(result.bytes, Bytes::from_static(b"accelerated: test"));
        assert_eq!(result.gas_used, 25);
    }
}
