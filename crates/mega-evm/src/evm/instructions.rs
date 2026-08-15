use core::cmp::min;

use crate::{
    constants::{self},
    ExternalEnvTypes, HostExt, JournalInspectTr, MegaContext, MegaSpecId,
};
use alloy_evm::Database;
use alloy_primitives::{keccak256, Bytes, U256};
use revm::{
    bytecode::opcode::{
        self, CALL as OP_CALL, CALLCODE as OP_CALLCODE, DELEGATECALL as OP_DELEGATECALL,
        STATICCALL as OP_STATICCALL,
    },
    context::ContextTr,
    handler::instructions::{EthInstructions, InstructionProvider},
    interpreter::{
        as_usize_or_fail, gas,
        instructions::{self, control, gas_table_spec, utility::IntoAddress},
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, LoopControl, MemoryTr, RuntimeFlag},
        CallScheme, FrameInput, GasTable, Instruction, InstructionContext, InstructionExecResult,
        InstructionResult, InstructionTable, InterpreterAction, InterpreterTypes, SStoreResult,
        Stack,
    },
    primitives::KECCAK_EMPTY,
};

/// `MegaInstructions` is the instruction table for `MegaETH`.
///
/// This instruction table implements a multi-dimensional gas model and customizes certain opcodes
/// for `MegaETH` specifications:
///
/// # Multi-Dimensional Gas Model
///
/// All instructions track gas usage across multiple dimensions:
/// - **Compute Gas**: Standard EVM operation costs (arithmetic, control flow, memory, etc.)
/// - **Storage Gas**: Dynamic costs for persistent storage operations (SSTORE, CREATE, CALL with
///   transfer)
/// - **Log Storage Gas**: Additional costs for persisting event logs (10x standard costs)
///
/// This separation allows for independent pricing and limiting of different resource types.
///
/// # Customized Opcodes
///
/// ## LOG Opcodes (LOG0-LOG4)
/// - Compute gas: Standard EVM costs (375 + 375×topics + `8×data_bytes`)
/// - Storage gas: 10x multiplier (3,750×topics + `80×data_bytes`)
/// - Data limit enforcement: Halts when total transaction data exceeds 3.125 MB
///
/// ## SELFDESTRUCT Opcode
/// - Disabled in Mini-Rex, Rex, and Rex1 specs
/// - Re-enabled in Rex2 with EIP-6780 semantics
/// - When disabled, halts with `InvalidFEOpcode` to prevent contract destruction
///
/// ## SSTORE Opcode
/// - Compute gas: Standard EIP-2200/EIP-2929 costs
/// - Storage gas: Dynamic bucket-based costs only when setting zero → non-zero
/// - Data/KV limit enforcement: Tracks 40 bytes + 1 KV update per storage slot modification
///
/// ## CREATE/CREATE2 Opcodes
/// - Compute gas: Standard costs (32,000 for CREATE, 6 gas/word for CREATE2 hashing)
/// - Storage gas: Dynamic bucket-based costs for new account creation
/// - Gas forwarding: 98/100 rule (2% withheld vs. standard 1.5%)
/// - Data/KV tracking: 40 bytes + 1 KV update per account creation
///
/// ## CALL-like Opcode
/// - Compute gas: Standard call costs
/// - Storage gas: Dynamic bucket-based costs for new account creation (when transferring to empty
///   account)
/// - REX4+: Value-transferring `CALL`/`CALLCODE` receives additional `STORAGE_CALL_STIPEND` for
///   storage-gas-heavy operations such as `LOG`
/// - REX4+: Compute gas remains capped at the original `forwarded_gas + CALL_STIPEND`, so the extra
///   stipend cannot be used for pure computation
/// - Gas forwarding: 98/100 rule (2% withheld vs. standard 1.5%)
/// - Oracle detection: Handled at frame level (in `frame_init`), applies gas detention for both
///   direct transaction calls and internal CALL operations
/// - Data/KV tracking: 40 bytes + 2 KV updates when transferring to empty account
///
/// ## Volatile Data Access Opcodes
/// Block environment opcodes (TIMESTAMP, NUMBER, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE,
/// BLOCKHASH, BLOBBASEFEE, BLOBHASH) and beneficiary-accessing opcodes (BALANCE, EXTCODESIZE,
/// EXTCODECOPY, EXTCODEHASH) implement immediate gas detention to prevent `DoS` attacks.
///
/// # Gas Detention Mechanism
///
/// When volatile data (block environment, beneficiary, or oracle) is accessed, the system
/// implements a gas detention mechanism:
/// 1. The compute gas limit is lowered based on the type of volatile data:
///    - Block environment or beneficiary: `BLOCK_ENV_ACCESS_COMPUTE_GAS` (20M gas)
///    - Oracle contract: `ORACLE_ACCESS_COMPUTE_GAS` (1M gas pre-Rex3, 20M gas Rex3+)
///
///    In pre-REX4, this is an **absolute** cap on total compute gas.
///    In REX4+, this is a **relative** cap: `usage_at_access + cap`.
/// 2. Most restrictive limit wins: If multiple volatile data types are accessed, the minimum (most
///    restrictive) effective limit applies, regardless of access order
/// 3. Detained gas is tracked and refunded at transaction end
/// 4. Users only pay for actual work performed, not for enforcement gas
/// 5. This prevents `DoS` attacks while maintaining fair gas accounting
///
/// # Instruction Layering Architecture
///
/// ## Extension Modules (Inner → Outer)
///
/// Each opcode handler is composed of one or more extension module wrappers, applied from
/// innermost (closest to revm) to outermost:
///
/// 1. **`compute_gas_ext`** — Tracks compute gas usage for every opcode. Wraps the raw revm
///    instruction and records how much gas was consumed.
/// 2. **`storage_gas_ext`** — Adds dynamic storage gas costs (SSTORE, CALL with value transfer,
///    CREATE, LOG). Wraps `compute_gas_ext` handlers.
/// 3. **`additional_limit_ext`** — Enforces multidimensional resource limits (data size, KV
///    updates). Wraps `storage_gas_ext` handlers.
/// 4. **`forward_gas_ext`** — Enforces the 98/100 gas forwarding rule for CALL-like and CREATE
///    opcodes. Wraps `storage_gas_ext` handlers.
/// 5. **`volatile_data_ext`** — Applies gas detention on volatile data access (block env,
///    beneficiary, oracle) and pre-execution disable checks (Rex4+). Wraps `compute_gas_ext` or
///    `forward_gas_ext` handlers depending on the opcode. Its opcodes are also the ones excluded
///    from the interpreter's static-gas pre-charge (see [`gas_table_for_spec`]), so that a frame
///    holding less gas than the pre-charge still reaches the disable check; that gas is then
///    charged from inside the layering, at the point revm's own opcode body charges it (see
///    [`charge_static_gas`]).
///
/// ## Spec Progression and Opcode Overrides
///
/// Each spec builds on the previous one. Only the opcodes that change are listed:
///
/// - **EQUIVALENCE**: Standard revm mainnet instruction table (no custom wrappers).
/// - **`MINI_REX`** (base custom table): All 256 opcodes initialized from scratch.
///   - Most opcodes: `compute_gas_ext::*`
///   - Block env opcodes (TIMESTAMP, NUMBER, etc.): `volatile_data_ext::*`
///   - BALANCE, EXTCODESIZE, EXTCODECOPY, EXTCODEHASH: `volatile_data_ext::*`
///   - SLOAD: `compute_gas_ext::sload`
///   - SSTORE: `additional_limit_ext` → `storage_gas_ext`
///   - LOG0–LOG4: `additional_limit_ext` → `storage_gas_ext`
///   - CALL: `forward_gas_ext` → `storage_gas_ext`
///   - CREATE, CREATE2: `forward_gas_ext` → `storage_gas_ext`
///   - CALLCODE, DELEGATECALL, STATICCALL: `compute_gas_ext::*` (bug: missing `forward_gas_ext`)
///   - SELFDESTRUCT: disabled (`control::invalid`)
/// - **REX / REX1** (extends `MINI_REX)`:
///   - CALLCODE: `forward_gas_ext` → `storage_gas_ext` (bugfix)
///   - DELEGATECALL: `forward_gas_ext` → `storage_gas_ext` (bugfix)
///   - STATICCALL: `forward_gas_ext` → `storage_gas_ext` (bugfix)
/// - **REX2** (extends REX):
///   - SELFDESTRUCT: `compute_gas_ext::selfdestruct` (re-enabled with EIP-6780)
/// - **REX3** (extends REX2):
///   - SLOAD: `volatile_data_ext::sload` → `compute_gas_ext::sload` (oracle gas detention)
/// - **REX4** (extends REX3):
///   - CALL: `volatile_data_ext` → `forward_gas_ext` → `storage_gas_ext`
///   - STATICCALL: `volatile_data_ext` → `forward_gas_ext` → `storage_gas_ext`
///   - DELEGATECALL: `volatile_data_ext` → `forward_gas_ext` → `storage_gas_ext`
///   - CALLCODE: `volatile_data_ext` → `forward_gas_ext` → `storage_gas_ext`
/// - **REX5** (extends REX4):
///   - SELFDESTRUCT: `volatile_data_ext::selfdestruct_with_beneficiary_guard` →
///     `storage_gas_ext::selfdestruct` (new-account storage gas, beneficiary-volatile guard
///     outermost)
/// - **REX6** (extends REX5): unifies the per-opcode gas-metering order. The table wiring is
///   unchanged. Storage-affecting handlers (SSTORE, LOG, CALL-family, CREATE/CREATE2) all follow a
///   canonical order: charge storage gas → run the raw opcode body → record compute gas exactly
///   once via `record_storage_compute_gas!` after the body completes, excluding the storage gas.
///   For SSTORE / LOG / CALL-family this is byte-equivalent to the pre-REX6 layering (nothing
///   between `gas_before` and the storage charge debits EVM gas), so `storage_gas_ext::*` records
///   compute inline on every spec — no `if REX6` branch is needed. SELFDESTRUCT keeps its
///   delegation to `compute_gas_ext::selfdestruct`, whose trailing `record_compute_gas_all_dims`
///   check records the same single compute window while latching the pre-recorded data/KV/state
///   usage. CREATE2 is the one real behavior change: REX6+ short-circuits to `create_rex6`, which
///   folds the memory-expansion gas into the single post-body recording instead of recording it as
///   a separate eager entry as REX5 did.
/// - **REX7** (extends REX6): switches to **checkpoint compute-gas settlement**. The plain opcodes
///   are revm's own instructions with no recording wrapper at all; compute gas settles as an
///   interpreter-gas delta at each checkpoint — the storage-gas opcodes, the CALL / CREATE family,
///   the volatile opcodes, and frame entry / resume / exit. Per-transaction totals are unchanged
///   for a transaction that stays inside every limit and never halts exceptionally; a frame that
///   does halt exceptionally additionally reports the budget it destroyed, which is enforced
///   against nothing. Enforcement inside a plain segment is the gas clamp, which stops the crossing
///   opcode before it executes; an exceed detected by a settlement instead surfaces at the
///   checkpoint that settled it rather than at the opcode that crossed the limit.
///   - Volatile opcodes: `volatile_data_ext::*_checkpoint` (raw instruction + segment settlement +
///     detention cap) in place of the `compute_gas_ext` delegation
///   - Storage-gas, CALL-family, CREATE and SELFDESTRUCT: the REX6 handler chains, settling from
///     the checkpoint baseline internally
///   - Every other opcode: revm's raw instruction
///
/// Note: chains terminating at `storage_gas_ext` (rather than `compute_gas_ext`) reflect the
/// canonical metering order above — `storage_gas_ext::*` records compute gas internally via
/// `record_storage_compute_gas!`, so there is no separate `compute_gas_ext` layer below it.
///
/// # Assumptions
///
/// This instruction table is only used when the `MINI_REX` spec (or later) is enabled, so we can
/// safely assume that all features before and including Mini-Rex are enabled.
#[derive(Clone)]
pub struct MegaInstructions<DB: Database, ExtEnvs: ExternalEnvTypes> {
    spec: MegaSpecId,
    inner: EthInstructions<EthInterpreter, MegaContext<DB, ExtEnvs>>,
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> core::fmt::Debug for MegaInstructions<DB, ExtEnvs> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethInstructions").field("spec", &self.spec).finish_non_exhaustive()
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaInstructions<DB, ExtEnvs> {
    /// Create a new `MegaethInstructions` with the given spec id.
    pub fn new(spec: MegaSpecId) -> Self {
        // MegaETH only ever runs its opcodes under the Ethereum spec its `MegaSpecId` maps to, so
        // the static gas table is always built from that spec rather than `SpecId::default()`.
        let eth_spec = spec.into_eth_spec();
        let gas_table = gas_table_for_spec(spec);
        let instruction_table = match spec {
            MegaSpecId::EQUIVALENCE => EthInstructions::new_mainnet_with_spec(eth_spec),
            MegaSpecId::MINI_REX => EthInstructions::new(
                mini_rex::instruction_table::<EthInterpreter, MegaContext<DB, ExtEnvs>>(),
                gas_table,
                eth_spec,
            ),
            MegaSpecId::REX | MegaSpecId::REX1 => EthInstructions::new(
                rex::instruction_table::<EthInterpreter, MegaContext<DB, ExtEnvs>>(),
                gas_table,
                eth_spec,
            ),
            MegaSpecId::REX2 => EthInstructions::new(
                rex2::instruction_table::<EthInterpreter, MegaContext<DB, ExtEnvs>>(),
                gas_table,
                eth_spec,
            ),
            MegaSpecId::REX3 => EthInstructions::new(
                rex3::instruction_table::<EthInterpreter, MegaContext<DB, ExtEnvs>>(),
                gas_table,
                eth_spec,
            ),
            MegaSpecId::REX4 => EthInstructions::new(
                rex4::instruction_table::<EthInterpreter, MegaContext<DB, ExtEnvs>>(),
                gas_table,
                eth_spec,
            ),
            MegaSpecId::REX5 => EthInstructions::new(
                rex5::instruction_table::<EthInterpreter, MegaContext<DB, ExtEnvs>>(),
                gas_table,
                eth_spec,
            ),
            MegaSpecId::REX6 => EthInstructions::new(
                rex6::instruction_table::<EthInterpreter, MegaContext<DB, ExtEnvs>>(),
                gas_table,
                eth_spec,
            ),
            MegaSpecId::REX7 => EthInstructions::new(
                rex7::instruction_table::<EthInterpreter, MegaContext<DB, ExtEnvs>>(),
                gas_table,
                eth_spec,
            ),
        };
        Self { spec, inner: instruction_table }
    }
}

/// Returns the static gas table the interpreter pre-charges from for `spec`.
///
/// This is revm's table for the spec's Ethereum spec, with the entry of every opcode that `spec`'s
/// instruction table wraps in a `volatile_data_ext` handler zeroed out — those chains charge their
/// own static gas so that their `disableVolatileDataAccess` guard is reached even by a frame that
/// could not afford the pre-charge (see [`charge_static_gas`]). The per-spec zeroing mirrors the
/// instruction-table layering one module at a time, so a table entry that gains or loses a volatile
/// wrapper has its gas entry adjusted right beside it.
fn gas_table_for_spec(spec: MegaSpecId) -> GasTable {
    let base = gas_table_spec(spec.into_eth_spec());
    match spec {
        // Vanilla mainnet table and handlers — no volatile wrappers, nothing to zero.
        MegaSpecId::EQUIVALENCE => base,
        MegaSpecId::MINI_REX => mini_rex::gas_table(base),
        MegaSpecId::REX | MegaSpecId::REX1 => rex::gas_table(base),
        MegaSpecId::REX2 => rex2::gas_table(base),
        MegaSpecId::REX3 => rex3::gas_table(base),
        MegaSpecId::REX4 => rex4::gas_table(base),
        MegaSpecId::REX5 => rex5::gas_table(base),
        MegaSpecId::REX6 => rex6::gas_table(base),
        MegaSpecId::REX7 => rex7::gas_table(base),
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> InstructionProvider
    for MegaInstructions<DB, ExtEnvs>
{
    type Context = MegaContext<DB, ExtEnvs>;
    type InterpreterTypes = EthInterpreter;

    fn instruction_table(&self) -> &InstructionTable<Self::InterpreterTypes, Self::Context> {
        self.inner.instruction_table()
    }

    fn gas_table(&self) -> &GasTable {
        self.inner.gas_table()
    }
}

mod rex {
    use super::*;

    /// Returns the instruction table for the `REX` and `REX1` specs.
    ///
    /// Changes from Mini-Rex (bugfix — adds missing `forward_gas_ext` wrapping):
    /// - CALLCODE: `forward_gas_ext` → `storage_gas_ext`
    /// - DELEGATECALL: `forward_gas_ext` → `storage_gas_ext`
    /// - STATICCALL: `forward_gas_ext` → `storage_gas_ext`
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >() -> [Instruction<WIRE, H>; 256]
    where
        WIRE::Stack: StackInspectTr,
    {
        use revm::bytecode::opcode::*;
        let mut table = mini_rex::instruction_table::<WIRE, H>();

        // Mini-Rex mistakenly not modifying these three call-like opcodes. They are fixed in Rex
        table[CALLCODE as usize] = Instruction::new(forward_gas_ext::call_code);
        table[DELEGATECALL as usize] = Instruction::new(forward_gas_ext::delegate_call);
        table[STATICCALL as usize] = Instruction::new(forward_gas_ext::static_call);

        table
    }

    /// Returns the static gas table for the `REX` and `REX1` specs.
    ///
    /// The three call-like opcodes rewired above gain a `forward_gas_ext` layer, not a volatile
    /// guard, so the zeroed set is unchanged from Mini-Rex.
    pub(super) const fn gas_table(table: GasTable) -> GasTable {
        mini_rex::gas_table(table)
    }
}

mod rex2 {
    use super::*;

    /// Returns the instruction table for the `REX2` spec.
    ///
    /// Changes from Rex:
    /// - SELFDESTRUCT: `compute_gas_ext::selfdestruct` (re-enabled with EIP-6780 semantics)
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >() -> [Instruction<WIRE, H>; 256]
    where
        WIRE::Stack: StackInspectTr,
    {
        use revm::bytecode::opcode::*;
        let mut table = rex::instruction_table::<WIRE, H>();

        table[SELFDESTRUCT as usize] = Instruction::new(compute_gas_ext::selfdestruct);

        table
    }

    /// Returns the static gas table for the `REX2` spec.
    ///
    /// `SELFDESTRUCT` is re-enabled above without a volatile guard (Rex4 adds one), so the zeroed
    /// set is unchanged from Rex.
    pub(super) const fn gas_table(table: GasTable) -> GasTable {
        rex::gas_table(table)
    }
}

mod rex3 {
    use super::*;

    /// Returns the instruction table for the `REX3` spec.
    ///
    /// Changes from Rex2:
    /// - SLOAD: `volatile_data_ext::sload` → `compute_gas_ext::sload` (oracle gas detention). This
    ///   replaces the CALL-based oracle access detection used in earlier specs.
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >() -> [Instruction<WIRE, H>; 256]
    where
        WIRE::Stack: StackInspectTr,
    {
        use revm::bytecode::opcode::*;
        let mut table = rex2::instruction_table::<WIRE, H>();

        // Rex3: SLOAD triggers gas detention for oracle contract access.
        // The host's sload() method marks oracle access in the volatile data tracker,
        // then the detain_gas_ext wrapper applies the compute gas limit.
        table[SLOAD as usize] = Instruction::new(volatile_data_ext::sload);

        table
    }

    /// Returns the static gas table for the `REX3` spec.
    ///
    /// Adds `SLOAD` to the zeroed set: it gains the oracle volatile guard above.
    pub(super) const fn gas_table(mut table: GasTable) -> GasTable {
        use revm::bytecode::opcode::*;
        table = rex2::gas_table(table);
        table[SLOAD as usize] = 0;
        table
    }
}

mod rex4 {
    use super::*;

    /// Returns the instruction table for the `REX4` spec.
    ///
    /// Changes from Rex3:
    /// - CALL: `volatile_data_ext::call` → `forward_gas_ext` → `storage_gas_ext`
    /// - STATICCALL: `volatile_data_ext::static_call` → `forward_gas_ext` → `storage_gas_ext`
    /// - DELEGATECALL: `volatile_data_ext::delegate_call` → `forward_gas_ext` → `storage_gas_ext`
    /// - CALLCODE: `volatile_data_ext::call_code` → `forward_gas_ext` → `storage_gas_ext`
    /// - SELFDESTRUCT: `volatile_data_ext::selfdestruct` → `compute_gas_ext::selfdestruct`
    /// - SELFBALANCE: `volatile_data_ext::selfbalance` → `compute_gas_ext::selfbalance`
    ///
    /// The `volatile_data_ext` wrapper checks if the target address is the beneficiary and
    /// volatile data access is disabled — if so, reverts before executing.
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >() -> [Instruction<WIRE, H>; 256]
    where
        WIRE::Stack: StackInspectTr,
    {
        use revm::bytecode::opcode::*;
        let mut table = rex3::instruction_table::<WIRE, H>();

        // Rex4: CALL-like opcodes check for beneficiary volatile access disabled.
        table[CALL as usize] = Instruction::new(volatile_data_ext::call);
        table[STATICCALL as usize] = Instruction::new(volatile_data_ext::static_call);
        table[DELEGATECALL as usize] = Instruction::new(volatile_data_ext::delegate_call);
        table[CALLCODE as usize] = Instruction::new(volatile_data_ext::call_code);

        // Rex4: SELFDESTRUCT checks for beneficiary volatile access.
        table[SELFDESTRUCT as usize] = Instruction::new(volatile_data_ext::selfdestruct);

        // Rex4: SELFBALANCE checks for beneficiary volatile access (when the executing
        // contract is the beneficiary, SELFBALANCE triggers gas detention).
        table[SELFBALANCE as usize] = Instruction::new(volatile_data_ext::selfbalance);

        table
    }

    /// Returns the static gas table for the `REX4` spec.
    ///
    /// Adds the CALL family, `SELFDESTRUCT` and `SELFBALANCE` to the zeroed set: all three gain a
    /// beneficiary volatile guard above.
    pub(super) const fn gas_table(mut table: GasTable) -> GasTable {
        use revm::bytecode::opcode::*;
        table = rex3::gas_table(table);
        table[CALL as usize] = 0;
        table[STATICCALL as usize] = 0;
        table[DELEGATECALL as usize] = 0;
        table[CALLCODE as usize] = 0;
        table[SELFDESTRUCT as usize] = 0;
        table[SELFBALANCE as usize] = 0;
        table
    }
}

mod rex5 {
    use super::*;

    /// Returns the instruction table for the `REX5` spec.
    ///
    /// Changes from Rex4:
    /// - SELFDESTRUCT: `volatile_data_ext::selfdestruct_with_beneficiary_guard` →
    ///   `storage_gas_ext::selfdestruct`. The outer wrapper keeps the beneficiary-volatile guard
    ///   outermost (matching the SSTORE / LOG layering) and slots the new-account storage-gas
    ///   charge between the guard and the inner opcode, so disabled-volatile frames short-circuit
    ///   ahead of any storage-layer side effects (account inspection, dynamic gas charge,
    ///   `on_selfdestruct_new_account` record).
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >() -> [Instruction<WIRE, H>; 256]
    where
        WIRE::Stack: StackInspectTr,
    {
        use revm::bytecode::opcode::*;
        let mut table = rex4::instruction_table::<WIRE, H>();

        // REX5: SELFDESTRUCT charges storage gas for new beneficiary accounts,
        // gated behind the beneficiary-volatile guard.
        table[SELFDESTRUCT as usize] =
            Instruction::new(volatile_data_ext::selfdestruct_with_beneficiary_guard);

        table
    }

    /// Returns the static gas table for the `REX5` spec.
    ///
    /// `SELFDESTRUCT` swaps one volatile-guarded handler for another, so the zeroed set is
    /// unchanged from Rex4.
    pub(super) const fn gas_table(table: GasTable) -> GasTable {
        rex4::gas_table(table)
    }
}

mod rex6 {
    use super::*;

    /// Returns the instruction table for the `REX6` spec.
    ///
    /// Changes from Rex5: the instruction *table* is unchanged (same handler functions as Rex5).
    /// Every Rex6 behavior difference is expressed as internal `spec.is_enabled(MegaSpecId::REX6)`
    /// dispatch inside the shared handlers, never as a swapped table entry:
    /// - the storage-affecting handlers (SSTORE, LOG, CALL-family, CREATE/CREATE2) charge storage
    ///   gas, run their body, then record compute gas exactly once (via
    ///   [`record_storage_compute_gas!`]) with the storage gas excluded; SELFDESTRUCT keeps its
    ///   delegation to `compute_gas_ext::selfdestruct`, whose trailing all-dimension check records
    ///   the same single compute window while latching the pre-recorded data/KV/state usage;
    /// - `storage_gas_ext::selfdestruct` additionally records existing-target balance-update
    ///   accounting, and its outer volatile wrapper
    ///   (`volatile_data_ext::selfdestruct_with_beneficiary_guard`) additionally guards the
    ///   executing contract (source) against the beneficiary;
    /// - the CALL-family volatile wrappers, on the `disableVolatileDataAccess` path, resolve the
    ///   stack target's one-hop EIP-7702 delegate before the beneficiary comparison.
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >() -> [Instruction<WIRE, H>; 256]
    where
        WIRE::Stack: StackInspectTr,
    {
        rex5::instruction_table::<WIRE, H>()
    }

    /// Returns the static gas table for the `REX6` spec.
    ///
    /// The instruction table is unchanged from Rex5, so the zeroed set is too.
    pub(super) const fn gas_table(table: GasTable) -> GasTable {
        rex5::gas_table(table)
    }
}

/// The static per-opcode gas revm's schedule prices separately from an opcode's dynamic cost.
///
/// Every handler below measures its opcode's compute gas as an `interpreter.gas.remaining()` delta
/// taken inside the handler, and the recorded compute gas must come out as the opcode's full EVM
/// cost — the quantity `MegaETH`'s compute-gas limit has always been defined over. Which site
/// debits this table's entry decides how the handler gets there:
///
/// - **The interpreter**, from its [`GasTable`] before dispatch, for every opcode whose entry the
///   spec leaves in place. The debit falls outside the measurement window, so the recording site
///   adds the entry back into the measured delta.
/// - **A [`charge_static_gas`] call site**, for the volatile-guarded opcodes whose entry the spec
///   zeroes. Whether that lands inside or outside the window depends on the position revm's body
///   charges from, so those sites add the entry back only when they charge it ahead of the window.
///
/// This is deliberately revm's unmodified table, not the per-spec one from [`gas_table_for_spec`]:
/// it is the *schedule*, not the pre-charge configuration, and the guarded opcodes' entries are
/// exactly what their handlers have to charge.
///
/// Every [`MegaSpecId`] maps to the same Ethereum spec, so a single table covers all of them; the
/// [`tests::test_static_gas_table_is_spec_invariant`] test fails if that stops holding.
const STATIC_GAS_TABLE: GasTable = gas_table_spec(MegaSpecId::MINI_REX.into_eth_spec());

/// Returns the static gas charged for `opcode` separately from its dynamic cost.
///
/// Call sites fold the lookup into a `const` at compile time, so the per-opcode hot path only gains
/// a constant addition (or, at a [`charge_static_gas`] site, subtraction).
const fn static_gas(opcode: u8) -> u64 {
    STATIC_GAS_TABLE[opcode as usize] as u64
}

/// Halts the interpreter with `$result`, replacing any pending action and leaving the frame's
/// remaining gas untouched.
///
/// This is the abort primitive for `MegaETH`'s resource-limit halts. It deliberately does not use
/// [`revm::interpreter::Interpreter::halt`], which differs on both counts:
///
/// - `halt` spends all remaining gas when `$result` is `OutOfGas`. A TX-level limit exceed halts
///   with exactly that result and `MegaETH` refunds the frame's remaining gas to the sender
///   (`AdditionalLimit::rescue_gas`, and the detained-gas refund) from the gas recorded in the halt
///   action — spending it would burn gas the user never got to use.
/// - `halt` rejects setting an action while one is already pending. A limit abort taken right after
///   a CALL/CREATE body published its child `NewFrame` must *replace* that action: the halt wins
///   and the child never runs. The pending action is therefore taken and dropped first.
///
/// The enclosing handler must still return `Err($result)` afterwards: the interpreter loop only
/// stops when a step reports an error, and it does not consult the action.
macro_rules! set_halt_action {
    ($interpreter:expr, $result:expr) => {{
        if $interpreter.bytecode.action().is_some() {
            // Drop the pending child `NewFrame` (or return) action — the halt supersedes it.
            let _ = $interpreter.take_next_action();
        }
        let gas = $interpreter.gas;
        $interpreter.bytecode.set_action(InterpreterAction::new_halt($result, gas));
    }};
}

mod rex7 {
    use super::*;

    /// Returns the instruction table for the `REX7` spec — **checkpoint compute-gas accounting**.
    ///
    /// Unlike every earlier custom table, the plain opcodes are revm's own instructions with no
    /// per-opcode gas recording at all: the interpreter's gas counter is the accounting source,
    /// and compute gas settles as a segment delta at each checkpoint. The checkpoints are exactly
    /// the positions that have to stay wrapped anyway:
    ///
    /// - the storage-gas opcodes (SSTORE, LOG0–LOG4, SELFDESTRUCT) and the CALL / CREATE family —
    ///   the same handler chains as Rex6, whose [`record_storage_compute_gas!`] settles from the
    ///   checkpoint baseline instead of a per-opcode capture;
    /// - the volatile / detention opcodes — `*_checkpoint` variants that run the raw instruction,
    ///   settle the segment, then apply the detention cap;
    /// - frame entry / resume and frame exit — `AdditionalLimit::before_frame_run` opens the window
    ///   and `after_frame_run_instructions` settles the tail segment.
    ///
    /// Per-transaction totals telescope to the same sums as per-opcode recording. What differs is
    /// where a limit-exceeding transaction halts: the exceed surfaces at the next checkpoint
    /// rather than at the opcode that crossed the limit.
    ///
    /// Of those, only the volatile / detention handlers (plus `GAS`, a checkpoint so that the
    /// clamp is restored before the counter is observed) are declared here. The storage-gas and
    /// frame-spawning slots are copied out of the Rex6 table one opcode at a time, which is what
    /// makes "the same handler chains as Rex6" a property of the construction rather than of two
    /// declarations kept in sync. The same copy covers the four opcodes revm wires ahead of the
    /// fork that activates them (`DUPN`, `SWAPN`, `EXCHANGE`, `SLOTNUM`): Rex6 leaves
    /// `control::unknown` in those slots, so inheriting them keeps the two opcode sets identical
    /// instead of letting revm's base table decide what Rex7 exposes.
    ///
    /// The Rex6 behavior differences (canonical metering order, `create_rex6` dispatch,
    /// SELFDESTRUCT existing-target accounting, CALL-family EIP-7702 delegate resolution on the
    /// disabled path) live as internal `spec.is_enabled(MegaSpecId::REX6)` dispatch inside the
    /// shared handlers reused here, so they carry over unchanged.
    ///
    /// `H` is `Sized` here — unlike the earlier tables — because the base table comes from revm's
    /// own [`instructions::instruction_table`], whose bound it is. The only caller instantiates it
    /// with [`MegaContext`], so nothing is lost.
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr,
    >() -> [Instruction<WIRE, H>; 256]
    where
        WIRE::Stack: StackInspectTr,
    {
        use revm::bytecode::opcode::*;
        let mut table = instructions::instruction_table::<WIRE, H>();
        let rex6 = rex6::instruction_table::<WIRE, H>();

        /// The opcodes Rex7 takes from the Rex6 table verbatim.
        const INHERITED_FROM_REX6: &[u8] = &[
            // Storage-gas and frame-spawning checkpoints: the Rex6 handler chains, which under
            // Rex7 open with a checkpoint prologue and close with an epilogue.
            SSTORE,
            LOG0,
            LOG1,
            LOG2,
            LOG3,
            LOG4,
            CREATE,
            CREATE2,
            CALL,
            CALLCODE,
            DELEGATECALL,
            STATICCALL,
            SELFDESTRUCT,
            // Opcodes revm's table wires ahead of the fork that activates them: every
            // `MegaSpecId` maps to a pre-activation Ethereum spec and no `MegaETH` table has ever
            // dispatched them, so Rex6 holds `control::unknown` here.
            DUPN,
            SWAPN,
            EXCHANGE,
            SLOTNUM,
        ];

        let mut i = 0;
        while i < INHERITED_FROM_REX6.len() {
            let opcode = INHERITED_FROM_REX6[i] as usize;
            table[opcode] = rex6[opcode];
            i += 1;
        }

        // Volatile / detention checkpoints: raw instruction, segment settlement, detention cap.
        table[BALANCE as usize] = Instruction::new(volatile_data_ext::balance_checkpoint);
        table[EXTCODESIZE as usize] = Instruction::new(volatile_data_ext::extcodesize_checkpoint);
        table[EXTCODECOPY as usize] = Instruction::new(volatile_data_ext::extcodecopy_checkpoint);
        table[EXTCODEHASH as usize] = Instruction::new(volatile_data_ext::extcodehash_checkpoint);
        table[BLOCKHASH as usize] = Instruction::new(volatile_data_ext::blockhash_checkpoint);
        table[COINBASE as usize] = Instruction::new(volatile_data_ext::coinbase_checkpoint);
        table[TIMESTAMP as usize] = Instruction::new(volatile_data_ext::timestamp_checkpoint);
        table[NUMBER as usize] = Instruction::new(volatile_data_ext::block_number_checkpoint);
        table[DIFFICULTY as usize] = Instruction::new(volatile_data_ext::difficulty_checkpoint);
        table[GASLIMIT as usize] = Instruction::new(volatile_data_ext::gas_limit_opcode_checkpoint);
        table[BASEFEE as usize] = Instruction::new(volatile_data_ext::basefee_checkpoint);
        table[BLOBBASEFEE as usize] = Instruction::new(volatile_data_ext::blobbasefee_checkpoint);
        table[BLOBHASH as usize] = Instruction::new(volatile_data_ext::blobhash_checkpoint);
        table[SELFBALANCE as usize] = Instruction::new(volatile_data_ext::selfbalance_checkpoint);
        table[SLOAD as usize] = Instruction::new(volatile_data_ext::sload_checkpoint);

        // Gas-clamp enforcement: `GAS` has to be a checkpoint so the clamp is restored before
        // the counter is observed.
        table[GAS as usize] = Instruction::new(compute_gas_ext::gas_checkpoint);

        table
    }

    /// Returns the static gas table for the `REX7` spec.
    ///
    /// The volatile-guarded set is unchanged from Rex6 — the checkpoint handlers guard and charge
    /// exactly the opcodes their per-opcode counterparts did — so the zeroed set is too. The plain
    /// opcodes keep revm's entries: their pre-charge is what the segment delta measures.
    pub(super) const fn gas_table(table: GasTable) -> GasTable {
        rex6::gas_table(table)
    }
}

/// Macro to record compute gas and check if the limit has been exceeded. If the limit is exceeded,
/// the interpreter halts and returns.
macro_rules! compute_gas {
    // Variant for helpers that report failure as `None` rather than `Err`.
    ($interpreter:expr, $additional_limit:expr, $gas_used:expr, $ret:expr) => {
        if !$additional_limit.record_compute_gas($gas_used) {
            set_halt_action!($interpreter, $additional_limit.exceeding_instruction_result());
            return $ret;
        }
    };
    // Variant for instruction handlers returning `InstructionExecResult`.
    ($interpreter:expr, $additional_limit:expr, $gas_used:expr) => {
        if !$additional_limit.record_compute_gas($gas_used) {
            let result = $additional_limit.exceeding_instruction_result();
            set_halt_action!($interpreter, result);
            return Err(result);
        }
    };
}

/// Converts a `U256` operand to `usize`, halting with `InvalidOperandOOG` and returning `$ret` from
/// the enclosing function when it does not fit.
///
/// Mirrors the limb check of `revm::interpreter::as_usize_or_fail!`, which cannot be used here
/// because it returns `Err(..)` and these helpers return an `Option`.
macro_rules! as_usize_or_fail_ret {
    ($interpreter:expr, $v:expr, $ret:expr) => {
        match $v.as_limbs() {
            x => {
                if (x[0] > usize::MAX as u64) | (x[1] != 0) | (x[2] != 0) | (x[3] != 0) {
                    $interpreter.halt(InstructionResult::InvalidOperandOOG);
                    return $ret;
                }
                x[0] as usize
            }
        }
    };
}

/// Expands memory and charges the expansion gas, halting and returning `$ret` from the enclosing
/// function on failure.
///
/// Wraps `Interpreter::resize_memory`, which reports failure as `Err(InstructionResult)` rather
/// than halting the interpreter itself.
macro_rules! resize_memory_ret {
    ($context:expr, $offset:expr, $len:expr, $ret:expr) => {
        if let Err(result) =
            $context.interpreter.resize_memory($context.host.gas_params(), $offset, $len)
        {
            $context.interpreter.halt(result);
            return $ret;
        }
    };
}

/// Charges a gas cost that may have overflowed, halting with `OutOfGas` on the `None` arm.
///
/// Replaces `revm::interpreter::gas_or_fail!`, which was removed when instructions started
/// returning `InstructionExecResult`.
macro_rules! gas_or_fail {
    ($interpreter:expr, $gas:expr) => {
        match $gas {
            Some(gas_used) => gas!($interpreter, gas_used),
            None => return Err(InstructionResult::OutOfGas),
        }
    };
}

/// Charges `$opcode`'s [`STATIC_GAS_TABLE`] entry, returning `Err(OutOfGas)` from the enclosing
/// handler when the frame cannot afford it.
///
/// `Interpreter::step` normally debits an opcode's static gas from its [`GasTable`] *before*
/// dispatching to the handler, and reports out-of-gas without ever entering it. That pre-check is
/// incompatible with a guard that must reject its opcode with a revert and full gas: a frame
/// holding less than the static gas would be halted out of gas before the guard could run. Every
/// spec's gas table therefore zeroes the entries of its volatile-guarded opcodes (see
/// [`gas_table_for_spec`]), which makes the dispatch unconditional, and the handler charges the
/// entry itself.
///
/// Where it charges it is not free choice. `MegaETH`'s consensus schedule is the one revm charged
/// before the static gas table existed, which is to say from *inside* each opcode's body. The two
/// call-site groups below reproduce that position per opcode; each states the body order it is
/// mirroring.
///
/// The `const` item folds the table lookup away at compile time, leaving a constant subtraction on
/// the hot path.
macro_rules! charge_static_gas {
    ($context:expr, $opcode:ident) => {{
        const STATIC_GAS: u64 = static_gas(opcode::$opcode);
        gas!($context.interpreter, STATIC_GAS);
    }};
}

/// Macro to run the inner instruction and abort if the instruction result is an error.
macro_rules! run_inner_instruction_or_abort {
    ($inner_fn:path, $context:expr) => {
        run_inner_instruction_or_abort!($inner_fn, $context, _unused_inner_outcome)
    };
    ($inner_fn:path, $context:expr, $out:ident) => {
        let ctx = InstructionContext::<'_, H, WIRE> {
            interpreter: &mut *$context.interpreter,
            host: &mut *$context.host,
        };
        // Halting results abort the wrapper immediately. Non-halting terminations (STOP, RETURN,
        // SELFDESTRUCT, SUSPEND) are carried in `inner_outcome` so the wrapper's post-instruction
        // accounting still runs, and must be returned as the wrapper's own result — returning
        // `Ok(())` after the inner instruction set an action would let the interpreter loop keep
        // stepping.
        #[allow(unused_variables)]
        let $out: InstructionExecResult = match $inner_fn(ctx) {
            Ok(()) => Ok(()),
            Err(result) if result.is_halt() => return Err(result),
            Err(result) => Err(result),
        };
    };
    // Same as above, but a plain out-of-gas halt runs `$tripwire` before the early return skips
    // the wrapper's tail. Debug builds only; see
    // `volatile_data_ext::debug_check_frozen_detention_window` for what the tripwire watches.
    ($inner_fn:path, $context:expr, $out:ident, on_plain_oog: $tripwire:expr) => {
        let ctx = InstructionContext::<'_, H, WIRE> {
            interpreter: &mut *$context.interpreter,
            host: &mut *$context.host,
        };
        #[allow(unused_variables)]
        let $out: InstructionExecResult = match $inner_fn(ctx) {
            Ok(()) => Ok(()),
            Err(result) if result.is_halt() => {
                #[cfg(debug_assertions)]
                if matches!(result, InstructionResult::OutOfGas) {
                    $tripwire;
                }
                return Err(result);
            }
            Err(result) => Err(result),
        };
    };
}

/// REX7 checkpoint prologue. Runs at the top of every checkpoint handler, before any gas capture or
/// gas-consuming work:
///
/// 1. Settles the open plain-opcode segment — `baseline − remaining`, both readings on the clamped
///    counter, telescoping over exactly the unwrapped opcodes since the last checkpoint.
/// 2. Restores the clamp-hidden gas, so the checkpoint's body runs on the **true** counter: the
///    CALL-family forwarding math, the `GAS` opcode's pushed value and the storage-gas charges all
///    observe real gas, which is what keeps the clamp unobservable to a transaction that never
///    exceeds a limit.
/// 3. Re-opens the settlement window at the restored counter.
///
/// Halts — returning from the enclosing handler — when the settlement surfaces a limit exceed,
/// including one latched earlier by a non-compute mutation site. The restore has already happened
/// on that path, so the frame result carries true gas. No-op before REX7.
macro_rules! checkpoint_prologue {
    ($context:expr) => {
        if $context.host.spec_id().is_enabled(MegaSpecId::REX7) {
            let exceeding_result = {
                let mut additional_limit = $context.host.additional_limit().borrow_mut();
                let remaining = $context.interpreter.gas.remaining();
                let segment = additional_limit.checkpoint_baseline().saturating_sub(remaining);
                let hidden = additional_limit.checkpoint_restore_hidden();
                $context.interpreter.gas.erase_cost(hidden);
                additional_limit.sync_checkpoint_baseline($context.interpreter.gas.remaining());
                if additional_limit.record_compute_gas(segment) {
                    None
                } else {
                    Some(additional_limit.exceeding_instruction_result())
                }
            };
            if let Some(result) = exceeding_result {
                set_halt_action!($context.interpreter, result);
                return Err(result);
            }
        }
    };
}

/// REX7 checkpoint epilogue: re-applies the gas clamp from the freshly settled usage — including
/// any detention cap the checkpoint just installed — and re-opens the settlement window on the
/// clamped counter.
///
/// Only applies when the frame keeps executing. A checkpoint that published an action has either
/// suspended into a child frame (the resume clamps in `AdditionalLimit::before_frame_run`) or ended
/// the frame (the frame's final result restores instead), and clamping either would strand hidden
/// gas across the boundary. No-op before REX7.
macro_rules! checkpoint_epilogue {
    ($context:expr) => {
        if $context.host.spec_id().is_enabled(MegaSpecId::REX7) &&
            $context.interpreter.bytecode.action().is_none()
        {
            let mut additional_limit = $context.host.additional_limit().borrow_mut();
            let hide =
                additional_limit.checkpoint_clamp_amount($context.interpreter.gas.remaining());
            if hide > 0 {
                let clamped = $context.interpreter.gas.record_regular_cost(hide);
                debug_assert!(clamped, "clamp amount exceeds remaining gas");
            }
            additional_limit.sync_checkpoint_baseline($context.interpreter.gas.remaining());
        }
    };
}

/// Records a checkpoint opcode's own body gas (`$gas_before − remaining`) and re-opens the
/// settlement window, enforcing the compute-gas limit exactly as the per-opcode wrappers do.
///
/// Used by the REX7 checkpoint handlers whose bodies can never spawn a child frame (the volatile
/// opcodes, `SLOAD`, `SELFBALANCE`, `GAS`). The CALL / CREATE and storage-gas bodies use
/// [`record_storage_compute_gas!`] instead, which additionally excludes storage charges and
/// forwarded child gas.
macro_rules! record_checkpoint_body_compute_gas {
    ($context:expr, $gas_before:expr) => {
        let gas_after = $context.interpreter.gas.remaining();
        let gas_used = $gas_before.saturating_sub(gas_after);
        {
            let mut additional_limit = $context.host.additional_limit().borrow_mut();
            additional_limit.sync_checkpoint_baseline(gas_after);
            compute_gas!($context.interpreter, additional_limit, gas_used);
        }
    };
    // Variant for the volatile checkpoints, whose tail installs the detention cap. A frame-local
    // exceed reports as a revert, which the per-opcode layering carries past the cap application
    // rather than returning on, so the cap is applied here before returning. A TX-level exceed
    // reports as an out-of-gas halt, which that layering does short-circuit — no cap on that path.
    ($context:expr, $gas_before:expr, detention_tail) => {
        let gas_after = $context.interpreter.gas.remaining();
        let gas_used = $gas_before.saturating_sub(gas_after);
        let exceeding_result = {
            let mut additional_limit = $context.host.additional_limit().borrow_mut();
            additional_limit.sync_checkpoint_baseline(gas_after);
            if additional_limit.record_compute_gas(gas_used) {
                None
            } else {
                Some(additional_limit.exceeding_instruction_result())
            }
        };
        if let Some(result) = exceeding_result {
            set_halt_action!($context.interpreter, result);
            if !result.is_halt() {
                apply_compute_gas_limit!($context);
            }
            return Err(result);
        }
    };
}

/// Charges `$amount` of `MegaETH` storage gas to the interpreter's counter and keeps it out of the
/// REX7 settlement segment that is currently open, returning the amount charged.
///
/// Storage gas is never compute gas. A checkpoint body normally subtracts its own charge when
/// [`record_storage_compute_gas!`] closes the body's measurement window — but a body that halts
/// before reaching that macro (a static-context `LOG`, an inner instruction that runs out of gas)
/// leaves the frame-exit settlement measuring a segment the charge is still inside, which would
/// report storage gas as compute gas. Excluding it from the segment as it is charged makes the
/// exclusion hold on both paths; on the normal path the body's own window re-syncs the segment
/// afterwards, so this is invisible there.
///
/// Returns `Err(OutOfGas)` from the enclosing handler when the frame cannot afford the charge,
/// exactly as a bare `gas!` would — with nothing debited and so nothing to exclude. No-op before
/// REX7, where nothing measures against a segment.
macro_rules! charge_storage_gas {
    ($context:expr, $amount:expr) => {{
        let amount: u64 = $amount;
        gas!($context.interpreter, amount);
        $context.host.additional_limit().borrow_mut().exclude_storage_gas_from_segment(amount);
        amount
    }};
}

/// Records an opcode's compute gas in a single measurement window and enforces the compute-gas
/// limit. The REX6 storage-affecting handlers invoke it directly with the storage gas they
/// charged; plain opcodes use the leaner inline recording in
/// `compute_gas_ext::wrap_op_compute_gas`, which implements the same forwarded-gas exclusion
/// without the REX6 abort-path handling (unreachable from those wrappers).
///
/// `$gas_before` MUST be the interpreter gas remaining captured at the very top of the handler,
/// before any storage-gas charge or wrapper-side EVM-gas work (e.g. CREATE2 memory expansion), so
/// the single measurement window covers all of the opcode's compute work. `$opcode` is the wrapped
/// opcode, whose [`STATIC_GAS_TABLE`] entry the interpreter charged before the handler was entered
/// and which therefore falls outside that window. The recorded amount is
/// `(static_gas + $gas_before − gas_after) − $storage_charged − forwarded_child_gas`: the storage
/// gas charged inside the window is subtracted back out, and gas forwarded to a child frame is
/// excluded (the child records its own compute gas). The forwarded-gas exclusion is spec-aware —
/// REX5+ excludes the revm-side `CALL_STIPEND`, pre-REX5 does not — so the result is identical to
/// the pre-existing inline recording on every spec.
///
/// Because nothing consumes EVM gas before the storage charge for the non-CREATE2 opcodes, the REX6
/// storage handlers record the same compute gas, at the same point, as the pre-REX6 layering;
/// CREATE2 differs only by folding its memory-expansion gas into this single window instead of
/// recording it separately.
///
/// Under REX7 checkpoint accounting the window is the same one — [`checkpoint_prologue!`] runs
/// ahead of the `$gas_before` capture — but the static gas is not added back: see the macro body.
///
/// On exceeding the compute-gas limit, halts the interpreter and returns from the enclosing
/// instruction handler. The early return mirrors [`compute_gas!`] so a trailing statement after
/// this macro (e.g. the pre-REX5 `resize_gas` late-record in `storage_gas_ext::create`) is only
/// reached on the non-halt path; without the return, a halt here would let a later `compute_gas!`
/// add gas to the tracker after the OOG was already set.
macro_rules! record_storage_compute_gas {
    ($context:expr, $gas_before:expr, $storage_charged:expr, $opcode:expr) => {{
        let spec = $context.host.spec_id();
        let is_rex6 = spec.is_enabled(MegaSpecId::REX6);
        let is_rex7 = spec.is_enabled(MegaSpecId::REX7);
        let gas_after = $context.interpreter.gas.remaining();
        // The per-opcode `$gas_before` window applies on every spec: under checkpoint accounting
        // the plain segment ahead of this opcode was already settled by
        // [`checkpoint_prologue!`], which also restored the gas clamp, so `$gas_before`
        // (captured after the prologue) lives on the true counter and measures the same
        // span it measures everywhere else.
        //
        // What the two differ on is the opcode's static gas. Whoever charges it — the interpreter
        // before dispatch, or an outer volatile wrapper — does so ahead of the prologue, so under
        // checkpoint accounting it is already inside the settled segment and adding it back here
        // would bill it twice.
        let mut gas_used = if is_rex7 {
            $gas_before.saturating_sub(gas_after).saturating_sub($storage_charged)
        } else {
            (const { static_gas($opcode) } + $gas_before.saturating_sub(gas_after))
                .saturating_sub($storage_charged)
        };
        // Exclude gas forwarded to a child frame. REX5+ keeps the revm-side `CALL_STIPEND` out of
        // the subtraction — a value-transferring CALL/CALLCODE mints it into the child's budget
        // instead of deducting it from the parent — so the parent's compute gas is not
        // under-counted; pre-REX5 subtracts the full child gas limit for replay parity.
        // `forwarded_child_gas` records that deducted amount so the abort path below can return it
        // to the parent.
        let mut forwarded_child_gas: u64 = 0;
        // The stipend revm mints into the child's budget without debiting the caller, booked once
        // this opcode hands the child invocation on — whether or not a child frame then runs. The
        // one path that mints nothing is the compute-limit abort below, which discards the pending
        // child before the EVM ever sees it.
        let mut minted_call_stipend: u64 = 0;
        match $context.interpreter.bytecode.action() {
            Some(InterpreterAction::NewFrame(FrameInput::Call(call_inputs))) => {
                let stipend_from_revm = if spec.is_enabled(MegaSpecId::REX5) &&
                    matches!(call_inputs.scheme, CallScheme::Call | CallScheme::CallCode) &&
                    call_inputs.transfers_value()
                {
                    gas::CALL_STIPEND
                } else {
                    0
                };
                minted_call_stipend = stipend_from_revm;
                let parent_contributed = call_inputs.gas_limit.saturating_sub(stipend_from_revm);
                forwarded_child_gas = parent_contributed;
                gas_used = gas_used.saturating_sub(parent_contributed);
            }
            Some(InterpreterAction::NewFrame(FrameInput::Create(create_inputs))) => {
                forwarded_child_gas = create_inputs.gas_limit();
                gas_used = gas_used.saturating_sub(create_inputs.gas_limit());
            }
            _ => {}
        }
        // On a compute-limit halt the pending child `NewFrame` is discarded (the child never runs),
        // but revm already deducted the forwarded gas and the outer `forward_gas_ext` erase is
        // skipped on this abort path. REX6+: return that gas to the parent before halting.
        let exceeding_result = {
            let mut additional_limit = $context.host.additional_limit().borrow_mut();
            // Re-open the settlement window at this opcode's exit before recording, so neither a
            // halt here nor the frame-final settlement can bill this segment twice.
            if is_rex7 {
                additional_limit.sync_checkpoint_baseline(gas_after);
            }
            if additional_limit.record_compute_gas(gas_used) {
                // The invocation survives this opcode, so the stipend revm minted into its budget
                // is now live, whatever becomes of the child: the callee spends it as work no
                // envelope funded, or hands it back and shrinks the envelope, or never runs at all
                // — a frame init that fails on balance or depth refunds the whole child budget,
                // mint included, into the caller's envelope, which shrinks it by the same amount.
                // Book it where the destroyed-remainder derivation can reconcile the recorded work
                // against what the transaction spent.
                additional_limit.record_minted_call_stipend(minted_call_stipend);
                None
            } else {
                Some(additional_limit.exceeding_instruction_result())
            }
        };
        if let Some(result) = exceeding_result {
            if is_rex6 {
                $context.interpreter.gas.erase_cost(forwarded_child_gas);
            }
            // The halt action is set even though `Err` is returned: a pending child `NewFrame` is
            // already set here and must be replaced (the child never runs), and the interpreter's
            // own fallback halt would spend all the remaining gas that the erase above just
            // returned. Setting it after `erase_cost` is what captures the returned gas in the
            // action.
            set_halt_action!($context.interpreter, result);
            return Err(result);
        }
    }};
}

/// Runs revm's CALL-family opcode body inside a CALL target-resolution scope.
///
/// Every `MegaETH` handler for `CALL` / `CALLCODE` / `DELEGATECALL` / `STATICCALL` reaches revm's
/// body through this function instead of calling [`instructions::contract::call`] directly, on
/// every spec and at every wrapper layer. The scope is what lets
/// [`Host::load_account_info_skip_cold_load`] tell the CALL's raw stack operand (marks beneficiary
/// access on every spec) from its EIP-7702 delegate hop (marks only from `REX6`) — two loads that
/// are otherwise identical at the host boundary. Routing every handler through one function is what
/// makes the bracket impossible to forget when a table entry or wrapper layer is added.
///
/// The scope is closed on both exit paths, so a halting CALL cannot leak it into the next opcode's
/// loads. The inner instruction is invoked directly rather than through
/// [`run_inner_instruction_or_abort`], which returns early on a halt.
///
/// `EQUIVALENCE` is the one spec whose table keeps revm's unwrapped CALL handlers, so its delegate
/// hop still marks. That is unobservable: pre-`MINI_REX` there is no detention (`AdditionalLimit`
/// and the `apply_compute_gas_limit!` wrappers are `MINI_REX`+), the reported volatile-access info
/// is `MINI_REX`-gated in `execution_result`, and `get_block_env_accesses` masks the beneficiary
/// bit out.
#[inline]
fn call_resolving_target<const KIND: u8, WIRE: InterpreterTypes, H: HostExt + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) -> InstructionExecResult {
    context.host.begin_call_target_resolution();
    let inner_outcome = {
        let ctx = InstructionContext::<'_, H, WIRE> {
            interpreter: &mut *context.interpreter,
            host: &mut *context.host,
        };
        instructions::contract::call::<KIND, _, _>(ctx)
    };
    context.host.end_call_target_resolution();
    inner_outcome
}

mod mini_rex {
    use super::*;

    /// Returns the instruction table for the `MINI_REX` spec.
    ///
    /// This is the base custom table — all 256 opcodes are initialized from scratch with
    /// compute gas tracking. Key opcode layering:
    /// - Most opcodes: `compute_gas_ext::*` (compute gas tracking only)
    /// - Block env opcodes: `volatile_data_ext::*` (gas detention)
    /// - BALANCE, EXTCODESIZE, EXTCODECOPY, EXTCODEHASH: `volatile_data_ext::*` (conditional)
    /// - SSTORE: `additional_limit_ext` → `storage_gas_ext`
    /// - LOG0–LOG4: `additional_limit_ext` → `storage_gas_ext`
    /// - CALL: `forward_gas_ext` → `storage_gas_ext`
    /// - CREATE, CREATE2: `forward_gas_ext` → `storage_gas_ext`
    /// - CALLCODE, DELEGATECALL, STATICCALL: `compute_gas_ext::*` (bug: missing `forward_gas_ext`)
    /// - SELFDESTRUCT: disabled (`control::invalid`)
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >() -> [Instruction<WIRE, H>; 256] {
        use revm::bytecode::opcode::*;
        let mut table = [Instruction::new(control::unknown); 256];

        table[STOP as usize] = Instruction::new(compute_gas_ext::stop);
        table[ADD as usize] = Instruction::new(compute_gas_ext::add);
        table[MUL as usize] = Instruction::new(compute_gas_ext::mul);
        table[SUB as usize] = Instruction::new(compute_gas_ext::sub);
        table[DIV as usize] = Instruction::new(compute_gas_ext::div);
        table[SDIV as usize] = Instruction::new(compute_gas_ext::sdiv);
        table[MOD as usize] = Instruction::new(compute_gas_ext::rem);
        table[SMOD as usize] = Instruction::new(compute_gas_ext::smod);
        table[ADDMOD as usize] = Instruction::new(compute_gas_ext::addmod);
        table[MULMOD as usize] = Instruction::new(compute_gas_ext::mulmod);
        table[EXP as usize] = Instruction::new(compute_gas_ext::exp);
        table[SIGNEXTEND as usize] = Instruction::new(compute_gas_ext::signextend);

        table[LT as usize] = Instruction::new(compute_gas_ext::lt);
        table[GT as usize] = Instruction::new(compute_gas_ext::gt);
        table[SLT as usize] = Instruction::new(compute_gas_ext::slt);
        table[SGT as usize] = Instruction::new(compute_gas_ext::sgt);
        table[EQ as usize] = Instruction::new(compute_gas_ext::eq);
        table[ISZERO as usize] = Instruction::new(compute_gas_ext::iszero);
        table[AND as usize] = Instruction::new(compute_gas_ext::bitand);
        table[OR as usize] = Instruction::new(compute_gas_ext::bitor);
        table[XOR as usize] = Instruction::new(compute_gas_ext::bitxor);
        table[NOT as usize] = Instruction::new(compute_gas_ext::not);
        table[BYTE as usize] = Instruction::new(compute_gas_ext::byte);
        table[SHL as usize] = Instruction::new(compute_gas_ext::shl);
        table[SHR as usize] = Instruction::new(compute_gas_ext::shr);
        table[SAR as usize] = Instruction::new(compute_gas_ext::sar);
        table[CLZ as usize] = Instruction::new(compute_gas_ext::clz);

        table[KECCAK256 as usize] = Instruction::new(compute_gas_ext::keccak256);

        table[ADDRESS as usize] = Instruction::new(compute_gas_ext::address);
        table[BALANCE as usize] = Instruction::new(volatile_data_ext::balance);
        table[ORIGIN as usize] = Instruction::new(compute_gas_ext::origin);
        table[CALLER as usize] = Instruction::new(compute_gas_ext::caller);
        table[CALLVALUE as usize] = Instruction::new(compute_gas_ext::callvalue);
        table[CALLDATALOAD as usize] = Instruction::new(compute_gas_ext::calldataload);
        table[CALLDATASIZE as usize] = Instruction::new(compute_gas_ext::calldatasize);
        table[CALLDATACOPY as usize] = Instruction::new(compute_gas_ext::calldatacopy);
        table[CODESIZE as usize] = Instruction::new(compute_gas_ext::codesize);
        table[CODECOPY as usize] = Instruction::new(compute_gas_ext::codecopy);

        table[GASPRICE as usize] = Instruction::new(compute_gas_ext::gasprice);
        table[EXTCODESIZE as usize] = Instruction::new(volatile_data_ext::extcodesize);
        table[EXTCODECOPY as usize] = Instruction::new(volatile_data_ext::extcodecopy);
        table[EXTCODEHASH as usize] = Instruction::new(volatile_data_ext::extcodehash);
        table[RETURNDATASIZE as usize] = Instruction::new(compute_gas_ext::returndatasize);
        table[RETURNDATACOPY as usize] = Instruction::new(compute_gas_ext::returndatacopy);
        table[BLOCKHASH as usize] = Instruction::new(volatile_data_ext::blockhash);
        table[COINBASE as usize] = Instruction::new(volatile_data_ext::coinbase);
        table[TIMESTAMP as usize] = Instruction::new(volatile_data_ext::timestamp);
        table[NUMBER as usize] = Instruction::new(volatile_data_ext::block_number);
        table[DIFFICULTY as usize] = Instruction::new(volatile_data_ext::difficulty);
        table[GASLIMIT as usize] = Instruction::new(volatile_data_ext::gas_limit_opcode);
        table[CHAINID as usize] = Instruction::new(compute_gas_ext::chainid);
        table[SELFBALANCE as usize] = Instruction::new(compute_gas_ext::selfbalance);
        table[BASEFEE as usize] = Instruction::new(volatile_data_ext::basefee);
        table[BLOBBASEFEE as usize] = Instruction::new(volatile_data_ext::blobbasefee);
        table[BLOBHASH as usize] = Instruction::new(volatile_data_ext::blobhash);

        table[POP as usize] = Instruction::new(compute_gas_ext::pop);
        table[MLOAD as usize] = Instruction::new(compute_gas_ext::mload);
        table[MSTORE as usize] = Instruction::new(compute_gas_ext::mstore);
        table[MSTORE8 as usize] = Instruction::new(compute_gas_ext::mstore8);
        table[SLOAD as usize] = Instruction::new(compute_gas_ext::sload);
        table[SSTORE as usize] = Instruction::new(additional_limit_ext::sstore);
        table[JUMP as usize] = Instruction::new(compute_gas_ext::jump);
        table[JUMPI as usize] = Instruction::new(compute_gas_ext::jumpi);
        table[PC as usize] = Instruction::new(compute_gas_ext::pc);
        table[MSIZE as usize] = Instruction::new(compute_gas_ext::msize);
        table[GAS as usize] = Instruction::new(compute_gas_ext::gas);
        table[JUMPDEST as usize] = Instruction::new(compute_gas_ext::jumpdest);
        table[TLOAD as usize] = Instruction::new(compute_gas_ext::tload);
        table[TSTORE as usize] = Instruction::new(compute_gas_ext::tstore);
        table[MCOPY as usize] = Instruction::new(compute_gas_ext::mcopy);

        table[PUSH0 as usize] = Instruction::new(compute_gas_ext::push0);
        table[PUSH1 as usize] = Instruction::new(compute_gas_ext::push1);
        table[PUSH2 as usize] = Instruction::new(compute_gas_ext::push2);
        table[PUSH3 as usize] = Instruction::new(compute_gas_ext::push3);
        table[PUSH4 as usize] = Instruction::new(compute_gas_ext::push4);
        table[PUSH5 as usize] = Instruction::new(compute_gas_ext::push5);
        table[PUSH6 as usize] = Instruction::new(compute_gas_ext::push6);
        table[PUSH7 as usize] = Instruction::new(compute_gas_ext::push7);
        table[PUSH8 as usize] = Instruction::new(compute_gas_ext::push8);
        table[PUSH9 as usize] = Instruction::new(compute_gas_ext::push9);
        table[PUSH10 as usize] = Instruction::new(compute_gas_ext::push10);
        table[PUSH11 as usize] = Instruction::new(compute_gas_ext::push11);
        table[PUSH12 as usize] = Instruction::new(compute_gas_ext::push12);
        table[PUSH13 as usize] = Instruction::new(compute_gas_ext::push13);
        table[PUSH14 as usize] = Instruction::new(compute_gas_ext::push14);
        table[PUSH15 as usize] = Instruction::new(compute_gas_ext::push15);
        table[PUSH16 as usize] = Instruction::new(compute_gas_ext::push16);
        table[PUSH17 as usize] = Instruction::new(compute_gas_ext::push17);
        table[PUSH18 as usize] = Instruction::new(compute_gas_ext::push18);
        table[PUSH19 as usize] = Instruction::new(compute_gas_ext::push19);
        table[PUSH20 as usize] = Instruction::new(compute_gas_ext::push20);
        table[PUSH21 as usize] = Instruction::new(compute_gas_ext::push21);
        table[PUSH22 as usize] = Instruction::new(compute_gas_ext::push22);
        table[PUSH23 as usize] = Instruction::new(compute_gas_ext::push23);
        table[PUSH24 as usize] = Instruction::new(compute_gas_ext::push24);
        table[PUSH25 as usize] = Instruction::new(compute_gas_ext::push25);
        table[PUSH26 as usize] = Instruction::new(compute_gas_ext::push26);
        table[PUSH27 as usize] = Instruction::new(compute_gas_ext::push27);
        table[PUSH28 as usize] = Instruction::new(compute_gas_ext::push28);
        table[PUSH29 as usize] = Instruction::new(compute_gas_ext::push29);
        table[PUSH30 as usize] = Instruction::new(compute_gas_ext::push30);
        table[PUSH31 as usize] = Instruction::new(compute_gas_ext::push31);
        table[PUSH32 as usize] = Instruction::new(compute_gas_ext::push32);

        table[DUP1 as usize] = Instruction::new(compute_gas_ext::dup1);
        table[DUP2 as usize] = Instruction::new(compute_gas_ext::dup2);
        table[DUP3 as usize] = Instruction::new(compute_gas_ext::dup3);
        table[DUP4 as usize] = Instruction::new(compute_gas_ext::dup4);
        table[DUP5 as usize] = Instruction::new(compute_gas_ext::dup5);
        table[DUP6 as usize] = Instruction::new(compute_gas_ext::dup6);
        table[DUP7 as usize] = Instruction::new(compute_gas_ext::dup7);
        table[DUP8 as usize] = Instruction::new(compute_gas_ext::dup8);
        table[DUP9 as usize] = Instruction::new(compute_gas_ext::dup9);
        table[DUP10 as usize] = Instruction::new(compute_gas_ext::dup10);
        table[DUP11 as usize] = Instruction::new(compute_gas_ext::dup11);
        table[DUP12 as usize] = Instruction::new(compute_gas_ext::dup12);
        table[DUP13 as usize] = Instruction::new(compute_gas_ext::dup13);
        table[DUP14 as usize] = Instruction::new(compute_gas_ext::dup14);
        table[DUP15 as usize] = Instruction::new(compute_gas_ext::dup15);
        table[DUP16 as usize] = Instruction::new(compute_gas_ext::dup16);

        table[SWAP1 as usize] = Instruction::new(compute_gas_ext::swap1);
        table[SWAP2 as usize] = Instruction::new(compute_gas_ext::swap2);
        table[SWAP3 as usize] = Instruction::new(compute_gas_ext::swap3);
        table[SWAP4 as usize] = Instruction::new(compute_gas_ext::swap4);
        table[SWAP5 as usize] = Instruction::new(compute_gas_ext::swap5);
        table[SWAP6 as usize] = Instruction::new(compute_gas_ext::swap6);
        table[SWAP7 as usize] = Instruction::new(compute_gas_ext::swap7);
        table[SWAP8 as usize] = Instruction::new(compute_gas_ext::swap8);
        table[SWAP9 as usize] = Instruction::new(compute_gas_ext::swap9);
        table[SWAP10 as usize] = Instruction::new(compute_gas_ext::swap10);
        table[SWAP11 as usize] = Instruction::new(compute_gas_ext::swap11);
        table[SWAP12 as usize] = Instruction::new(compute_gas_ext::swap12);
        table[SWAP13 as usize] = Instruction::new(compute_gas_ext::swap13);
        table[SWAP14 as usize] = Instruction::new(compute_gas_ext::swap14);
        table[SWAP15 as usize] = Instruction::new(compute_gas_ext::swap15);
        table[SWAP16 as usize] = Instruction::new(compute_gas_ext::swap16);

        table[LOG0 as usize] = Instruction::new(additional_limit_ext::log::<0, _, _>);
        table[LOG1 as usize] = Instruction::new(additional_limit_ext::log::<1, _, _>);
        table[LOG2 as usize] = Instruction::new(additional_limit_ext::log::<2, _, _>);
        table[LOG3 as usize] = Instruction::new(additional_limit_ext::log::<3, _, _>);
        table[LOG4 as usize] = Instruction::new(additional_limit_ext::log::<4, _, _>);

        table[CREATE as usize] = Instruction::new(forward_gas_ext::create);
        table[CREATE2 as usize] = Instruction::new(forward_gas_ext::create2);
        table[CALL as usize] = Instruction::new(forward_gas_ext::call);
        table[CALLCODE as usize] = Instruction::new(compute_gas_ext::call_code);
        table[DELEGATECALL as usize] = Instruction::new(compute_gas_ext::delegate_call);
        table[STATICCALL as usize] = Instruction::new(compute_gas_ext::static_call);

        table[INVALID as usize] = Instruction::new(compute_gas_ext::invalid);
        table[RETURN as usize] = Instruction::new(compute_gas_ext::ret);
        table[REVERT as usize] = Instruction::new(compute_gas_ext::revert);
        table[SELFDESTRUCT as usize] = Instruction::new(control::invalid);

        table
    }

    /// Returns the static gas table for the `MINI_REX` spec.
    ///
    /// Zeroes the entry of every opcode this spec's instruction table wraps in a
    /// `volatile_data_ext` handler — the block-environment reads plus the beneficiary-conditional
    /// account-touching opcodes. See [`super::volatile_data_ext::charge_static_gas`] for why those
    /// entries have to move out of the interpreter's pre-charge and into the handler.
    ///
    /// Opcodes this table dispatches to `control::invalid` (disabled) or `control::unknown` (never
    /// wired) keep their entries even though no handler consumes them. The pre-charge cannot change
    /// what such a frame pays — the handler takes the whole budget either way — only which halt it
    /// reports, and revm treats that as an implementation detail: its own table prices `CLZ`,
    /// `SLOTNUM` and the `DUPN` family at every fork, including the forks whose handlers reject
    /// them outright.
    pub(super) const fn gas_table(mut table: GasTable) -> GasTable {
        use revm::bytecode::opcode::*;

        // Unconditionally volatile: `wrap_op_detain_gas_unconditional!`.
        table[BLOCKHASH as usize] = 0;
        table[COINBASE as usize] = 0;
        table[TIMESTAMP as usize] = 0;
        table[NUMBER as usize] = 0;
        table[DIFFICULTY as usize] = 0;
        table[GASLIMIT as usize] = 0;
        table[BASEFEE as usize] = 0;
        table[BLOBBASEFEE as usize] = 0;
        table[BLOBHASH as usize] = 0;

        // Beneficiary-conditional: `wrap_op_detain_gas_conditional!`.
        table[BALANCE as usize] = 0;
        table[EXTCODESIZE as usize] = 0;
        table[EXTCODECOPY as usize] = 0;
        table[EXTCODEHASH as usize] = 0;

        table
    }
}

/// Call-like and create-like opcode handlers with 98/100 gas forwarding rule.
///
/// This module provides wrapper implementations for CALL, CALLCODE, DELEGATECALL, STATICCALL,
/// CREATE, and CREATE2 opcodes that enforce the 98/100 gas forwarding rule (retaining 2% of
/// remaining gas in the parent call instead of the standard 1/64).
///
/// The wrappers:
/// 1. Check for value transfer (CALL and CALLCODE only) to account for call stipend
/// 2. Call the underlying opcode implementation
/// 3. Cap the forwarded gas to 98% of the parent's remaining gas
/// 4. Preserve the call stipend (2300 gas) when value is transferred (CALL/CALLCODE only)
/// 5. Support both Call and Create frame types
pub mod forward_gas_ext {
    use super::*;

    /// Macro to wrap call-like and create-like opcodes with 98/100 gas forwarding rule.
    ///
    /// This macro generates a wrapper function that:
    /// 1. Optionally checks for value transfer (`has_transfer`) for CALL opcode
    /// 2. Calls the wrapped opcode handler
    /// 3. Caps the forwarded gas to 98/100 of the remaining gas
    /// 4. Adjusts for call stipend if value is being transferred
    /// 5. Supports both Call and Create frame types
    ///
    /// # Parameters
    /// - `$fn_name`: Name of the generated function
    /// - `$opcode_name`: String name of the opcode (for documentation)
    /// - `$wrapped_fn`: Path to the wrapped instruction implementation
    /// - `$has_transfer_logic`: Expression to determine if value is being transferred (e.g.,
    ///   `has_transfer` or `false`)
    ///
    /// The `@checkpoint_tail` variant additionally re-applies the REX7 gas clamp on the way out. It
    /// is used by `CREATE` / `CREATE2`, whose table entries dispatch straight here; the CALL family
    /// is wrapped once more by `volatile_data_ext::wrap_call_volatile_check`, which owns the
    /// epilogue so that it lands after the detention cap that wrapper installs.
    macro_rules! wrap_gas_cap {
        ($fn_name:ident, $opcode_name:expr, $wrapped_fn:path, $has_transfer_logic:expr) => {
            wrap_gas_cap!(@inner $fn_name, $opcode_name, $wrapped_fn, $has_transfer_logic, false);
        };
        (@checkpoint_tail $fn_name:ident, $opcode_name:expr, $wrapped_fn:path, $has_transfer_logic:expr) => {
            wrap_gas_cap!(@inner $fn_name, $opcode_name, $wrapped_fn, $has_transfer_logic, true);
        };
        (@inner $fn_name:ident, $opcode_name:expr, $wrapped_fn:path, $has_transfer_logic:expr, $checkpoint_tail:literal) => {
            #[doc = concat!("`", $opcode_name, "` opcode with 98/100 gas forwarding rule.")]
            #[inline]
            pub fn $fn_name<
                WIRE: InterpreterTypes<Stack: StackInspectTr>,
                H: HostExt + ContextTr + JournalInspectTr + ?Sized,
            >(
                context: InstructionContext<'_, H, WIRE>,
            ) -> InstructionExecResult {
                // Determine if there's a value transfer (only applies to CALL opcode).
                let has_transfer = $has_transfer_logic(&context);

                // Call the wrapped opcode handler.
                run_inner_instruction_or_abort!($wrapped_fn, context, inner_outcome);

                // Cap the forwarded gas to the child call/create to the 98/100 of the remaining
                // gas.
                match context.interpreter.bytecode.action() {
                    Some(InterpreterAction::NewFrame(FrameInput::Call(call_inputs))) => {
                        // The forwarded gas to the child call should be further restricted to the
                        // 98/100 of the remaining gas. Here, we first recover the total
                        // gas left in the parent call and then cap the child call gas
                        // limit if necessary.

                        // We recover the forwarded gas to the child call from the parent call.
                        let child_gas = call_inputs.gas_limit as u128;
                        // There may be a call stipend if there is value to be transferred.
                        let transfer_gas_stipend =
                            if has_transfer { gas::CALL_STIPEND as u128 } else { 0 };
                        let forwarded_gas = child_gas - transfer_gas_stipend; // Safe from underflow

                        // Recover the remaining gas in the parent call before forwarding to the
                        // child call.
                        let parent_original_gas_left =
                            context.interpreter.gas.remaining() as u128 + forwarded_gas;

                        // Calculate the amount of gas that should be returned to the parent call
                        // under the 98/100 rule.
                        let forwarded_gas_cap =
                            parent_original_gas_left - parent_original_gas_left * 2 / 100;
                        let capped_forwarded_gas = min(forwarded_gas, forwarded_gas_cap);
                        let gas_to_return = forwarded_gas - capped_forwarded_gas; // Safe from underflow

                        // Recalculate the child gas
                        let new_child_gas = capped_forwarded_gas + transfer_gas_stipend;

                        //  Return the gas to the parent call.
                        context.interpreter.gas.erase_cost(gas_to_return as u64);

                        // Set the child call gas limit to the capped value.
                        // Note: REX4+ STORAGE_CALL_STIPEND is applied later in
                        // AdditionalLimit::before_frame_init, which owns the full
                        // stipend lifecycle (grant → compute cap → burn on return).
                        call_inputs.gas_limit = new_child_gas as u64;
                    }
                    Some(InterpreterAction::NewFrame(FrameInput::Create(create_inputs))) => {
                        // The forwarded gas to the child create should be further restricted to the
                        // 98/100 of the remaining gas. CREATE opcodes don't have a call
                        // stipend, so the logic is simpler.

                        // We recover the forwarded gas from the parent call.
                        let child_gas = create_inputs.gas_limit() as u128;
                        let forwarded_gas = child_gas; // No stipend for CREATE

                        // Recover the remaining gas in the parent call before forwarding to the
                        // child create.
                        let parent_original_gas_left =
                            context.interpreter.gas.remaining() as u128 + forwarded_gas;

                        // Calculate the amount of gas that should be returned to the parent call
                        // under the 98/100 rule.
                        let forwarded_gas_cap =
                            parent_original_gas_left - parent_original_gas_left * 2 / 100;
                        let capped_forwarded_gas = min(forwarded_gas, forwarded_gas_cap);
                        let gas_to_return = forwarded_gas - capped_forwarded_gas; // Safe from underflow

                        //  Return the gas to the parent call.
                        context.interpreter.gas.erase_cost(gas_to_return as u64);

                        // Set the child create gas limit to the capped value.
                        create_inputs.set_gas_limit(capped_forwarded_gas as u64);
                    }
                    _ => {}
                }
                if $checkpoint_tail {
                    checkpoint_epilogue!(context);
                }
                inner_outcome
            }
        };
    }

    // Helper function to check if CALL has value transfer
    #[inline]
    fn check_call_has_transfer<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ?Sized,
    >(
        context: &InstructionContext<'_, H, WIRE>,
    ) -> bool {
        if let Some(value) = context.interpreter.stack.inspect::<2>() {
            !value.is_zero()
        } else {
            false
        }
    }

    // Helper function for opcodes without value transfer
    #[inline]
    fn no_transfer<WIRE: InterpreterTypes<Stack: StackInspectTr>, H: HostExt + ?Sized>(
        _context: &InstructionContext<'_, H, WIRE>,
    ) -> bool {
        false
    }

    wrap_gas_cap!(call, "CALL", storage_gas_ext::call, check_call_has_transfer);
    wrap_gas_cap!(call_code, "CALLCODE", storage_gas_ext::call_code, check_call_has_transfer);
    wrap_gas_cap!(delegate_call, "DELEGATECALL", storage_gas_ext::delegate_call, no_transfer);
    wrap_gas_cap!(static_call, "STATICCALL", storage_gas_ext::static_call, no_transfer);
    wrap_gas_cap!(
        @checkpoint_tail create, "CREATE", storage_gas_ext::create::<WIRE, false, H>, no_transfer
    );
    wrap_gas_cap!(
        @checkpoint_tail create2, "CREATE2", storage_gas_ext::create::<WIRE, true, H>, no_transfer
    );
}

/** Volatile data access opcode handlers with compute gas limit enforcement.

These custom instruction handlers override opcodes that access volatile data (block environment,
beneficiary account data, oracle contract) to lower the compute gas limit.
This prevents `DoS` attacks while allowing storage operations to continue with full transaction gas.

# Compute Gas Limit Enforcement

When volatile data is accessed:
1. The opcode executes normally (calls host method, processes data)
2. If this is the first volatile data access in the transaction:
   - The compute gas limit is lowered based on the type:
     * Block environment or beneficiary: `BLOCK_ENV_ACCESS_REMAINING_GAS` (20M compute gas)
     * Oracle contract: `ORACLE_ACCESS_REMAINING_GAS` (1M compute gas)
3. Most restrictive limit wins: If additional volatile data with different limit is accessed,
   the minimum (most restrictive) limit is applied, regardless of access order
4. All subsequent compute operations are limited by this compute gas limit
5. Storage operations (SSTORE, account creation) continue with full transaction gas

# Volatile Data Access Disable (Rex4+)

When `disableVolatileDataAccess()` is active, the handlers check **before** executing the
opcode and revert immediately if the access would be volatile.
This ensures that disabled volatile accesses do not pollute the tracker's `volatile_data_accessed`
bitmap or lower the `compute_gas_limit`.

Through REX6 the check runs before *anything* is charged for the opcode, including its static gas:
every spec's static gas table zeroes the entries of the opcodes handled here, and the entry is
charged via [`charge_static_gas`] only after the check has declined to fire.
That is what lets the rejection be a revert that keeps the frame's whole remaining gas even when
that gas would not have covered the opcode's static cost.

REX7 charges the static entry on a rejection, before producing the revert: the payload is
unchanged, but the static fee stays charged and is settled into compute gas with the open
segment at frame exit. A passing guard still charges at the success-path position (after the
checkpoint prologue has restored the true counter). A frame that cannot afford the static fee
runs out of gas instead of reaching the revert. Frozen specs are unchanged.

# Where the Static Gas Lands Once the Guard Declines

`MegaETH`'s gas schedule is the one revm charged from inside each opcode's body, before a static gas
table existed, so each guarded opcode's charge has to go back to that body position rather than to
whichever wrapper is convenient. It splits the guarded set in two:

- The block-environment reads and `SELFBALANCE` — bodies that open with their gas charge — are
  charged here, at the top of the handler, before the inner instruction runs.
- The account-reading opcodes (`BALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`, `SLOAD`,
  `SELFDESTRUCT`) — bodies that pop their operands and read the host *first* — are charged by the
  `compute_gas_ext` `@self_charged` handlers, after the inner instruction. A frame too poor to pay
  therefore still consumes its operands and performs the host read, so it halts on whatever the body
  raised first and its volatile-access mark stands, instead of being pre-empted out of gas.
- The CALL family is the exception: its body derives the child frame's gas limit from what is left
  after the charge, so the charge cannot move behind the body without changing what the child is
  forwarded. See [`wrap_call_volatile_check`].

# Two Categories of Opcodes

## Block Environment Opcodes (Always Volatile)
These opcodes ALWAYS access volatile data and apply 20M compute gas limit:
- TIMESTAMP, NUMBER, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE, BLOCKHASH, BLOBBASEFEE, BLOBHASH

## Account-Accessing Opcodes (Conditionally Volatile)
These opcodes only SOMETIMES access volatile data (20M compute gas limit when volatile):
- `BALANCE(beneficiary_address)` → volatile, applies 20M compute gas limit
- `BALANCE(other_address)` → not volatile, no limit
- EXTCODESIZE/EXTCODECOPY/EXTCODEHASH → same conditional behavior

For conditional opcodes, the instruction handler peeks the target address from the stack before
executing the opcode to determine if the access would be volatile.

## Oracle SLOAD (Rex3+)
SLOAD targeting the oracle contract is volatile and applies the oracle compute gas limit.
The target address comes from `interpreter.input.target_address()` (not from the stack).
*/
pub mod volatile_data_ext {
    use super::*;

    use alloy_primitives::Address;

    use crate::{
        volatile_data_access_disabled_revert_data, VolatileDataAccessType, ORACLE_CONTRACT_ADDRESS,
    };

    /// Applies the compute gas limit from the volatile data tracker to the additional limit.
    ///
    /// This is safe to call unconditionally after any instruction: `get_compute_gas_limit()`
    /// returns `None` if no volatile data has been accessed in this transaction, and if a
    /// prior instruction already set the limit, re-applying the same value is idempotent.
    macro_rules! apply_compute_gas_limit {
        ($context:expr) => {
            let compute_gas_limit =
                $context.host.volatile_data_tracker().borrow().get_compute_gas_limit();
            if let Some(limit) = compute_gas_limit {
                $context.host.additional_limit().borrow_mut().set_compute_gas_limit(limit);
            }
        };
    }

    /// Debug-build tripwire for the frozen detention window (CALL family and EXTCODECOPY).
    ///
    /// revm 27 loaded these opcodes' target account before charging the opcode's own costs, so a
    /// frame that ran out of gas on those charges had already marked beneficiary access and the
    /// rest of the transaction ran detained. revm 40 charges first, so the same frame halts with
    /// the beneficiary unmarked and the rest of the transaction runs undetained. Release builds
    /// accept the revm 40 order; the full-history replay that gates a release is what proves no
    /// historical transaction sits in that window — and this check, compiled only under debug
    /// assertions, is the tripwire such a replay must run with: it fires exactly when a window
    /// transaction is found, so the wrapper backfill archived for that case gets implemented
    /// instead of the divergence going unnoticed.
    ///
    /// REX7 specifies that same charge-before-load order, so a window miss is not a replay
    /// divergence there and this check must not fire. The gate is `!is_enabled(REX7)` rather
    /// than a table split because the CALL-family wrapper is shared with the frozen specs.
    ///
    /// The check over-approximates on purpose — it does not reconstruct how far revm 27 would
    /// have gotten — with one exception: a `MemoryOOG` halt is never routed here, because memory
    /// expansion was charged before the load under revm 27 as well, so that halt shape cannot
    /// diverge. An already-marked transaction is skipped for the same reason: the mark is
    /// idempotent, so losing a duplicate cannot change replay.
    #[cfg(debug_assertions)]
    fn debug_check_frozen_detention_window<H: HostExt + ?Sized>(
        host: &mut H,
        opcode: u8,
        raw_target: Option<Address>,
    ) {
        if host.spec_id().is_enabled(MegaSpecId::REX7) {
            return;
        }
        let Some(target) = raw_target else { return };
        if host.volatile_data_tracker().borrow().has_accessed_beneficiary_balance() {
            return;
        }
        let beneficiary = host.beneficiary_address();
        let hits_beneficiary = target == beneficiary ||
            (host.spec_id().is_enabled(MegaSpecId::REX6) &&
                host.best_effort_resolve_eip7702_delegate_address(target) == beneficiary);
        debug_assert!(
            !hits_beneficiary,
            "frozen detention window hit: opcode 0x{opcode:02x} ran out of gas before loading \
             the beneficiary, which revm 27 marked (detaining the rest of the transaction). \
             Replaying this transaction diverges from its historical execution; implement the \
             wrapper backfill archived in REVM_40_REVIEW_GUIDE.md (wontfix #20)."
        );
    }

    /// Rejects the guarded opcode with the `disableVolatileDataAccess` revert data and returns from
    /// the enclosing handler, snapshotting the frame's gas as it stands.
    ///
    /// Through REX6 nothing has been debited at this point — the guarded opcodes are excluded from
    /// the interpreter's static-gas pre-charge and are charged by [`charge_static_gas`] only once
    /// the guard has declined to fire — so the snapshot is the gas the frame held on entry and the
    /// parent gets all of it back. REX7 charges the static entry first, so the snapshot already
    /// reflects that debit and the revert does not refund it. Debiting and refunding around the
    /// guard instead would be observably wrong for a frozen-spec frame holding less gas than the
    /// pre-charge: it would never reach the guard at all.
    macro_rules! revert_volatile_access_disabled {
        ($context:expr, $opcode:ident, $access_type:expr) => {{
            $context.interpreter.bytecode.set_action(InterpreterAction::new_return(
                InstructionResult::Revert,
                volatile_data_access_disabled_revert_data($access_type),
                $context.interpreter.gas,
            ));
            return Err(InstructionResult::Revert);
        }};
    }

    /// Macro to create opcode handlers for **unconditionally volatile** opcodes.
    ///
    /// These opcodes (TIMESTAMP, NUMBER, etc.) always access volatile data.
    /// The handler:
    /// 1. Checks if volatile data access is disabled (Rex4+) — if so, reverts immediately
    ///    **before** executing the opcode, avoiding any side effects on the tracker.
    /// 2. Charges the opcode's static gas, which this spec's gas table left to the handler. These
    ///    opcodes are the ones whose revm body opens with its gas charge — `gas!(BASE)` /
    ///    `gas!(BLOCKHASH)` / `gas!(VERYLOW)` ahead of the operand pop and the host read — so
    ///    charging here, before the inner handler, is that same position.
    /// 3. Executes the original instruction.
    /// 4. Applies the compute gas limit via `apply_compute_gas_limit!`.
    macro_rules! wrap_op_detain_gas_unconditional {
    ($fn_name:ident, $opcode:ident, $original_fn:path, $access_type:expr) => {
        #[doc = concat!("`", stringify!($opcode), "` opcode with compute gas limit enforcement on volatile data access.")]
        #[inline]
        pub fn $fn_name<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
            context: InstructionContext<'_, H, WIRE>,
        ) -> InstructionExecResult {
            // Rex4+: revert before executing if volatile data access is disabled.
            if context.host.volatile_access_disabled() {
                revert_volatile_access_disabled!(context, $opcode, $access_type);
            }
            charge_static_gas!(context, $opcode);

            run_inner_instruction_or_abort!($original_fn, context, inner_outcome);
            apply_compute_gas_limit!(context);
            inner_outcome
        }
    };
    }

    /// Macro to create opcode handlers for **conditionally volatile** opcodes.
    ///
    /// These opcodes (BALANCE, EXTCODESIZE, EXTCODECOPY, EXTCODEHASH) are volatile only when
    /// targeting the block beneficiary address.
    /// The handler:
    /// 1. Peeks the target address from the stack (position 0) without consuming it.
    /// 2. If the target is the beneficiary and volatile access is disabled, reverts immediately
    ///    **before** executing the opcode.
    /// 3. Otherwise executes the instruction normally and applies gas detention if volatile data
    ///    was accessed.
    ///
    /// The opcode's static gas is *not* charged here. Every opcode in this family charges it from
    /// inside its revm body, after the operand pop and after the host load that marks beneficiary
    /// access — so the handlers this delegates to are the `_self_charged` variants, which charge it
    /// at that point instead.
    macro_rules! wrap_op_detain_gas_conditional {
    ($fn_name:ident, $opcode:ident, $original_fn:path) => {
        #[doc = concat!("`", stringify!($opcode), "` opcode with compute gas limit enforcement on volatile data access.")]
        #[inline]
        pub fn $fn_name<
            WIRE: InterpreterTypes<Stack: StackInspectTr>,
            H: HostExt + ContextTr + JournalInspectTr + ?Sized,
        >(
            context: InstructionContext<'_, H, WIRE>,
        ) -> InstructionExecResult {
            // Peek the target address from the stack to check if it's the beneficiary.
            // Rex4+: If targeting the beneficiary while volatile access is disabled, revert
            // before executing the opcode to avoid polluting the tracker.
            if let Some(addr_word) = context.interpreter.stack.inspect::<0>() {
                let target: Address = addr_word.into_address();
                let beneficiary = context.host.beneficiary_address();
                if target == beneficiary && context.host.volatile_access_disabled() {
                    revert_volatile_access_disabled!(
                        context,
                        $opcode,
                        VolatileDataAccessType::Beneficiary
                    );
                }
            }

            // The raw stack target, captured before the body pops it, for the frozen-window
            // tripwire. EXTCODECOPY is the family member with a real window (its revm body
            // charges the copy cost before the load); for the others the hook doubles as a
            // running check that their loads do mark before any out-of-gas halt.
            #[cfg(debug_assertions)]
            let tripwire_target: Option<Address> =
                context.interpreter.stack.inspect::<0>().map(|w| w.into_address());

            run_inner_instruction_or_abort!(
                $original_fn,
                context,
                inner_outcome,
                on_plain_oog: debug_check_frozen_detention_window(
                    context.host,
                    opcode::$opcode,
                    tripwire_target
                )
            );
            apply_compute_gas_limit!(context);
            inner_outcome
        }
    };
    }

    // Unconditional volatile opcodes — always access volatile data, no stack inspection needed.
    wrap_op_detain_gas_unconditional!(
        timestamp,
        TIMESTAMP,
        compute_gas_ext::timestamp,
        VolatileDataAccessType::Timestamp
    );
    wrap_op_detain_gas_unconditional!(
        block_number,
        NUMBER,
        compute_gas_ext::number,
        VolatileDataAccessType::BlockNumber
    );
    wrap_op_detain_gas_unconditional!(
        difficulty,
        DIFFICULTY,
        compute_gas_ext::difficulty,
        VolatileDataAccessType::Difficulty
    );
    wrap_op_detain_gas_unconditional!(
        gas_limit_opcode,
        GASLIMIT,
        compute_gas_ext::gaslimit,
        VolatileDataAccessType::GasLimit
    );
    wrap_op_detain_gas_unconditional!(
        basefee,
        BASEFEE,
        compute_gas_ext::basefee,
        VolatileDataAccessType::BaseFee
    );
    wrap_op_detain_gas_unconditional!(
        coinbase,
        COINBASE,
        compute_gas_ext::coinbase,
        VolatileDataAccessType::Coinbase
    );
    wrap_op_detain_gas_unconditional!(
        blockhash,
        BLOCKHASH,
        compute_gas_ext::blockhash,
        VolatileDataAccessType::BlockHash
    );
    wrap_op_detain_gas_unconditional!(
        blobbasefee,
        BLOBBASEFEE,
        compute_gas_ext::blobbasefee,
        VolatileDataAccessType::BlobBaseFee
    );
    wrap_op_detain_gas_unconditional!(
        blobhash,
        BLOBHASH,
        compute_gas_ext::blobhash,
        VolatileDataAccessType::BlobHash
    );

    // Conditional volatile opcodes — volatile only when targeting the block beneficiary.
    wrap_op_detain_gas_conditional!(balance, BALANCE, compute_gas_ext::balance);
    wrap_op_detain_gas_conditional!(extcodesize, EXTCODESIZE, compute_gas_ext::extcodesize);
    wrap_op_detain_gas_conditional!(extcodecopy, EXTCODECOPY, compute_gas_ext::extcodecopy);
    wrap_op_detain_gas_conditional!(extcodehash, EXTCODEHASH, compute_gas_ext::extcodehash);
    wrap_op_detain_gas_conditional!(
        selfdestruct,
        SELFDESTRUCT,
        compute_gas_ext::selfdestruct_self_charged
    );

    /// REX5+ SELFDESTRUCT outer wrapper: beneficiary volatile-access guard ahead of the
    /// `storage_gas_ext::selfdestruct` layer (which under REX6 also records the existing-target
    /// balance-update accounting), then the compute-gas-limit application.
    ///
    /// This is the conditional-volatile shape of [`wrap_op_detain_gas_conditional`] (it cannot be
    /// macro-generated because of the extra REX6 source check below): when volatile access is
    /// disabled and the stack target is the beneficiary, revert before any storage-layer side
    /// effect. REX6 additionally guards the executing contract (the *source*, whose balance is read
    /// and zeroed): REX5 inspected only the stack target, so when the source itself was the
    /// beneficiary its state was still observed without `disableVolatileDataAccess` rejecting.
    /// The source check is REX6-gated, leaving REX5 byte-for-byte frozen. When volatile access is
    /// *enabled*, the beneficiary is already balance-marked before its own code runs (it is reached
    /// as the tx recipient or a CALL target, both of which mark it), so detention engages without a
    /// SELFDESTRUCT-specific hook.
    #[inline]
    pub fn selfdestruct_with_beneficiary_guard<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        if context.host.volatile_access_disabled() {
            let beneficiary = context.host.beneficiary_address();
            // Confirm the SELFDESTRUCT has a target operand first: a stack-underflow SELFDESTRUCT
            // must keep revm's `StackUnderflow` halt and not be pre-empted by a beneficiary revert.
            // The guards apply only once the opcode actually acts on a target.
            if let Some(addr_word) = context.interpreter.stack.inspect::<0>() {
                let target: Address = addr_word.into_address();
                let spec = context.host.spec_id();
                // REX6: the executing contract (source) reading and zeroing its own balance is
                // itself a beneficiary observation. Frozen off pre-REX6, where only the stack
                // target below was guarded.
                let hits_source = spec.is_enabled(MegaSpecId::REX6) &&
                    context.interpreter.input.target_address() == beneficiary;
                // All specs: the stack target (the value-transfer destination).
                let hits_target = target == beneficiary;
                if hits_source || hits_target {
                    // REX7 charge-on-reject: the static entry is paid even though the body never
                    // runs. Frozen specs keep the historical zero-charge reject.
                    if spec.is_enabled(MegaSpecId::REX7) {
                        charge_static_gas!(context, SELFDESTRUCT);
                    }
                    revert_volatile_access_disabled!(
                        context,
                        SELFDESTRUCT,
                        VolatileDataAccessType::Beneficiary
                    );
                }
            }
        }

        run_inner_instruction_or_abort!(
            super::storage_gas_ext::selfdestruct,
            context,
            inner_outcome
        );
        apply_compute_gas_limit!(context);
        inner_outcome
    }

    /// `SELFBALANCE` opcode with compute gas limit enforcement on volatile data access.
    ///
    /// SELFBALANCE is conditionally volatile when the current contract is the beneficiary.
    /// Unlike the other beneficiary-conditional opcodes (BALANCE, EXTCODESIZE, etc.),
    /// the target comes from `interpreter.input.target_address()` (the executing contract),
    /// not from a stack operand, so `wrap_op_detain_gas_conditional` cannot be reused.
    ///
    /// It also charges its static gas here rather than from inside the compute-gas window, for the
    /// same reason as the block-environment reads: revm's body opens with `gas!(LOW)` and only then
    /// reads the balance, so this is that position. The `check!(ISTANBUL)` the body runs first
    /// never fires — every `MegaSpecId` maps to a post-Istanbul Ethereum spec.
    #[inline]
    pub fn selfbalance<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        let target = context.interpreter.input.target_address();
        let beneficiary = context.host.beneficiary_address();
        if target == beneficiary && context.host.volatile_access_disabled() {
            revert_volatile_access_disabled!(
                context,
                SELFBALANCE,
                VolatileDataAccessType::Beneficiary
            );
        }
        charge_static_gas!(context, SELFBALANCE);

        run_inner_instruction_or_abort!(compute_gas_ext::selfbalance, context, inner_outcome);
        apply_compute_gas_limit!(context);
        inner_outcome
    }

    /// `SLOAD` opcode with compute gas limit enforcement on volatile data access.
    ///
    /// SLOAD is conditionally volatile when targeting the oracle contract.
    /// Unlike the beneficiary-conditional opcodes, the target address comes from
    /// `interpreter.input.target_address()` (the current contract), not from the stack.
    ///
    /// The handler checks if the SLOAD targets the oracle contract and volatile access is
    /// disabled — if so, reverts before executing the instruction.
    ///
    /// The static gas is charged by [`compute_gas_ext::sload_self_charged`], after the host read —
    /// revm's body loads the slot first and prices it afterwards, and it is that load which marks
    /// oracle access.
    #[inline]
    pub fn sload<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        // Rex4+: If SLOAD targets the oracle contract and volatile access is disabled,
        // revert before executing to avoid polluting the tracker.
        let target = context.interpreter.input.target_address();
        if target == ORACLE_CONTRACT_ADDRESS && context.host.volatile_access_disabled() {
            revert_volatile_access_disabled!(context, SLOAD, VolatileDataAccessType::Oracle);
        }

        run_inner_instruction_or_abort!(
            compute_gas_ext::sload_self_charged,
            context,
            inner_outcome
        );
        apply_compute_gas_limit!(context);
        inner_outcome
    }

    /// Macro to create opcode handlers for **conditionally volatile CALL-like** opcodes.
    ///
    /// These opcodes (CALL, STATICCALL, DELEGATECALL, CALLCODE) are volatile only when
    /// targeting the block beneficiary address.
    ///
    /// When volatile access is disabled, the handler peeks the stack target (position 1) and
    /// reverts **before** executing the opcode if the target is the beneficiary — avoiding tracker
    /// pollution by the opcode's own account loads. Under REX6 it compares the target's one-hop
    /// EIP-7702 delegate so a call to a delegator pointing at the beneficiary is also caught; that
    /// resolution (a DB read) is gated behind the disabled check so it stays off the enabled
    /// path, where the raw call proceeds and the host marks the delegate as it is loaded
    /// (see [`call_resolving_target`]). `<=` REX5 compares the raw stack operand (frozen).
    /// Otherwise it delegates to the existing `forward_gas_ext` handler.
    ///
    /// # Where the static gas is charged
    ///
    /// This family is the one that cannot put its static gas back where revm's body charges it.
    /// The body charges it as part of the call cost, after the target load — but it then derives
    /// the child's gas limit from what the frame has left, and `forward_gas_ext` re-derives its
    /// 98/100 cap from the same quantity. A charge taken after the body would therefore hand the
    /// child ~98 gas more than the schedule allows. Charging it before the body keeps every
    /// forwarded amount identical, because the same total is deducted before either split is
    /// computed; only the order of the deductions within the body differs, which is unobservable
    /// for a frame that can afford them.
    ///
    /// A frame that *cannot* afford the charge is the case where that order shows: the body never
    /// runs, so it never loads the target and never marks beneficiary access. Through REX6 that
    /// window is a frozen replay hazard (wontfix #20, tripwire below). REX7 specifies it: the mark
    /// is produced when the target account is loaded, so a frame that cannot pay the pre-load fees
    /// produces none.
    ///
    /// REX7 also charges the static entry on a disable rejection (charge-on-reject); frozen specs
    /// still reject for free. The charge sits in the open segment and the frame-exit settlement
    /// records it as compute.
    ///
    /// What is reachable on every spec is the tail: the detention cap below is applied on every
    /// path out of this handler, including the out-of-gas one, so an access marked by an
    /// already-returned inner frame is still propagated into the transaction's compute budget.
    macro_rules! wrap_call_volatile_check {
    ($fn_name:ident, $opcode:ident, $inner_fn:path) => {
        #[doc = concat!("`", stringify!($opcode), "` opcode with volatile data access disabled check for beneficiary.")]
        #[inline]
        pub fn $fn_name<
            WIRE: InterpreterTypes<Stack: StackInspectTr>,
            H: HostExt + ContextTr + JournalInspectTr + ?Sized,
        >(
            context: InstructionContext<'_, H, WIRE>,
        ) -> InstructionExecResult {
            let inner_outcome: InstructionExecResult;
            let spec = context.host.spec_id();
            let is_rex7 = spec.is_enabled(MegaSpecId::REX7);
            // Rex4+: If targeting the beneficiary while volatile access is disabled, revert before
            // executing the opcode to avoid polluting the tracker. Only this disabled path can
            // revert and only it needs the EIP-7702 delegate resolved, so the resolve (a DB read)
            // is gated behind the disabled check to keep it off the common (enabled) hot path —
            // enabled-access detention is marked by the host as the CALL loads the resolved
            // delegate.
            let mut reject_disabled = false;
            if context.host.volatile_access_disabled() {
                // Peek the target address from the stack (position 1 for CALL-like opcodes:
                // stack layout is [gas_limit, to, ...]).
                if let Some(addr_word) = context.interpreter.stack.inspect::<1>() {
                    let target: Address = addr_word.into_address();
                    let beneficiary = context.host.beneficiary_address();
                    // The raw target already being the beneficiary observes beneficiary state
                    // regardless of where it itself delegates, so check it first — `||` short-circuits
                    // so no EIP-7702 delegate is resolved (and no DB read happens) in that case.
                    // REX6 otherwise resolves the delegate one hop so a CALL to a delegator `A` whose
                    // code points at `B == beneficiary` is also caught; <= REX5 compares the raw
                    // operand (frozen). The resolve is best-effort: a DB error (e.g. the delegate's
                    // code failing to load) falls back to the raw target WITHOUT stashing a
                    // `ctx.error`, so a malformed CALL that underflows before it ever runs the target
                    // keeps its `StackUnderflow` rather than surfacing a spurious DB error from this
                    // precheck. The opcode's real execution path reads the account again and owns
                    // surfacing any genuine failure.
                    if target == beneficiary ||
                        (spec.is_enabled(MegaSpecId::REX6) &&
                            context.host.best_effort_resolve_eip7702_delegate_address(target) ==
                                beneficiary)
                    {
                        reject_disabled = true;
                    }
                }
            }
            // The raw stack target, captured before the body pops it, for the frozen-window
            // tripwire. The CALL family is where the window lives: this wrapper's static charge
            // and the body's value-transfer charge both precede the load that marks.
            #[cfg(debug_assertions)]
            let tripwire_target: Option<Address> =
                context.interpreter.stack.inspect::<1>().map(|w| w.into_address());

            // REX7 charges the static entry even when the guard will reject, so the fee lands in
            // the open segment and the revert does not refund it. Frozen specs keep charging only
            // after the guard declines, which is the zero-charge reject the historical tests pin.
            // Charged here rather than after the body on the success path — see the macro's doc
            // comment. The charge does not return early: the detention tail below has to run on
            // this path too.
            if is_rex7 || !reject_disabled {
                const STATIC_GAS: u64 = static_gas(opcode::$opcode);
                if !context.interpreter.gas.record_regular_cost(STATIC_GAS) {
                    #[cfg(debug_assertions)]
                    debug_check_frozen_detention_window(
                        context.host,
                        opcode::$opcode,
                        tripwire_target,
                    );
                    apply_compute_gas_limit!(context);
                    return Err(InstructionResult::OutOfGas);
                }
            }
            if reject_disabled {
                revert_volatile_access_disabled!(
                    context,
                    $opcode,
                    VolatileDataAccessType::Beneficiary
                );
            }

            // Delegate to the existing forward_gas_ext handler via reborrow so that
            // `context` remains usable for `apply_compute_gas_limit!` afterward.
            {
                let ctx = InstructionContext::<'_, H, WIRE> {
                    interpreter: &mut *context.interpreter,
                    host: &mut *context.host,
                };
                // The inner handler already recorded its outcome in the interpreter action; the
                // detention below must run regardless, so the result is carried to the tail.
                inner_outcome = $inner_fn(ctx);
            }

            // A plain out-of-gas halt from the body can be its value-transfer charge, which
            // sits before the load: the frozen-window tripwire has to look at it.
            #[cfg(debug_assertions)]
            if matches!(inner_outcome, Err(InstructionResult::OutOfGas)) {
                debug_check_frozen_detention_window(context.host, opcode::$opcode, tripwire_target);
            }

            // Propagate the detained compute gas limit if the CALL triggered beneficiary
            // access (marked by the host as the CALL's target account was loaded).
            // `apply_compute_gas_limit!` only touches the tracker and `AdditionalLimit`,
            // not interpreter state, so it is safe in any interpreter state (including
            // `NewFrame` after a successful CALL).
            apply_compute_gas_limit!(context);
            // REX7: re-clamp for a CALL that never published a child frame (an insufficient balance
            // or depth rejection pushes 0 and lets the frame keep running). The epilogue is what
            // keeps the following plain segment bounded, and it sits after the cap above so a CALL
            // that just marked beneficiary access clamps against the detained headroom.
            checkpoint_epilogue!(context);
            inner_outcome
        }
    };
    }

    // Conditionally volatile CALL-like opcodes — volatile only when targeting the block
    // beneficiary. These wrap forward_gas_ext handlers with a pre-execution beneficiary check.
    wrap_call_volatile_check!(call, CALL, forward_gas_ext::call);
    wrap_call_volatile_check!(static_call, STATICCALL, forward_gas_ext::static_call);
    wrap_call_volatile_check!(delegate_call, DELEGATECALL, forward_gas_ext::delegate_call);
    wrap_call_volatile_check!(call_code, CALLCODE, forward_gas_ext::call_code);

    /* Checkpoint variants of the volatile handlers (REX7+).

    Under checkpoint accounting the volatile opcodes stay wrapped — they are checkpoints. The
    prologue settles the open plain segment and restores the gas clamp, revm's raw instruction runs
    on the true counter, the body's own gas is recorded per opcode, the detention cap is applied
    from the fully settled usage exactly as the per-opcode order applies it, and the epilogue
    re-clamps against the possibly-lowered headroom.

    Each handler still charges the opcode's static gas at the position its per-opcode counterpart
    charges it on the success path, because that position decides what an underfunded frame has
    already done when it halts. The one REX7 change is charge-on-reject: a disable rejection
    debits the static entry and then reverts, so the fee lands in the open segment for the
    frame-exit settlement. A passing guard is unchanged — prologue, `gas_before`, then the
    body charge — so the static fee is taken on the restored true counter.

    The frozen detention-window tripwire the per-opcode conditional wrapper carries is not
    repeated here: it watches for historical transactions whose replay would diverge across a revm
    bump, and no such transaction can exist for a spec with no activation history. The shared
    CALL-family wrapper still has the tripwire; that copy is spec-gated so REX7 cannot trip it. */

    /// Checkpoint form of [`wrap_op_detain_gas_unconditional`]: disabled guard, prologue, static
    /// gas ahead of the raw instruction (the position these opcodes' revm bodies charge from),
    /// body recording, detention cap, epilogue.
    ///
    /// A disable rejection charges the static entry first (REX7 charge-on-reject) and then reverts
    /// without running the prologue or the body. A passing guard is the baseline order: prologue,
    /// `gas_before`, then the charge, so the static fee is taken on the restored true counter.
    macro_rules! wrap_checkpoint_detain_gas_unconditional {
    ($fn_name:ident, $opcode:ident, $original_fn:path, $access_type:expr) => {
        #[doc = concat!("`", stringify!($opcode), "` opcode as a checkpoint: segment settlement, raw instruction, gas detention, re-clamp.")]
        #[inline]
        pub fn $fn_name<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
            context: InstructionContext<'_, H, WIRE>,
        ) -> InstructionExecResult {
            if context.host.volatile_access_disabled() {
                charge_static_gas!(context, $opcode);
                revert_volatile_access_disabled!(context, $opcode, $access_type);
            }
            checkpoint_prologue!(context);
            let gas_before = context.interpreter.gas.remaining();
            charge_static_gas!(context, $opcode);

            run_inner_instruction_or_abort!($original_fn, context, inner_outcome);
            record_checkpoint_body_compute_gas!(context, gas_before, detention_tail);
            apply_compute_gas_limit!(context);
            checkpoint_epilogue!(context);
            inner_outcome
        }
    };
    }

    /// Checkpoint form of [`wrap_op_detain_gas_conditional`]: beneficiary peek, prologue, raw
    /// instruction, static gas after it (the position these opcodes' revm bodies charge from, so an
    /// underfunded frame has already popped its operands and marked its access), body recording,
    /// detention cap, epilogue.
    ///
    /// A disable rejection charges the static entry first (REX7 charge-on-reject) and then reverts
    /// without running the body, so the success-path charge-after-load order — and the mark that
    /// load produces — is unchanged.
    macro_rules! wrap_checkpoint_detain_gas_conditional {
    ($fn_name:ident, $opcode:ident, $original_fn:path) => {
        #[doc = concat!("`", stringify!($opcode), "` opcode as a checkpoint: segment settlement, raw instruction, gas detention, re-clamp.")]
        #[inline]
        pub fn $fn_name<WIRE: InterpreterTypes<Stack: StackInspectTr>, H: HostExt + ?Sized>(
            context: InstructionContext<'_, H, WIRE>,
        ) -> InstructionExecResult {
            if let Some(addr_word) = context.interpreter.stack.inspect::<0>() {
                let target: Address = addr_word.into_address();
                let beneficiary = context.host.beneficiary_address();
                if target == beneficiary && context.host.volatile_access_disabled() {
                    charge_static_gas!(context, $opcode);
                    revert_volatile_access_disabled!(
                        context,
                        $opcode,
                        VolatileDataAccessType::Beneficiary
                    );
                }
            }
            checkpoint_prologue!(context);
            let gas_before = context.interpreter.gas.remaining();

            run_inner_instruction_or_abort!($original_fn, context, inner_outcome);
            charge_static_gas!(context, $opcode);
            record_checkpoint_body_compute_gas!(context, gas_before, detention_tail);
            apply_compute_gas_limit!(context);
            checkpoint_epilogue!(context);
            inner_outcome
        }
    };
    }

    wrap_checkpoint_detain_gas_unconditional!(
        timestamp_checkpoint,
        TIMESTAMP,
        instructions::block_info::timestamp,
        VolatileDataAccessType::Timestamp
    );
    wrap_checkpoint_detain_gas_unconditional!(
        block_number_checkpoint,
        NUMBER,
        instructions::block_info::block_number,
        VolatileDataAccessType::BlockNumber
    );
    wrap_checkpoint_detain_gas_unconditional!(
        difficulty_checkpoint,
        DIFFICULTY,
        instructions::block_info::difficulty,
        VolatileDataAccessType::Difficulty
    );
    wrap_checkpoint_detain_gas_unconditional!(
        gas_limit_opcode_checkpoint,
        GASLIMIT,
        instructions::block_info::gaslimit,
        VolatileDataAccessType::GasLimit
    );
    wrap_checkpoint_detain_gas_unconditional!(
        basefee_checkpoint,
        BASEFEE,
        instructions::block_info::basefee,
        VolatileDataAccessType::BaseFee
    );
    wrap_checkpoint_detain_gas_unconditional!(
        coinbase_checkpoint,
        COINBASE,
        instructions::block_info::coinbase,
        VolatileDataAccessType::Coinbase
    );
    wrap_checkpoint_detain_gas_unconditional!(
        blockhash_checkpoint,
        BLOCKHASH,
        instructions::host::blockhash,
        VolatileDataAccessType::BlockHash
    );
    wrap_checkpoint_detain_gas_unconditional!(
        blobbasefee_checkpoint,
        BLOBBASEFEE,
        instructions::block_info::blob_basefee,
        VolatileDataAccessType::BlobBaseFee
    );
    wrap_checkpoint_detain_gas_unconditional!(
        blobhash_checkpoint,
        BLOBHASH,
        instructions::tx_info::blob_hash,
        VolatileDataAccessType::BlobHash
    );

    wrap_checkpoint_detain_gas_conditional!(
        balance_checkpoint,
        BALANCE,
        instructions::host::balance
    );
    wrap_checkpoint_detain_gas_conditional!(
        extcodesize_checkpoint,
        EXTCODESIZE,
        instructions::host::extcodesize
    );
    wrap_checkpoint_detain_gas_conditional!(
        extcodecopy_checkpoint,
        EXTCODECOPY,
        instructions::host::extcodecopy
    );
    wrap_checkpoint_detain_gas_conditional!(
        extcodehash_checkpoint,
        EXTCODEHASH,
        instructions::host::extcodehash
    );

    /// `SLOAD` as a checkpoint. Same oracle-volatile handling as [`sload`], but the raw revm
    /// instruction runs unwrapped and the open segment settles in the prologue. A disable
    /// rejection charges the static entry first (REX7 charge-on-reject) without running the
    /// load, so the success-path charge-after-load order is unchanged.
    #[inline]
    pub fn sload_checkpoint<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        let target = context.interpreter.input.target_address();
        if target == ORACLE_CONTRACT_ADDRESS && context.host.volatile_access_disabled() {
            charge_static_gas!(context, SLOAD);
            revert_volatile_access_disabled!(context, SLOAD, VolatileDataAccessType::Oracle);
        }
        checkpoint_prologue!(context);
        let gas_before = context.interpreter.gas.remaining();

        run_inner_instruction_or_abort!(instructions::host::sload, context, inner_outcome);
        charge_static_gas!(context, SLOAD);
        record_checkpoint_body_compute_gas!(context, gas_before, detention_tail);
        apply_compute_gas_limit!(context);
        checkpoint_epilogue!(context);
        inner_outcome
    }

    /// `SELFBALANCE` as a checkpoint. Same beneficiary-volatile handling as [`selfbalance`], but
    /// the raw revm instruction runs unwrapped and the open segment settles in the prologue. A
    /// disable rejection charges the static entry first (REX7 charge-on-reject); a passing guard
    /// charges after the prologue, at the same position as the baseline handler.
    #[inline]
    pub fn selfbalance_checkpoint<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        let target = context.interpreter.input.target_address();
        let beneficiary = context.host.beneficiary_address();
        if target == beneficiary && context.host.volatile_access_disabled() {
            charge_static_gas!(context, SELFBALANCE);
            revert_volatile_access_disabled!(
                context,
                SELFBALANCE,
                VolatileDataAccessType::Beneficiary
            );
        }
        checkpoint_prologue!(context);
        let gas_before = context.interpreter.gas.remaining();
        charge_static_gas!(context, SELFBALANCE);

        run_inner_instruction_or_abort!(instructions::host::selfbalance, context, inner_outcome);
        record_checkpoint_body_compute_gas!(context, gas_before, detention_tail);
        apply_compute_gas_limit!(context);
        checkpoint_epilogue!(context);
        inner_outcome
    }
}

/// Extends opcodes with additional limit (kv update limit, data limit, etc.) enforcement.
pub mod additional_limit_ext {
    use super::*;

    /// `SSTORE` opcode implementation with data size and KV update limit enforcement.
    ///
    /// This wrapper adds limit tracking on top of [`storage_gas_ext::sstore`], which handles
    /// compute gas tracking and storage gas costs.
    ///
    /// # Data Size and KV Update Tracking
    ///
    /// When first writing non-zero value to originally-zero slot:
    /// - Adds 40 bytes to transaction data size
    /// - Adds 1 KV update count
    ///
    /// # Limit Enforcement
    ///
    /// Halts with `OutOfGas` when data (3.125 MB) or KV (1,000) limits exceeded.
    ///
    /// # Refund Logic
    ///
    /// Refunds data/KV when slot reset to original value.
    pub fn sstore<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        // Load storage slot values before executing the instruction
        let target_address = context.interpreter.input.target_address();
        let Some(index) = context.interpreter.stack.inspect::<0>() else {
            return Err(InstructionResult::StackUnderflow);
        };
        let mega_spec = context.host.spec_id();
        let Ok(slot) = context.host.inspect_storage(mega_spec, target_address, index) else {
            return Err(InstructionResult::FatalExternalError);
        };
        let (original_value, present_value) = (slot.original_value(), slot.present_value());
        let Some(new_value) = context.interpreter.stack.inspect::<1>() else {
            return Err(InstructionResult::StackUnderflow);
        };
        let loaded_data = SStoreResult { original_value, present_value, new_value };

        // Execute the original SSTORE instruction
        run_inner_instruction_or_abort!(storage_gas_ext::sstore, context, inner_outcome);

        // KV update bomb and data bomb (only when first writing non-zero value to originally zero
        // slot): check if the number of key-value updates or the total data size will exceed the
        // limit, if so, halt.
        let additional_limit = context.host.additional_limit();
        let mut additional_limit = additional_limit.borrow_mut();
        if !additional_limit.on_sstore(target_address, index, &loaded_data) {
            let result = additional_limit.exceeding_instruction_result();
            set_halt_action!(context.interpreter, result);
            return Err(result);
        }
        drop(additional_limit);
        // REX7: re-clamp once every dimension this opcode touches has been recorded.
        checkpoint_epilogue!(context);
        inner_outcome
    }

    /// `LOG` opcode implementation with data size limit enforcement.
    ///
    /// This wrapper adds data limit tracking on top of [`storage_gas_ext::log`], which handles
    /// compute gas tracking and storage gas costs.
    ///
    /// # Data Size Limit Enforcement
    ///
    /// After log emission, checks if total transaction data size exceeds `TX_DATA_LIMIT` (3.125
    /// MB). Halts when data limit exceeded.
    pub fn log<
        const N: usize,
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        // Get the log data length before executing the instruction
        let Some(len) = context.interpreter.stack.inspect::<1>() else {
            return Err(InstructionResult::StackUnderflow);
        };
        let len = as_usize_or_fail!(context.interpreter, len);

        // Execute the original LOG instruction
        run_inner_instruction_or_abort!(storage_gas_ext::log::<N, WIRE, H>, context, inner_outcome);

        // Record the size of the log topics and data. If the total data size exceeds the limit, we
        // halt.
        let additional_limit = context.host.additional_limit();
        let mut additional_limit = additional_limit.borrow_mut();
        if !additional_limit.on_log(N as u64, len as u64) {
            let result = additional_limit.exceeding_instruction_result();
            set_halt_action!(context.interpreter, result);
            return Err(result);
        }
        drop(additional_limit);
        // REX7: re-clamp once every dimension this opcode touches has been recorded.
        checkpoint_epilogue!(context);
        inner_outcome
    }
}

/// Extends opcodes with storage gas cost on top of `compute_gas_ext`.
pub mod storage_gas_ext {
    use super::*;
    use alloy_primitives::Address;

    /// Address-selector for opcodes where the storage account is the stack `to` address (e.g.
    /// CALL).
    fn storage_addr_from_to(_mega_spec: MegaSpecId, _current: Address, to: Address) -> Address {
        to
    }

    /// Selects the create opcode that `IS_CREATE2` stands for.
    ///
    /// The `CREATE`/`CREATE2` handlers are one function generic over `IS_CREATE2`, so the opcode
    /// cannot be spelled as a literal macro argument the way the other wrappers do it.
    pub(super) const fn create_opcode(is_create2: bool) -> u8 {
        if is_create2 {
            opcode::CREATE2
        } else {
            opcode::CREATE
        }
    }

    /// Address-selector for CALLCODE: Rex5+ uses the current frame's address because CALLCODE
    /// executes borrowed code in the caller's own storage context; pre-Rex5 preserves the frozen
    /// behavior of metering against the code-source (stack `to`).
    fn storage_addr_for_callcode(mega_spec: MegaSpecId, current: Address, to: Address) -> Address {
        if mega_spec.is_enabled(MegaSpecId::REX5) {
            current
        } else {
            to
        }
    }

    /// Macro to charge storage gas for new account creation before calling the wrapped instruction.
    ///
    /// This macro generates a wrapper function that:
    /// 1. Inspects the target address (stack position 1) and value (stack position 2)
    /// 2. Resolves the storage account address via `$select_addr`
    /// 3. Checks if the storage account is empty and value transfer is non-zero
    /// 4. Charges storage gas for new account creation if applicable
    /// 5. Calls the wrapped instruction implementation
    ///
    /// # Call Opcode Behavior
    ///
    /// The generated `CALL` and `CALLCODE` implementations add:
    ///
    /// **Dynamic New Account Gas**: When calling empty account with value transfer:
    /// - Base cost 2,000,000 gas, multiplied by `bucket_capacity / MIN_BUCKET_SIZE`
    ///
    /// # Parameters
    /// - `$fn_name`: Name of the generated function
    /// - `$opcode`: The wrapped opcode, used for documentation and for the static-gas add-back
    /// - `$raw_fn`: Path to the raw inner opcode implementation (no compute-gas wrapper)
    /// - `$has_transfer_logic`: `true` if the opcode can transfer value (inspects stack position 2)
    /// - `$select_addr` (optional): Path to a `fn(MegaSpecId, current: Address, to: Address) ->
    ///   Address` function that returns the address to check for emptiness and charge
    ///   `new_account_storage_gas` against. `current` is the current frame's address; `to` is the
    ///   stack position-1 address. Defaults to [`storage_addr_from_to`].
    ///
    /// # Metering order
    ///
    /// Runs `$raw_fn` directly and records compute gas exactly once after the body completes via
    /// [`record_storage_compute_gas!`], excluding the storage gas charged above. Nothing consumes
    /// EVM gas between the `gas_before` capture and the storage-gas charge (only stack inspects,
    /// host account reads, and additional-limit operations), so this single-window form records
    /// the same compute gas as the pre-REX6 "wrap the inner with `compute_gas_ext`" layering on
    /// every spec — the recorded amount is `body_gas` either way.
    macro_rules! wrap_call_with_storage_gas {
        ($fn_name:ident, $opcode:ident, $raw_fn:path, $has_transfer_logic:expr) => {
            wrap_call_with_storage_gas!(
                $fn_name,
                $opcode,
                $raw_fn,
                $has_transfer_logic,
                storage_addr_from_to
            );
        };
        ($fn_name:ident, $opcode:ident, $raw_fn:path, $has_transfer_logic:expr, $select_addr:path) => {
            #[doc = concat!("`", stringify!($opcode), "` opcode implementation modified from `revm` with compute gas tracking and dynamically-scaled storage gas costs.")]
            pub fn $fn_name<
                WIRE: InterpreterTypes<Stack: StackInspectTr>,
                H: HostExt + ContextTr + JournalInspectTr + ?Sized,
            >(
                context: InstructionContext<'_, H, WIRE>,
            ) -> InstructionExecResult {
                // REX7: settle the open segment and restore the clamp before any gas observation,
                // so the storage charge and the body's 63/64 forwarding math see the true counter.
                checkpoint_prologue!(context);
                // Captured at the very top so the single compute window covers all of the
                // opcode's compute work.
                let gas_before = context.interpreter.gas.remaining();
                let spec = context.interpreter.runtime_flag.spec_id();
                let Some(to) = context.interpreter.stack.inspect::<1>() else {
                    return Err(InstructionResult::StackUnderflow);
                };
                let to = to.into_address();
                let mega_spec = context.host.spec_id();
                let current_address = context.interpreter.input.target_address();
                let storage_address = $select_addr(mega_spec, current_address, to);
                let Ok(storage_account) = (if mega_spec.is_enabled(MegaSpecId::REX5) {
                    context.host.inspect_account(storage_address, false)
                } else {
                    context.host.inspect_account_delegated(mega_spec, storage_address)
                }) else {
                    return Err(InstructionResult::FatalExternalError);
                };
                let is_empty = storage_account.state_clear_aware_is_empty(spec);
                let has_transfer = if $has_transfer_logic {
                    let Some(value) = context.interpreter.stack.inspect::<2>() else {
                        return Err(InstructionResult::StackUnderflow);
                    };
                    !value.is_zero()
                } else {
                    false
                };
                // Charge additional storage gas cost for creating a new account.
                // REX5 drains the storage stipend allowance first; pre-REX5 returns 0.
                // `storage_charged` is the EVM gas actually debited for storage gas, so the REX6
                // single compute recording below can exclude it from the measured window.
                let storage_charged = if is_empty && has_transfer {
                    let Some(new_account_storage_gas) =
                        context.host.new_account_storage_gas(storage_address)
                    else {
                        return Err(InstructionResult::FatalExternalError);
                    };
                    let drained = context
                        .host
                        .additional_limit()
                        .borrow_mut()
                        .try_consume_storage_stipend(new_account_storage_gas);
                    charge_storage_gas!(context, new_account_storage_gas - drained)
                } else {
                    0
                };

                // Run the raw opcode and record compute gas once after the body completes
                // (canonical metering order). Byte-equivalent to the pre-REX6 layering on every
                // spec because nothing between the `gas_before` capture and the storage charge
                // above consumes EVM gas.
                run_inner_instruction_or_abort!($raw_fn, context, inner_outcome);
                record_storage_compute_gas!(
                    context,
                    gas_before,
                    storage_charged,
                    opcode::$opcode
                );
                inner_outcome
            }
        };
    }

    wrap_call_with_storage_gas!(call, CALL, call_resolving_target::<OP_CALL, _, _>, true);
    wrap_call_with_storage_gas!(
        delegate_call,
        DELEGATECALL,
        call_resolving_target::<OP_DELEGATECALL, _, _>,
        false
    );
    wrap_call_with_storage_gas!(
        static_call,
        STATICCALL,
        call_resolving_target::<OP_STATICCALL, _, _>,
        false
    );
    wrap_call_with_storage_gas!(
        call_code,
        CALLCODE,
        call_resolving_target::<OP_CALLCODE, _, _>,
        true,
        storage_addr_for_callcode
    );

    /// Inspects the creator account and computes the address a `CREATE`/`CREATE2` would deploy to.
    ///
    /// Shared by the pre-REX6 [`create`] and the REX6 [`create_rex6`] handlers so the address
    /// computation lives in one place. For CREATE2 this expands memory to hash the initcode, which
    /// debits EVM gas:
    ///
    /// - When `record_resize_eagerly` is `true` (pre-REX6 with REX5 enabled), the memory-expansion
    ///   gas is recorded into the compute-gas tracker immediately — the same position and timing as
    ///   the original inline code — and the returned `resize_gas` is `0`.
    /// - When `record_resize_eagerly` is `false`, the gas is left unrecorded and returned, so the
    ///   caller decides when to record it: the pre-REX5 late-record path, or the REX6 single-window
    ///   folding in [`record_storage_compute_gas!`].
    ///
    /// Returns `None` if a precondition failed (stack underflow, oversized operand, a REX6
    /// oversized-initcode halt, memory OOG, external DB error, or an eager compute-gas-limit
    /// exceed) and the interpreter was halted.
    fn compute_created_address<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        const IS_CREATE2: bool,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >(
        context: &mut InstructionContext<'_, H, WIRE>,
        spec: MegaSpecId,
        record_resize_eagerly: bool,
    ) -> Option<(Address, u64)> {
        // The current execution contract (the caller). Load it without marking it warm (it is
        // already warm since the current frame began). REX5+ uses non-delegating inspection to get
        // the authority's own state.
        let creator_address = context.interpreter.input.target_address();
        let Ok(creator) = (if spec.is_enabled(MegaSpecId::REX5) {
            context.host.inspect_account(creator_address, false)
        } else {
            context.host.inspect_account_delegated(spec, creator_address)
        }) else {
            context.interpreter.halt(InstructionResult::FatalExternalError);
            return None;
        };

        let mut resize_gas: u64 = 0;
        let is_rex5_enabled = spec.is_enabled(MegaSpecId::REX5);
        let created_address = if IS_CREATE2 {
            let Some(initcode_offset) = context.interpreter.stack.inspect::<1>() else {
                context.interpreter.halt(InstructionResult::StackUnderflow);
                return None;
            };
            let Some(initcode_len) = context.interpreter.stack.inspect::<2>() else {
                context.interpreter.halt(InstructionResult::StackUnderflow);
                return None;
            };
            // REX5+: validate the salt operand before running `resize_memory!` and the copy /
            // keccak block, so a missing salt halts with `StackUnderflow` without
            // performing the expensive memory work. Pre-REX5 keeps the original "resize
            // first, salt last" order.
            let rex5_salt = if is_rex5_enabled {
                let Some(salt) = context.interpreter.stack.inspect::<3>() else {
                    context.interpreter.halt(InstructionResult::StackUnderflow);
                    return None;
                };
                Some(salt)
            } else {
                None
            };

            // REX5+: when `initcode_len == 0`, mirror canonical revm CREATE2 — ignore the offset
            // entirely (no conversion, no memory expansion, no slice, no keccak) and use
            // `KECCAK_EMPTY`. Pre-REX5 keeps the "observe offset, resize, slice, hash" sequence.
            let initcode_hash = if is_rex5_enabled && initcode_len.is_zero() {
                KECCAK_EMPTY
            } else {
                // Convert `initcode_len` before `initcode_offset` (matching canonical revm's
                // `create`, which pops/converts `len` first): both operands halt with the same
                // `InstructionResult::InvalidOperandOOG` reason when they don't fit in a
                // `usize` (see `as_usize_or_fail_ret!`'s default reason), so this reordering
                // does not change pre-REX6 behavior — but it matters for the REX6 size check
                // below, which must run before `initcode_offset` is ever touched so that an
                // oversized `initcode_len` halts with `CreateInitCodeSizeLimit` even when
                // `initcode_offset` does not fit in a `usize` either.
                let initcode_len = as_usize_or_fail_ret!(context.interpreter, initcode_len, None);

                // REX6: EIP-3860 initcode-size halt, matching revm's canonical
                // ordering intent (`revm::interpreter::instructions::contract::create`) — halt
                // BEFORE `initcode_offset` conversion, `resize_memory!`/copy/keccak256/address-
                // derivation, not after. Pre-REX6 performs that prework before the halt
                // eventually fires inside the inner opcode call below; that ordering is
                // non-consensus under REX6 when the halt fires before the inner opcode
                // completes (committed gas/state is identical either way —
                // `record_resize_eagerly=false` means the resize gas is not yet recorded at
                // this point, so skipping it here charges nothing that a later, slower path
                // would have charged — only the halt timing / node CPU differ), but changing
                // pre-REX6 timing would perturb sealed-spec replay, so this is gated to REX6
                // only. A REX6 static frame never reaches this check: [`create_rex6`] rejects
                // static frames before entering this helper.
                if spec.is_enabled(MegaSpecId::REX6) &&
                    initcode_len > context.host.max_initcode_size()
                {
                    context.interpreter.halt(InstructionResult::CreateInitCodeSizeLimit);
                    return None;
                }

                let initcode_offset =
                    as_usize_or_fail_ret!(context.interpreter, initcode_offset, None);

                // Expand memory before slicing so the read can never go out of bounds. The inner
                // CREATE2 also calls `resize_memory!`, which is a no-op once memory already fits.
                let gas_before_resize = context.interpreter.gas.remaining();
                resize_memory_ret!(context, initcode_offset, initcode_len, None);
                resize_gas = gas_before_resize.saturating_sub(context.interpreter.gas.remaining());

                // Eager recording (pre-REX6 / REX5): record the expansion gas immediately to align
                // its timing with revm's EVM-gas debit, then zero `resize_gas` so the caller's
                // late-record path does not double-count.
                if record_resize_eagerly && resize_gas > 0 {
                    let mut additional_limit = context.host.additional_limit().borrow_mut();
                    compute_gas!(context.interpreter, additional_limit, resize_gas, None);
                    resize_gas = 0;
                }

                let code = Bytes::copy_from_slice(
                    context.interpreter.memory.slice_len(initcode_offset, initcode_len).as_ref(),
                );
                keccak256(&code)
            };

            let salt = if let Some(s) = rex5_salt {
                s
            } else {
                let Some(salt) = context.interpreter.stack.inspect::<3>() else {
                    context.interpreter.halt(InstructionResult::StackUnderflow);
                    return None;
                };
                salt
            };

            creator_address.create2(salt.to_be_bytes(), initcode_hash)
        } else {
            creator_address.create(creator.info.nonce)
        };

        Some((created_address, resize_gas))
    }

    /// `CREATE`/`CREATE2` opcode implementation modified from `revm` with compute gas tracking and
    /// dynamically-scaled storage gas costs.
    ///
    /// # Differences from the standard EVM
    ///
    /// 1. **Dynamic New Account Gas**: Additional storage gas for new account creation:
    ///    - Base cost 2,000,000 gas, multiplied by `bucket_capacity / MIN_BUCKET_SIZE`
    ///
    /// # Assumptions
    ///
    /// This is the entry point for `CREATE`/`CREATE2` from `MINI_REX` onward. REX6+ short-
    /// circuits to [`create_rex6`] at the top; the body below is the pre-REX6 path, which can
    /// assume all features up to and including `MINI_REX` are enabled.
    pub fn create<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        const IS_CREATE2: bool,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >(
        mut context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        let spec = context.host.spec_id();

        // REX6+: canonical metering order — fold the CREATE2 memory-expansion gas into a single
        // compute-gas recording taken after the body completes (see `create_rex6`), instead of
        // the pre-REX6 split `resize_gas` recording handled below.
        if spec.is_enabled(MegaSpecId::REX6) {
            return create_rex6::<WIRE, IS_CREATE2, H>(context);
        }

        // Inspect the creator and compute the created address. REX5+ records the CREATE2
        // memory-expansion gas eagerly inside the helper (the same position and timing as the
        // original inline code); pre-REX5 leaves it in `resize_gas` for the late-record path below.
        let Some((created_address, resize_gas)) = compute_created_address::<WIRE, IS_CREATE2, H>(
            &mut context,
            spec,
            spec.is_enabled(MegaSpecId::REX5),
        ) else {
            // `compute_created_address` halted the interpreter; surface the recorded result so
            // the interpreter loop stops instead of stepping past the halt.
            return Err(context
                .interpreter
                .bytecode
                .instruction_result()
                .unwrap_or(InstructionResult::FatalExternalError));
        };

        // Charge storage gas cost for creating a new contract
        let create_contract_storage_gas = if spec.is_enabled(MegaSpecId::REX) {
            // Rex spec distinguishes between contract creation and account creation.
            context.host.create_contract_storage_gas(created_address)
        } else {
            // Mini-Rex spec does not distinguish between contract creation and account creation.
            context.host.new_account_storage_gas(created_address)
        };
        let Some(create_contract_storage_gas) = create_contract_storage_gas else {
            return Err(InstructionResult::FatalExternalError);
        };
        // REX5 drains the storage stipend allowance first; pre-REX5 returns 0.
        let drained = context
            .host
            .additional_limit()
            .borrow_mut()
            .try_consume_storage_stipend(create_contract_storage_gas);
        gas!(context.interpreter, create_contract_storage_gas - drained);

        // Capture, run raw, record — `gas_before` here is captured after the storage debit and
        // after `compute_created_address`'s eager `resize_gas` record (REX5+), so the recorded
        // amount equals the inner opcode's body gas. Byte-equivalent to the old per-`IS_CREATE2`
        // dispatch through `compute_gas_ext::{create, create2}`, which captured at the same
        // point.
        let gas_before = context.interpreter.gas.remaining();
        run_inner_instruction_or_abort!(
            instructions::contract::create::<IS_CREATE2, _, _>,
            context,
            inner_outcome
        );
        record_storage_compute_gas!(context, gas_before, 0, create_opcode(IS_CREATE2));

        // Pre-REX5 late-record path for the CREATE2 initcode memory-expansion gas.
        // Preserved verbatim for replay parity: pre-REX5 keeps the original "skip on inner
        // error" semantics where storage-gas OOG and inner-CREATE2 failure both skip this
        // recording. REX5+ already recorded `resize_gas` above (and zeroed it), so this
        // branch is a no-op under REX5.
        if resize_gas > 0 {
            let mut additional_limit = context.host.additional_limit().borrow_mut();
            compute_gas!(context.interpreter, additional_limit, resize_gas);
        }
        inner_outcome
    }

    /// `CREATE`/`CREATE2` under the REX6+ canonical metering order.
    ///
    /// Records the opcode's compute gas exactly once, after the inner opcode completes, via
    /// [`record_storage_compute_gas!`]. This folds the CREATE2 memory-expansion (`resize_memory!`)
    /// gas into the single compute window instead of the pre-REX6 split recording (REX5 recorded it
    /// eagerly before the storage charge; pre-REX5 recorded it after the inner op). The storage gas
    /// charged for contract creation is excluded from the recorded compute gas.
    ///
    /// On the straight-line success path the total compute gas equals the pre-REX6 amount. The two
    /// differ only when a compute-limit or storage-gas-OOG halt occurs between the memory expansion
    /// and inner-op completion: REX6 records compute gas only once the body has fully executed, so
    /// a partial memory expansion that never reaches the inner opcode is not recorded against
    /// the compute-gas limit (its EVM gas is still debited).
    ///
    /// REX6 implies REX5 (and REX), so the REX5 operand validation and the contract-creation
    /// storage-gas path are taken unconditionally here.
    fn create_rex6<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        const IS_CREATE2: bool,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >(
        mut context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        // Canonical revm's `create` runs `require_non_staticcall!` before any operand read,
        // memory work, address derivation, or storage-gas charge, so a static-frame
        // `CREATE`/`CREATE2` halts here first. This unifies the halt reasons the prework below
        // would otherwise produce in a static frame — stack underflow from operand inspection,
        // memory OOG from an unaffordable resize, storage-gas OOG from the creation charge, or
        // a fatal external error from the storage-pricing lookup — into the canonical
        // `StateChangeDuringStaticCall`. No spec gate: [`create`] dispatches here exactly when
        // REX6 is enabled. Pre-REX6 keeps the prework-first order (changing sealed-spec halt
        // reasons would perturb replay). Every reachable path is an all-gas-consuming halt, so
        // committed gas and state are identical either way.
        if context.interpreter.runtime_flag.is_static() {
            return Err(InstructionResult::StateChangeDuringStaticCall);
        }

        // REX7: settle the open segment and restore the clamp before any gas observation, so the
        // memory expansion, the storage charge and the body's forwarding math see the true counter.
        checkpoint_prologue!(context);

        // Captured before any gas movement so the single compute window covers the wrapper-side
        // CREATE2 memory expansion as well as the inner opcode.
        let gas_before = context.interpreter.gas.remaining();
        let spec = context.host.spec_id();

        // Inspect the creator and compute the created address. `record_resize_eagerly = false`:
        // the CREATE2 memory-expansion gas is left unrecorded so it folds into the single compute
        // window closed by `record_storage_compute_gas!` below.
        let Some((created_address, _resize_gas)) =
            compute_created_address::<WIRE, IS_CREATE2, H>(&mut context, spec, false)
        else {
            return Err(context
                .interpreter
                .bytecode
                .instruction_result()
                .unwrap_or(InstructionResult::FatalExternalError));
        };

        // Charge storage gas cost for creating a new contract. REX6 implies REX, so the
        // contract-creation cost path applies. `storage_charged` is excluded from the compute
        // recording below.
        let Some(create_contract_storage_gas) =
            context.host.create_contract_storage_gas(created_address)
        else {
            return Err(InstructionResult::FatalExternalError);
        };
        // REX6 implies REX5: drain the storage stipend allowance first.
        let drained = context
            .host
            .additional_limit()
            .borrow_mut()
            .try_consume_storage_stipend(create_contract_storage_gas);
        let storage_charged = charge_storage_gas!(context, create_contract_storage_gas - drained);

        // Run the raw inner create opcode (no `compute_gas_ext` wrapper — REX6 records compute gas
        // once below).
        let inner_outcome = if IS_CREATE2 {
            run_inner_instruction_or_abort!(
                instructions::contract::create::<true, _, _>,
                context,
                outcome
            );
            outcome
        } else {
            run_inner_instruction_or_abort!(
                instructions::contract::create::<false, _, _>,
                context,
                outcome
            );
            outcome
        };

        record_storage_compute_gas!(
            context,
            gas_before,
            storage_charged,
            create_opcode(IS_CREATE2)
        );
        inner_outcome
    }

    /// `LOG` opcode implementation modified from `revm` with compute gas tracking, increased
    /// storage gas costs, and data size limit enforcement.
    ///
    /// # Differences from the standard EVM
    ///
    /// 1. **Storage Gas Costs**: Additional storage gas charged for log storage:
    ///    - Topic storage: 3,750 gas per topic (10x standard topic cost)
    ///    - Data storage: 80 gas per byte (10x standard data cost)
    ///
    /// # Assumptions
    ///
    /// This alternative implementation of `LOG` is only used when the `MINI_REX` spec is enabled.
    pub fn log<
        const N: usize,
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        // REX7: settle the open segment and restore the clamp before any gas observation.
        checkpoint_prologue!(context);
        // Captured at the very top so the single compute window covers the inner opcode.
        let gas_before = context.interpreter.gas.remaining();
        let Some(len) = context.interpreter.stack.inspect::<1>() else {
            return Err(InstructionResult::StackUnderflow);
        };
        let len = as_usize_or_fail!(context.interpreter, len);

        // Charge storage gas cost for log topics and data before instruction execution.
        // REX5 drains the allowance on the `Some(amount)` arm; the `None` (overflow) arm
        // is passed through unchanged to preserve the OOG halt.
        let log_storage_cost = {
            let topic_cost = constants::mini_rex::LOG_TOPIC_STORAGE_GAS.checked_mul(N as u64);
            let data_cost = constants::mini_rex::LOG_DATA_STORAGE_GAS.checked_mul(len as u64);
            topic_cost.and_then(|topic| data_cost.and_then(|cost| cost.checked_add(topic)))
        };
        let log_storage_cost = log_storage_cost.map(|amount| {
            let drained =
                context.host.additional_limit().borrow_mut().try_consume_storage_stipend(amount);
            amount - drained
        });
        gas_or_fail!(context.interpreter, log_storage_cost);
        // `gas_or_fail!` halts and returns on the `None` (overflow) arm, so reaching here means the
        // cost was `Some`; this is the storage gas actually charged, excluded from the compute
        // recording below. Assert the invariant with `expect` rather than `unwrap_or(0)`: a silent
        // `0` here would make `record_storage_compute_gas!` over-count compute gas by the full LOG
        // storage cost.
        let storage_charged =
            log_storage_cost.expect("gas_or_fail! above halts and returns on None");
        // The `gas_or_fail!` above is the storage-gas charge, so it gets the same segment
        // exclusion `charge_storage_gas!` applies at every other charge site: the raw opcode below
        // can halt (a static frame rejects `LOG` outright) before the recording that would
        // otherwise subtract it.
        context
            .host
            .additional_limit()
            .borrow_mut()
            .exclude_storage_gas_from_segment(storage_charged);

        // Run the raw opcode and record compute gas once after the body completes (canonical
        // metering order). Byte-equivalent to the pre-REX6 per-`N` `compute_gas_ext::logK`
        // dispatch on every spec because nothing between `gas_before` and the `gas_or_fail!` above
        // consumes EVM gas. The wrapper is only ever instantiated for `N` in `0..=4`, so the
        // generic `instructions::host::log::<N, _>` covers every valid call site.
        run_inner_instruction_or_abort!(instructions::host::log::<N, _>, context, inner_outcome);
        record_storage_compute_gas!(context, gas_before, storage_charged, opcode::LOG0 + N as u8);
        inner_outcome
    }

    /// `SSTORE` opcode implementation modified from `revm` with compute gas tracking and
    /// dynamically-scaled storage gas costs.
    ///
    /// # Differences from the standard EVM
    ///
    /// 1. **Dynamic Storage Gas**: Additional storage gas ONLY when setting originally-zero slot to
    ///    non-zero:
    ///    - Base cost 2,000,000 gas, multiplied by `bucket_capacity / MIN_BUCKET_SIZE`
    ///    - Not charged for updating already-non-zero slots or resetting to zero
    ///
    /// # Assumptions
    ///
    /// This alternative implementation of `SSTORE` is only used when the `MINI_REX` spec is
    /// enabled, so we can safely assume that all features before and including Mini-Rex are
    /// enabled.
    pub fn sstore<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        // REX7: settle the open segment and restore the clamp before any gas observation.
        checkpoint_prologue!(context);
        // Captured at the very top so the single compute window covers the inner opcode.
        let gas_before = context.interpreter.gas.remaining();
        // The address to the underlying execution contract state
        let target_address = context.interpreter.input.target_address();
        // The storage slot to write
        let Some(index) = context.interpreter.stack.inspect::<0>() else {
            return Err(InstructionResult::StackUnderflow);
        };
        // The storage slot values
        let mega_spec = context.host.spec_id();
        let Ok(slot) = context.host.inspect_storage(mega_spec, target_address, index) else {
            return Err(InstructionResult::FatalExternalError);
        };
        let (original_value, present_value) = (slot.original_value(), slot.present_value());
        let Some(new_value) = context.interpreter.stack.inspect::<1>() else {
            return Err(InstructionResult::StackUnderflow);
        };

        // Charge storage gas cost before the instruction is executed.
        // REX5 drains the storage stipend allowance first; pre-REX5 returns 0.
        // `storage_charged` is the EVM gas actually debited for storage gas, excluded from the
        // single compute recording below.
        let storage_charged =
            if original_value.is_zero() && present_value.is_zero() && !new_value.is_zero() {
                let Some(sstore_set_storage_gas) =
                    context.host.sstore_set_storage_gas(target_address, index)
                else {
                    return Err(InstructionResult::FatalExternalError);
                };
                let drained = context
                    .host
                    .additional_limit()
                    .borrow_mut()
                    .try_consume_storage_stipend(sstore_set_storage_gas);
                charge_storage_gas!(context, sstore_set_storage_gas - drained)
            } else {
                0
            };

        // Run the raw opcode and record compute gas once after the body completes (canonical
        // metering order). Byte-equivalent to the pre-REX6 `compute_gas_ext::sstore` layering on
        // every spec because nothing between `gas_before` and the storage charge above consumes
        // EVM gas.
        run_inner_instruction_or_abort!(instructions::host::sstore, context, inner_outcome);
        record_storage_compute_gas!(context, gas_before, storage_charged, opcode::SSTORE);
        inner_outcome
    }

    /// `SELFDESTRUCT` opcode implementation with storage gas metering for
    /// new beneficiary account creation (REX5+).
    ///
    /// When SELFDESTRUCT sends remaining balance to an empty beneficiary, charges:
    /// - Storage gas for new account creation (dynamic bucket-based cost)
    /// - Data size (+40 for account info write)
    /// - KV update (+1)
    /// - State growth (+1)
    ///
    /// This wrapper sits between `volatile_data_ext` and `compute_gas_ext` in the
    /// REX5 SELFDESTRUCT dispatch chain
    /// (`volatile_data_ext::selfdestruct_with_beneficiary_guard` → `storage_gas_ext::selfdestruct`
    /// → `compute_gas_ext::selfdestruct_self_charged`), matching the layering used by SSTORE
    /// and LOG. The beneficiary-volatile guard runs in the outer
    /// `volatile_data_ext::selfdestruct_with_beneficiary_guard` ahead of any side effects below.
    ///
    /// REX6 additionally records the `DataSize` +40 / KV +1 of a balance credit to an existing
    /// *distinct* beneficiary — the account-info write the frame-init / `target_updated` path never
    /// sees — via the REX6-gated arm below; pre-REX6 records nothing for an existing target. The
    /// rest of the body, and all ≤REX5 behavior, is unchanged.
    pub fn selfdestruct<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr + JournalInspectTr + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        // REX7: settle the open segment and restore the clamp before any gas observation — the
        // beneficiary-creation storage charge below and the inner opcode both run on the true
        // counter, which is what keeps the storage charge outside every compute window.
        checkpoint_prologue!(context);

        // Inside a static frame, revm's inner SELFDESTRUCT halts on the
        // static-context check without changing state. Skip the mega host work below
        // (two account inspections, SALT account-creation pricing, the storage-gas
        // stipend draw and tracker write) — the frame reverts and discards all of it
        // anyway — and let the inner instruction produce the identical halt. This is
        // behavior-neutral: the static halt is exceptional, so the frame's gas and
        // tracked usage are the same whether or not the host work ran first. The table
        // installs this wrapper only for REX5+, so pre-REX5 specs never reach here.
        if context.interpreter.runtime_flag.is_static() {
            run_inner_instruction_or_abort!(
                compute_gas_ext::selfdestruct_self_charged,
                context,
                inner_outcome
            );
            // Defensive: unreachable in practice — a static SELFDESTRUCT always halts
            // inside the inner instruction, so the macro returns early above.
            return inner_outcome;
        }

        let eth_spec = context.interpreter.runtime_flag.spec_id();

        // Peek beneficiary address from stack (SELFDESTRUCT uses stack position 0)
        let Some(target) = context.interpreter.stack.inspect::<0>() else {
            return Err(InstructionResult::StackUnderflow);
        };
        let target = target.into_address();

        // Use non-delegating inspection (REX5+)
        let Ok(target_account) = context.host.inspect_account(target, false) else {
            return Err(InstructionResult::FatalExternalError);
        };
        let is_empty = target_account.state_clear_aware_is_empty(eth_spec);

        // Check if caller has balance (value will be transferred to beneficiary)
        let caller = context.interpreter.input.target_address();
        let Ok(caller_account) = context.host.inspect_account(caller, false) else {
            return Err(InstructionResult::FatalExternalError);
        };
        let has_value = !caller_account.info.balance.is_zero();

        if is_empty && has_value {
            // Charge storage gas for creating a new account.
            // REX5 drains the storage stipend allowance first; pre-REX5 returns 0.
            let Some(cost) = context.host.new_account_storage_gas(target) else {
                return Err(InstructionResult::FatalExternalError);
            };
            let drained =
                context.host.additional_limit().borrow_mut().try_consume_storage_stipend(cost);
            charge_storage_gas!(context, cost - drained);

            // Record resource usage for new beneficiary account
            context.host.additional_limit().borrow_mut().on_selfdestruct_new_account();
        } else if context.host.spec_id().is_enabled(MegaSpecId::REX6) &&
            has_value &&
            caller != target
        {
            // REX6: a balance credit to an existing *distinct* beneficiary performs an account-info
            // write the frame-init / `target_updated` path never sees — record DataSize +40 / KV +1
            // (no `StateGrowth`, the account already exists; no storage gas, the bucket is paid).
            // SELFDESTRUCT to self (`caller == target`) is an EIP-6780 balance no-op on a
            // non-same-tx-created account (and a burn-to-self on a same-tx-created one) — neither
            // is a distinct-target credit, so record nothing. Pre-REX6 records nothing
            // for any existing target.
            context.host.additional_limit().borrow_mut().on_selfdestruct_existing_account();
        }

        // Delegate to compute_gas_ext::selfdestruct_self_charged (the volatile-disabled guard
        // ran in the outer `volatile_data_ext::selfdestruct_with_beneficiary_guard` wrapper).
        run_inner_instruction_or_abort!(
            compute_gas_ext::selfdestruct_self_charged,
            context,
            inner_outcome
        );
        inner_outcome
    }
}

/// Compute gas recording implementation. TODO: add more doc
pub mod compute_gas_ext {
    use super::*;

    /// Macro to wrap the original instruction implementation with compute gas tracking.
    ///
    /// `$opcode` names the wrapped opcode; its [`STATIC_GAS_TABLE`] entry is added back into the
    /// measured window because the interpreter charged that portion before entering the handler.
    ///
    /// Three variants:
    /// - default: "simple" opcodes that can never spawn a child frame. The compute gas used is
    ///   simply `static_gas + (gas_before - gas_after)`. These opcodes never set an
    ///   `InterpreterAction::NewFrame`, so the child-gas-subtraction match (below) would always
    ///   fall through to `_` — it is omitted entirely to keep the per-opcode hot path lean.
    /// - `@frame`: the call/create family (`CALL`/`CALLCODE`/`DELEGATECALL`/`STATICCALL`/
    ///   `CREATE`/`CREATE2`), the only opcodes that set `NewFrame`. These must subtract the gas
    ///   forwarded to the child frame so the parent's compute gas is not over-counted.
    /// - `@self_charged`: the account-reading opcodes whose spec zeroes their static gas entry, so
    ///   that their volatile guard is reached even by a frame too poor to pay it. Nobody has
    ///   charged the entry when the handler is entered; it is charged here, right after the inner
    ///   instruction, which is where revm's own body charges it (operands popped and the host read
    ///   — the read that marks volatile access — already done). The charge therefore lands *inside*
    ///   the measurement window and must not be added back on top of it.
    macro_rules! wrap_op_compute_gas {
        ($fn_name:ident, $opcode:ident, $original_fn:path) => {
            #[doc = concat!("`", stringify!($opcode), "` opcode with compute gas tracking.")]
            #[inline]
            pub fn $fn_name<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
                context: InstructionContext<'_, H, WIRE>,
            ) -> InstructionExecResult {
                // Captured before the inner opcode runs. Plain opcodes charge no storage gas, so
                // the measurement window starting here plus the opcode's pre-charged static gas
                // covers exactly the opcode's compute work.
                let gas_before = context.interpreter.gas.remaining();

                // Call the original instruction
                run_inner_instruction_or_abort!($original_fn, context, inner_outcome);

                let gas_used = const { static_gas(opcode::$opcode) } +
                    gas_before.saturating_sub(context.interpreter.gas.remaining());
                let mut additional_limit = context.host.additional_limit().borrow_mut();
                compute_gas!(context.interpreter, additional_limit, gas_used);
                inner_outcome
            }
        };
        (@self_charged $fn_name:ident, $opcode:ident, $original_fn:path) => {
            #[doc = concat!("`", stringify!($opcode), "` opcode with compute gas tracking, charging its own static gas.")]
            #[inline]
            pub fn $fn_name<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
                context: InstructionContext<'_, H, WIRE>,
            ) -> InstructionExecResult {
                let gas_before = context.interpreter.gas.remaining();

                // Call the original instruction
                run_inner_instruction_or_abort!($original_fn, context, inner_outcome);

                // revm charges this opcode's static gas from here — after the operand pop, after
                // the host read. A frame that cannot afford it has already had its stack popped and
                // its account read, so it halts on whatever the body raised first (stack underflow,
                // an unrepresentable operand, a database failure) and its volatile-access mark
                // stands, instead of being pre-empted by an out-of-gas halt before the body ran.
                charge_static_gas!(context, $opcode);

                // No static-gas add-back: unlike the pre-charged wrappers above, the charge is
                // inside this window.
                let gas_used = gas_before.saturating_sub(context.interpreter.gas.remaining());
                let mut additional_limit = context.host.additional_limit().borrow_mut();
                compute_gas!(context.interpreter, additional_limit, gas_used);
                inner_outcome
            }
        };
        (@frame $fn_name:ident, $opcode:ident, $original_fn:path) => {
            #[doc = concat!("`", stringify!($opcode), "` opcode with compute gas tracking.")]
            #[inline]
            pub fn $fn_name<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
                context: InstructionContext<'_, H, WIRE>,
            ) -> InstructionExecResult {
                let gas_before = context.interpreter.gas.remaining();

                // Call the original instruction
                run_inner_instruction_or_abort!($original_fn, context, inner_outcome);

                let mut gas_used = const { static_gas(opcode::$opcode) } +
                    gas_before.saturating_sub(context.interpreter.gas.remaining());
                // Subtract the gas forwarded to the child. REX5 excludes the revm-side
                // `CALL_STIPEND` (added by value-transferring CALL/CALLCODE without
                // deducting from the parent) so parent compute-gas is not under-counted.
                // Pre-REX5 keeps the legacy raw-`gas_limit` subtraction for replay parity.
                match context.interpreter.bytecode.action() {
                    Some(InterpreterAction::NewFrame(FrameInput::Call(call_inputs))) => {
                        let stipend_from_revm = if context
                            .host
                            .spec_id()
                            .is_enabled(MegaSpecId::REX5) &&
                            matches!(call_inputs.scheme, CallScheme::Call | CallScheme::CallCode) &&
                            call_inputs.transfers_value()
                        {
                            gas::CALL_STIPEND
                        } else {
                            0
                        };
                        let parent_contributed =
                            call_inputs.gas_limit.saturating_sub(stipend_from_revm);
                        gas_used = gas_used.saturating_sub(parent_contributed);
                    }
                    Some(InterpreterAction::NewFrame(FrameInput::Create(create_inputs))) => {
                        gas_used = gas_used.saturating_sub(create_inputs.gas_limit());
                    }
                    _ => {}
                }
                let mut additional_limit = context.host.additional_limit().borrow_mut();
                compute_gas!(context.interpreter, additional_limit, gas_used);
                inner_outcome
            }
        };
    }

    wrap_op_compute_gas!(stop, STOP, instructions::control::stop);
    wrap_op_compute_gas!(add, ADD, instructions::arithmetic::add);
    wrap_op_compute_gas!(mul, MUL, instructions::arithmetic::mul);
    wrap_op_compute_gas!(sub, SUB, instructions::arithmetic::sub);
    wrap_op_compute_gas!(div, DIV, instructions::arithmetic::div);
    wrap_op_compute_gas!(sdiv, SDIV, instructions::arithmetic::sdiv);
    wrap_op_compute_gas!(rem, MOD, instructions::arithmetic::rem);
    wrap_op_compute_gas!(smod, SMOD, instructions::arithmetic::smod);
    wrap_op_compute_gas!(addmod, ADDMOD, instructions::arithmetic::addmod);
    wrap_op_compute_gas!(mulmod, MULMOD, instructions::arithmetic::mulmod);
    wrap_op_compute_gas!(exp, EXP, instructions::arithmetic::exp);
    wrap_op_compute_gas!(signextend, SIGNEXTEND, instructions::arithmetic::signextend);

    wrap_op_compute_gas!(lt, LT, instructions::bitwise::lt);
    wrap_op_compute_gas!(gt, GT, instructions::bitwise::gt);
    wrap_op_compute_gas!(slt, SLT, instructions::bitwise::slt);
    wrap_op_compute_gas!(sgt, SGT, instructions::bitwise::sgt);
    wrap_op_compute_gas!(eq, EQ, instructions::bitwise::eq);
    wrap_op_compute_gas!(iszero, ISZERO, instructions::bitwise::iszero);
    wrap_op_compute_gas!(bitand, AND, instructions::bitwise::bitand);
    wrap_op_compute_gas!(bitor, OR, instructions::bitwise::bitor);
    wrap_op_compute_gas!(bitxor, XOR, instructions::bitwise::bitxor);
    wrap_op_compute_gas!(not, NOT, instructions::bitwise::not);
    wrap_op_compute_gas!(byte, BYTE, instructions::bitwise::byte);
    wrap_op_compute_gas!(shl, SHL, instructions::bitwise::shl);
    wrap_op_compute_gas!(shr, SHR, instructions::bitwise::shr);
    wrap_op_compute_gas!(sar, SAR, instructions::bitwise::sar);
    wrap_op_compute_gas!(clz, CLZ, instructions::bitwise::clz);

    wrap_op_compute_gas!(keccak256, KECCAK256, instructions::system::keccak256);

    wrap_op_compute_gas!(address, ADDRESS, instructions::system::address);
    wrap_op_compute_gas!(@self_charged balance, BALANCE, instructions::host::balance);
    wrap_op_compute_gas!(origin, ORIGIN, instructions::tx_info::origin);
    wrap_op_compute_gas!(caller, CALLER, instructions::system::caller);
    wrap_op_compute_gas!(callvalue, CALLVALUE, instructions::system::callvalue);
    wrap_op_compute_gas!(calldataload, CALLDATALOAD, instructions::system::calldataload);
    wrap_op_compute_gas!(calldatasize, CALLDATASIZE, instructions::system::calldatasize);
    wrap_op_compute_gas!(calldatacopy, CALLDATACOPY, instructions::system::calldatacopy);
    wrap_op_compute_gas!(codesize, CODESIZE, instructions::system::codesize);
    wrap_op_compute_gas!(codecopy, CODECOPY, instructions::system::codecopy);

    wrap_op_compute_gas!(gasprice, GASPRICE, instructions::tx_info::gasprice);
    wrap_op_compute_gas!(@self_charged extcodesize, EXTCODESIZE, instructions::host::extcodesize);
    wrap_op_compute_gas!(@self_charged extcodecopy, EXTCODECOPY, instructions::host::extcodecopy);
    wrap_op_compute_gas!(returndatasize, RETURNDATASIZE, instructions::system::returndatasize);
    wrap_op_compute_gas!(returndatacopy, RETURNDATACOPY, instructions::system::returndatacopy);
    wrap_op_compute_gas!(@self_charged extcodehash, EXTCODEHASH, instructions::host::extcodehash);
    wrap_op_compute_gas!(blockhash, BLOCKHASH, instructions::host::blockhash);
    wrap_op_compute_gas!(coinbase, COINBASE, instructions::block_info::coinbase);
    wrap_op_compute_gas!(timestamp, TIMESTAMP, instructions::block_info::timestamp);
    wrap_op_compute_gas!(number, NUMBER, instructions::block_info::block_number);
    wrap_op_compute_gas!(difficulty, DIFFICULTY, instructions::block_info::difficulty);
    wrap_op_compute_gas!(gaslimit, GASLIMIT, instructions::block_info::gaslimit);
    wrap_op_compute_gas!(chainid, CHAINID, instructions::block_info::chainid);
    wrap_op_compute_gas!(selfbalance, SELFBALANCE, instructions::host::selfbalance);
    wrap_op_compute_gas!(basefee, BASEFEE, instructions::block_info::basefee);
    wrap_op_compute_gas!(blobhash, BLOBHASH, instructions::tx_info::blob_hash);
    wrap_op_compute_gas!(blobbasefee, BLOBBASEFEE, instructions::block_info::blob_basefee);

    wrap_op_compute_gas!(pop, POP, instructions::stack::pop);
    wrap_op_compute_gas!(mload, MLOAD, instructions::memory::mload);
    wrap_op_compute_gas!(mstore, MSTORE, instructions::memory::mstore);
    wrap_op_compute_gas!(mstore8, MSTORE8, instructions::memory::mstore8);
    wrap_op_compute_gas!(sload, SLOAD, instructions::host::sload);
    wrap_op_compute_gas!(@self_charged sload_self_charged, SLOAD, instructions::host::sload);
    wrap_op_compute_gas!(jump, JUMP, instructions::control::jump);
    wrap_op_compute_gas!(jumpi, JUMPI, instructions::control::jumpi);
    wrap_op_compute_gas!(pc, PC, instructions::control::pc);
    wrap_op_compute_gas!(msize, MSIZE, instructions::memory::msize);
    wrap_op_compute_gas!(gas, GAS, instructions::system::gas);
    wrap_op_compute_gas!(jumpdest, JUMPDEST, instructions::control::jumpdest);
    wrap_op_compute_gas!(tload, TLOAD, instructions::host::tload);
    wrap_op_compute_gas!(tstore, TSTORE, instructions::host::tstore);
    wrap_op_compute_gas!(mcopy, MCOPY, instructions::memory::mcopy);

    wrap_op_compute_gas!(push0, PUSH0, instructions::stack::push0);
    wrap_op_compute_gas!(push1, PUSH1, instructions::stack::push::<1, _, _>);
    wrap_op_compute_gas!(push2, PUSH2, instructions::stack::push::<2, _, _>);
    wrap_op_compute_gas!(push3, PUSH3, instructions::stack::push::<3, _, _>);
    wrap_op_compute_gas!(push4, PUSH4, instructions::stack::push::<4, _, _>);
    wrap_op_compute_gas!(push5, PUSH5, instructions::stack::push::<5, _, _>);
    wrap_op_compute_gas!(push6, PUSH6, instructions::stack::push::<6, _, _>);
    wrap_op_compute_gas!(push7, PUSH7, instructions::stack::push::<7, _, _>);
    wrap_op_compute_gas!(push8, PUSH8, instructions::stack::push::<8, _, _>);
    wrap_op_compute_gas!(push9, PUSH9, instructions::stack::push::<9, _, _>);
    wrap_op_compute_gas!(push10, PUSH10, instructions::stack::push::<10, _, _>);
    wrap_op_compute_gas!(push11, PUSH11, instructions::stack::push::<11, _, _>);
    wrap_op_compute_gas!(push12, PUSH12, instructions::stack::push::<12, _, _>);
    wrap_op_compute_gas!(push13, PUSH13, instructions::stack::push::<13, _, _>);
    wrap_op_compute_gas!(push14, PUSH14, instructions::stack::push::<14, _, _>);
    wrap_op_compute_gas!(push15, PUSH15, instructions::stack::push::<15, _, _>);
    wrap_op_compute_gas!(push16, PUSH16, instructions::stack::push::<16, _, _>);
    wrap_op_compute_gas!(push17, PUSH17, instructions::stack::push::<17, _, _>);
    wrap_op_compute_gas!(push18, PUSH18, instructions::stack::push::<18, _, _>);
    wrap_op_compute_gas!(push19, PUSH19, instructions::stack::push::<19, _, _>);
    wrap_op_compute_gas!(push20, PUSH20, instructions::stack::push::<20, _, _>);
    wrap_op_compute_gas!(push21, PUSH21, instructions::stack::push::<21, _, _>);
    wrap_op_compute_gas!(push22, PUSH22, instructions::stack::push::<22, _, _>);
    wrap_op_compute_gas!(push23, PUSH23, instructions::stack::push::<23, _, _>);
    wrap_op_compute_gas!(push24, PUSH24, instructions::stack::push::<24, _, _>);
    wrap_op_compute_gas!(push25, PUSH25, instructions::stack::push::<25, _, _>);
    wrap_op_compute_gas!(push26, PUSH26, instructions::stack::push::<26, _, _>);
    wrap_op_compute_gas!(push27, PUSH27, instructions::stack::push::<27, _, _>);
    wrap_op_compute_gas!(push28, PUSH28, instructions::stack::push::<28, _, _>);
    wrap_op_compute_gas!(push29, PUSH29, instructions::stack::push::<29, _, _>);
    wrap_op_compute_gas!(push30, PUSH30, instructions::stack::push::<30, _, _>);
    wrap_op_compute_gas!(push31, PUSH31, instructions::stack::push::<31, _, _>);
    wrap_op_compute_gas!(push32, PUSH32, instructions::stack::push::<32, _, _>);

    wrap_op_compute_gas!(dup1, DUP1, instructions::stack::dup::<1, _, _>);
    wrap_op_compute_gas!(dup2, DUP2, instructions::stack::dup::<2, _, _>);
    wrap_op_compute_gas!(dup3, DUP3, instructions::stack::dup::<3, _, _>);
    wrap_op_compute_gas!(dup4, DUP4, instructions::stack::dup::<4, _, _>);
    wrap_op_compute_gas!(dup5, DUP5, instructions::stack::dup::<5, _, _>);
    wrap_op_compute_gas!(dup6, DUP6, instructions::stack::dup::<6, _, _>);
    wrap_op_compute_gas!(dup7, DUP7, instructions::stack::dup::<7, _, _>);
    wrap_op_compute_gas!(dup8, DUP8, instructions::stack::dup::<8, _, _>);
    wrap_op_compute_gas!(dup9, DUP9, instructions::stack::dup::<9, _, _>);
    wrap_op_compute_gas!(dup10, DUP10, instructions::stack::dup::<10, _, _>);
    wrap_op_compute_gas!(dup11, DUP11, instructions::stack::dup::<11, _, _>);
    wrap_op_compute_gas!(dup12, DUP12, instructions::stack::dup::<12, _, _>);
    wrap_op_compute_gas!(dup13, DUP13, instructions::stack::dup::<13, _, _>);
    wrap_op_compute_gas!(dup14, DUP14, instructions::stack::dup::<14, _, _>);
    wrap_op_compute_gas!(dup15, DUP15, instructions::stack::dup::<15, _, _>);
    wrap_op_compute_gas!(dup16, DUP16, instructions::stack::dup::<16, _, _>);

    wrap_op_compute_gas!(swap1, SWAP1, instructions::stack::swap::<1, _, _>);
    wrap_op_compute_gas!(swap2, SWAP2, instructions::stack::swap::<2, _, _>);
    wrap_op_compute_gas!(swap3, SWAP3, instructions::stack::swap::<3, _, _>);
    wrap_op_compute_gas!(swap4, SWAP4, instructions::stack::swap::<4, _, _>);
    wrap_op_compute_gas!(swap5, SWAP5, instructions::stack::swap::<5, _, _>);
    wrap_op_compute_gas!(swap6, SWAP6, instructions::stack::swap::<6, _, _>);
    wrap_op_compute_gas!(swap7, SWAP7, instructions::stack::swap::<7, _, _>);
    wrap_op_compute_gas!(swap8, SWAP8, instructions::stack::swap::<8, _, _>);
    wrap_op_compute_gas!(swap9, SWAP9, instructions::stack::swap::<9, _, _>);
    wrap_op_compute_gas!(swap10, SWAP10, instructions::stack::swap::<10, _, _>);
    wrap_op_compute_gas!(swap11, SWAP11, instructions::stack::swap::<11, _, _>);
    wrap_op_compute_gas!(swap12, SWAP12, instructions::stack::swap::<12, _, _>);
    wrap_op_compute_gas!(swap13, SWAP13, instructions::stack::swap::<13, _, _>);
    wrap_op_compute_gas!(swap14, SWAP14, instructions::stack::swap::<14, _, _>);
    wrap_op_compute_gas!(swap15, SWAP15, instructions::stack::swap::<15, _, _>);
    wrap_op_compute_gas!(swap16, SWAP16, instructions::stack::swap::<16, _, _>);

    wrap_op_compute_gas!(@frame call_code, CALLCODE, call_resolving_target::<OP_CALLCODE, _, _>);
    wrap_op_compute_gas!(ret, RETURN, instructions::control::ret);
    wrap_op_compute_gas!(@frame delegate_call, DELEGATECALL, call_resolving_target::<OP_DELEGATECALL, _, _>);
    wrap_op_compute_gas!(@frame static_call, STATICCALL, call_resolving_target::<OP_STATICCALL, _, _>);

    wrap_op_compute_gas!(revert, REVERT, instructions::control::revert);
    wrap_op_compute_gas!(invalid, INVALID, instructions::control::invalid);

    /// `SELFDESTRUCT` opcode with compute gas tracking, for the specs whose static gas entry the
    /// interpreter still pre-charges (`REX2`, `REX3` — the two that re-enabled the opcode before it
    /// gained a volatile guard).
    pub fn selfdestruct<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        selfdestruct_impl::<false, WIRE, H>(context)
    }

    /// `SELFDESTRUCT` opcode with compute gas tracking, charging its own static gas.
    ///
    /// `REX4+` zeroes the opcode's static gas entry so its beneficiary guard is reached even by a
    /// frame that cannot afford the 5,000 gas; this variant charges that entry where revm's body
    /// does, after the static-context check, the target pop and the `host.selfdestruct` call that
    /// marks beneficiary access.
    pub fn selfdestruct_self_charged<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        selfdestruct_impl::<true, WIRE, H>(context)
    }

    /// Shared body of the two `SELFDESTRUCT` compute-gas wrappers.
    ///
    /// `SELF_CHARGES_STATIC_GAS` says who owes the opcode's static gas: when set, this handler
    /// charges it inside the measurement window and does not add it back; when clear, the
    /// interpreter pre-charged it outside the window and it is added back, exactly as in
    /// [`wrap_op_compute_gas`].
    ///
    /// Unlike the default wrapper, the trailing check fans out across all four limit
    /// dimensions (`record_compute_gas_all_dims`): the REX5 storage wrapper records
    /// beneficiary data/KV/state usage *before* the inner instruction runs, without
    /// latching, and those dimensions must latch (and halt) here — only once the inner
    /// instruction has succeeded. Latching at the recording site instead would stick
    /// even when the inner instruction subsequently fails and the frame's discardable
    /// usage is rolled back.
    #[inline]
    fn selfdestruct_impl<
        const SELF_CHARGES_STATIC_GAS: bool,
        WIRE: InterpreterTypes,
        H: HostExt + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        let gas_before = context.interpreter.gas.remaining();

        // Call the original instruction
        run_inner_instruction_or_abort!(instructions::host::selfdestruct, context, inner_outcome);

        if SELF_CHARGES_STATIC_GAS {
            charge_static_gas!(context, SELFDESTRUCT);
        }
        let pre_charged =
            if SELF_CHARGES_STATIC_GAS { 0 } else { const { static_gas(opcode::SELFDESTRUCT) } };
        let gas_after = context.interpreter.gas.remaining();
        let mut additional_limit = context.host.additional_limit().borrow_mut();
        // The per-opcode `gas_before` window applies on every spec. Under checkpoint accounting the
        // plain segment ahead of this opcode was already settled by the `checkpoint_prologue!` in
        // `storage_gas_ext::selfdestruct`, which also restored the clamp; the window is re-opened
        // here so the frame's final settlement cannot bill this body a second time.
        let gas_used = pre_charged + gas_before.saturating_sub(gas_after);
        if additional_limit.rex7_enabled() {
            additional_limit.sync_checkpoint_baseline(gas_after);
        }
        if !additional_limit.record_compute_gas_all_dims(gas_used) {
            // A successful inner SELFDESTRUCT has already set its return action, which the halt
            // replaces; the `Err` is what stops the interpreter loop.
            let result = additional_limit.exceeding_instruction_result();
            set_halt_action!(context.interpreter, result);
            return Err(result);
        }
        inner_outcome
    }

    /// `GAS` as a REX7 checkpoint.
    ///
    /// `GAS` has to be a checkpoint under gas-clamp enforcement even though it charges nothing but
    /// its static gas: the prologue hands the clamp-hidden gas back before the raw instruction
    /// reads the counter, so the value pushed on the stack is the true remaining and the clamp
    /// stays invisible to any transaction that never exceeds a limit.
    #[inline]
    pub fn gas_checkpoint<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
        context: InstructionContext<'_, H, WIRE>,
    ) -> InstructionExecResult {
        checkpoint_prologue!(context);
        let gas_before = context.interpreter.gas.remaining();
        run_inner_instruction_or_abort!(instructions::system::gas, context, inner_outcome);
        record_checkpoint_body_compute_gas!(context, gas_before);
        checkpoint_epilogue!(context);
        inner_outcome
    }
}

/// Trait to inspect the stack elements.
pub trait StackInspectTr {
    /// Inspect the N-th element of the stack. The top of the stack is the 0-th element.
    /// If the stack is too short, return None.
    fn inspect<const N: usize>(&self) -> Option<U256>;
}

impl StackInspectTr for Stack {
    fn inspect<const N: usize>(&self) -> Option<U256> {
        if N >= self.len() {
            return None;
        }
        let index = self.len() - 1 - N;
        // SAFETY: the index must be within the bounds of the stack
        Some(unsafe { *self.data().get_unchecked(index) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_SPECS: [MegaSpecId; 10] = [
        MegaSpecId::EQUIVALENCE,
        MegaSpecId::MINI_REX,
        MegaSpecId::REX,
        MegaSpecId::REX1,
        MegaSpecId::REX2,
        MegaSpecId::REX3,
        MegaSpecId::REX4,
        MegaSpecId::REX5,
        MegaSpecId::REX6,
        MegaSpecId::REX7,
    ];

    /// [`STATIC_GAS_TABLE`] is a single table shared by every handler, so the static gas it adds
    /// back is only correct as long as every spec's instruction table was built from the same
    /// Ethereum spec. A future spec that maps to a different one must give its handlers their own
    /// table instead of silently metering against this one.
    #[test]
    fn test_static_gas_table_is_spec_invariant() {
        for spec in ALL_SPECS {
            assert_eq!(
                gas_table_spec(spec.into_eth_spec()),
                STATIC_GAS_TABLE,
                "{spec} builds its instruction table from a different static gas table",
            );
        }
    }

    /// Pins the table entries the measurement windows depend on.
    ///
    /// The entry is the portion the interpreter charges before the handler runs, which for the
    /// account-touching opcodes is the *warm* access cost only — the cold surcharge and every other
    /// dynamic component are charged inside the body and therefore already land inside the window.
    /// An entry that silently became the opcode's full cost would make the handlers double-count.
    #[test]
    fn test_static_gas_matches_known_opcode_costs() {
        // Fully static opcodes: the whole cost is pre-charged and none of it lands in the window.
        assert_eq!(static_gas(opcode::STOP), 0);
        assert_eq!(static_gas(opcode::ADD), 3);
        assert_eq!(static_gas(opcode::PUSH1), 3);
        assert_eq!(static_gas(opcode::JUMPDEST), 1);
        // Warm access cost only (EIP-2929); the cold surcharge is charged in the body.
        assert_eq!(static_gas(opcode::SLOAD), gas::WARM_STORAGE_READ_COST);
        assert_eq!(static_gas(opcode::BALANCE), gas::WARM_STORAGE_READ_COST);
        assert_eq!(static_gas(opcode::CALL), gas::WARM_STORAGE_READ_COST);
        assert_eq!(static_gas(opcode::STATICCALL), gas::WARM_STORAGE_READ_COST);
        // Base LOG cost; the per-topic and per-byte costs are charged in the body.
        assert_eq!(static_gas(opcode::LOG0), gas::LOG);
        assert_eq!(static_gas(opcode::LOG4), gas::LOG);
        assert_eq!(static_gas(opcode::SELFDESTRUCT), 5_000);
        // Charged entirely inside the body.
        assert_eq!(static_gas(opcode::SSTORE), 0);
        assert_eq!(static_gas(opcode::CREATE), 0);
        assert_eq!(static_gas(opcode::CREATE2), 0);
    }

    /// The opcodes `spec`'s instruction table wraps in a `volatile_data_ext` handler, and whose
    /// static gas [`gas_table_for_spec`] therefore has to leave to the handler.
    ///
    /// Maintained by hand against the table wiring above, so that the pin below fails when a
    /// volatile wrapper is added to (or removed from) a table without its gas entry following.
    fn volatile_guarded_opcodes(spec: MegaSpecId) -> Vec<u8> {
        use opcode::*;
        if !spec.is_enabled(MegaSpecId::MINI_REX) {
            return Vec::new();
        }
        let mut opcodes = vec![
            // `wrap_op_detain_gas_unconditional!`
            BLOCKHASH,
            COINBASE,
            TIMESTAMP,
            NUMBER,
            DIFFICULTY,
            GASLIMIT,
            BASEFEE,
            BLOBBASEFEE,
            BLOBHASH,
            // `wrap_op_detain_gas_conditional!`
            BALANCE,
            EXTCODESIZE,
            EXTCODECOPY,
            EXTCODEHASH,
        ];
        if spec.is_enabled(MegaSpecId::REX3) {
            // `volatile_data_ext::sload`
            opcodes.push(SLOAD);
        }
        if spec.is_enabled(MegaSpecId::REX4) {
            // `wrap_call_volatile_check!`
            opcodes.extend([CALL, CALLCODE, DELEGATECALL, STATICCALL]);
            // `volatile_data_ext::selfdestruct` (REX5+: `selfdestruct_with_beneficiary_guard`)
            opcodes.push(SELFDESTRUCT);
            // `volatile_data_ext::selfbalance`
            opcodes.push(SELFBALANCE);
        }
        opcodes.sort_unstable();
        opcodes
    }

    /// A volatile-guarded opcode must have a zero static gas entry, and every other opcode must
    /// keep revm's.
    ///
    /// The two halves of the mechanism are wired in different places — the guard charges its
    /// opcode's static gas in `volatile_data_ext`, the table stops pre-charging it in
    /// [`gas_table_for_spec`] — so a mismatch is silent but consensus-visible: an opcode zeroed
    /// without a guard to charge it becomes free, and a guard whose entry was left in place charges
    /// its opcode twice.
    ///
    /// The disabled and never-wired opcodes are deliberately not in the zeroed set. Their entries
    /// are charged by a frame the handler was going to strip of its whole budget anyway, so the
    /// pre-charge moves no gas — it only decides which halt an underfunded frame reports, and that
    /// is not a surface this table is used to pin.
    #[test]
    fn test_only_volatile_guarded_opcodes_have_zero_static_gas() {
        for spec in ALL_SPECS {
            let vanilla = gas_table_spec(spec.into_eth_spec());
            let table = gas_table_for_spec(spec);
            let expected = volatile_guarded_opcodes(spec);

            let zeroed: Vec<u8> = (0..=u8::MAX)
                .filter(|&op| table[op as usize] == 0 && vanilla[op as usize] != 0)
                .collect();
            assert_eq!(
                zeroed, expected,
                "{spec}: the opcodes whose static gas is left to the handler must be exactly the \
                 volatile-guarded ones",
            );

            for op in 0..=u8::MAX {
                if !expected.contains(&op) {
                    assert_eq!(
                        table[op as usize], vanilla[op as usize],
                        "{spec}: opcode {op:#04x} is not volatile-guarded, so its static gas must \
                         stay exactly revm's",
                    );
                }
            }
        }
    }

    /// [`create_opcode`] feeds the static-gas lookup for the `IS_CREATE2`-generic create handlers.
    #[test]
    fn test_create_opcode_selects_by_const_generic() {
        assert_eq!(storage_gas_ext::create_opcode(false), opcode::CREATE);
        assert_eq!(storage_gas_ext::create_opcode(true), opcode::CREATE2);
    }

    /// Pins `Debug` for `MegaInstructions` so a `fmt` body that returns `Ok(Default)`
    /// (empty formatter write) fails.
    #[test]
    fn test_mega_instructions_debug_fmt_is_non_empty() {
        let instructions = MegaInstructions::<
            crate::test_utils::MemoryDatabase,
            crate::EmptyExternalEnv,
        >::new(MegaSpecId::REX5);
        let rendered = format!("{instructions:?}");
        assert!(!rendered.is_empty(), "Debug output must write at least one byte",);
        assert!(
            rendered.contains("MegaethInstructions") || rendered.contains("REX5"),
            "Debug output must identify the type or the configured spec; got {rendered}",
        );
    }
}
