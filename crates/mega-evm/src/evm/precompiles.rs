//! Custom precompiles for `MegaETH` EVM.
//!
//! This module provides custom precompile implementations with `MegaETH`-specific
//! gas cost overrides.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
};

use crate::{ExternalEnvTypes, MegaContext, MegaInnerContext, MegaSpecId};
use alloy_evm::{
    precompiles::{DynPrecompile, Precompile, PrecompileInput, PrecompilesMap},
    Database, EvmInternals,
};
use delegate::delegate;
use once_cell::race::OnceBox;
use op_revm::OpSpecId;
use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{precompile_output_to_interpreter_result, EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, Gas, InterpreterResult},
    precompile::{PrecompileHalt, Precompiles},
    primitives::{Address, AddressSet, HashMap},
};

/// `MegaETH` precompile provider with custom gas cost overrides.
#[derive(Debug, Clone)]
pub struct MegaPrecompiles {
    /// Inner precompile provider from op-revm.
    inner: EthPrecompiles,
    /// The `MegaETH` specification ID.
    spec: MegaSpecId,
}

impl MegaPrecompiles {
    /// Create a new precompile provider with the given `MegaETH` spec.
    #[inline]
    pub fn new_with_spec(spec: MegaSpecId) -> Self {
        // Get base precompiles from op-revm
        let inner = match spec {
            MegaSpecId::EQUIVALENCE => op_revm::precompiles::isthmus(),
            MegaSpecId::MINI_REX => mini_rex(),
            MegaSpecId::REX |
            MegaSpecId::REX1 |
            MegaSpecId::REX2 |
            MegaSpecId::REX3 |
            MegaSpecId::REX4 |
            MegaSpecId::REX5 |
            MegaSpecId::REX6 |
            MegaSpecId::REX7 => rex(),
        };

        Self { inner: EthPrecompiles { precompiles: inner, spec: spec.into_eth_spec() }, spec }
    }

    /// Get the precompiles for the current spec.
    ///
    /// This method returns precompiles with custom gas cost overrides for `MINI_REX` spec.
    #[inline]
    pub fn precompiles(&self) -> &'static Precompiles {
        // For now, just use the inner precompiles
        // Custom gas costs will be applied in the run() method
        self.inner.precompiles
    }
}

/// Precompiles for the `REX` spec.
pub fn rex() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(mini_rex().clone()))
}

/// Precompiles for the `MINI_REX` spec.
pub fn mini_rex() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = op_revm::precompiles::isthmus().clone();
        // Use the OSAKA modexp precompile (with the frozen legacy schedule) for MINI_REX
        precompiles.extend([modexp::OSAKA_LEGACY, kzg_point_evaluation::KZG_POINT_EVALUATION]);
        Box::new(precompiles)
    })
}

/// Customized `ModExp` precompile module.
///
/// `MINI_REX` adopted the Osaka / EIP-7883 pricing schedule through the revm implementation
/// current at the time, and that implementation short-circuited zero-base/zero-modulus inputs
/// to the 500-gas minimum *before* computing the EIP-7883 formula cost. Later revm versions
/// compute — and charge — the full formula cost for those inputs, as the EIP text specifies
/// (EIP-7883 has no zero-length special case, and its multiplication complexity has a floor
/// of 16, so a zero-base/zero-modulus call with a 64-byte exponent length prices in the
/// thousands rather than 500).
///
/// Every spec is pinned to the historical short-circuit, so replaying a block reproduces the
/// gas it was executed with. Charging the EIP-7883 formula cost for these inputs instead is a
/// consensus change, and belongs to a spec that adopts it deliberately.
pub mod modexp {
    use revm::{
        precompile::{
            self, u64_to_address, utilities::right_pad_with_offset, Precompile, PrecompileHalt,
            PrecompileId, PrecompileOutput, PrecompileResult,
        },
        primitives::{eip7823, Address, Bytes, U256},
    };

    /// Address of the `ModExp` precompile.
    pub const ADDRESS: Address = u64_to_address(5);

    /// Minimum gas of the Osaka `ModExp` schedule, and the flat charge of the legacy
    /// zero-base/zero-modulus short-circuit.
    pub const MIN_GAS: u64 = 500;

    /// Runs `ModExp` with the frozen-spec legacy Osaka schedule.
    ///
    /// Reproduces the historical implementation's check order for the intercepted region:
    /// minimum-gas check, header parse, EIP-7823 size limits, then the
    /// zero-base/zero-modulus short-circuit returning [`MIN_GAS`]. Every other input defers
    /// to the upstream implementation, whose leading checks are identical, so the wrapper
    /// diverges from upstream only in the short-circuit's position and charge.
    fn run_osaka_legacy(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
        if MIN_GAS > gas_limit {
            return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
        }

        let base_len = U256::from_be_bytes(right_pad_with_offset::<32>(input, 0).into_owned());
        let exp_len = U256::from_be_bytes(right_pad_with_offset::<32>(input, 32).into_owned());
        let mod_len = U256::from_be_bytes(right_pad_with_offset::<32>(input, 64).into_owned());

        let (Ok(base_len), Ok(mod_len)) = (usize::try_from(base_len), usize::try_from(mod_len))
        else {
            return Ok(PrecompileOutput::halt(PrecompileHalt::ModexpEip7823LimitSize, reservoir));
        };
        let exp_len = usize::try_from(exp_len).unwrap_or(usize::MAX);
        if base_len > eip7823::INPUT_SIZE_LIMIT ||
            mod_len > eip7823::INPUT_SIZE_LIMIT ||
            exp_len > eip7823::INPUT_SIZE_LIMIT
        {
            return Ok(PrecompileOutput::halt(PrecompileHalt::ModexpEip7823LimitSize, reservoir));
        }

        if base_len == 0 && mod_len == 0 {
            return Ok(PrecompileOutput::new(MIN_GAS, Bytes::new(), reservoir));
        }

        Ok(match precompile::modexp::osaka_run(input, gas_limit) {
            Ok(output) => PrecompileOutput::new(output.gas_used, output.bytes, reservoir),
            Err(halt) => PrecompileOutput::halt(halt, reservoir),
        })
    }

    /// `ModExp` with the frozen-spec legacy Osaka schedule.
    pub const OSAKA_LEGACY: Precompile =
        Precompile::new(PrecompileId::ModExp, ADDRESS, run_osaka_legacy);
}

/// Customized KZG point evaluation precompile module.
pub mod kzg_point_evaluation {
    use revm::{
        precompile::{
            Precompile, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
        },
        primitives::Address,
    };

    /// Address of the KZG point evaluation precompile (re-export of the upstream
    /// constant). Local re-export so call-sites can refer to `GAS_COST` and
    /// `ADDRESS` through the same module path.
    pub const ADDRESS: Address = revm::precompile::kzg_point_evaluation::ADDRESS;

    /// Gas cost for the KZG point evaluation precompile.
    pub const GAS_COST: u64 = 100_000;

    /// Runs the upstream KZG point evaluation and overrides the reported gas with `MegaETH`'s
    /// fixed [`GAS_COST`].
    fn run_with_fixed_cost(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
        if gas_limit < GAS_COST {
            return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
        }
        Ok(match revm::precompile::kzg_point_evaluation::run(input, gas_limit) {
            Ok(output) => PrecompileOutput::new(GAS_COST, output.bytes, reservoir),
            Err(halt) => PrecompileOutput::halt(halt, reservoir),
        })
    }

    /// KZG point evaluation precompile. This is the modified version of the original precompile
    /// with a custom gas cost.
    pub const KZG_POINT_EVALUATION: Precompile =
        Precompile::new(PrecompileId::KzgPointEvaluation, ADDRESS, run_with_fixed_cost);
}

impl<CTX> PrecompileProvider<CTX> for MegaPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = MegaSpecId>>,
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        if spec == self.spec {
            return false;
        }

        *self = Self::new_with_spec(spec);
        true
    }

    delegate! {
        to self.inner {
            fn run(
                &mut self,
                context: &mut CTX,
                inputs: &CallInputs,
            ) -> Result<Option<Self::Output>, String>;
            fn warm_addresses(&self) -> &AddressSet;
            fn contains(&self, address: &Address) -> bool;
        }
    }
}

impl Default for MegaPrecompiles {
    fn default() -> Self {
        Self::new_with_spec(MegaSpecId::EQUIVALENCE)
    }
}

/// Runs the precompile addressed by `inputs`, reporting the halt reason next to the
/// `InterpreterResult`.
///
/// This is a mirror of `<PrecompilesMap as PrecompileProvider<Context<..>>>::run`
/// (`alloy_evm::precompiles`, alloy-evm 0.36.0 — the pinned version) rather than a call into it.
/// Delegating would
/// hand back only the converted `InterpreterResult`, and the conversion folds every non-out-of-gas
/// halt into one opaque `PrecompileError` code — so the caller could no longer tell a doorway
/// reject apart from a failure raised mid-computation, which is exactly the distinction the
/// compute-gas split needs. Mirroring keeps the raw `PrecompileStatus` in reach while the
/// `InterpreterResult` still comes out of the same public conversion, so the two paths agree
/// bit-for-bit (pinned by `test_precompile_mirror_matches_upstream_delegation`).
///
/// Upgrading alloy-evm obliges a re-read of that upstream function: a new `PrecompileInput`
/// field, a different dispatch address, a result cache, or any other added step must be mirrored
/// here, or this silently stops being the same call. The next published version, 0.37.1, was read
/// against this one: its `run` body is byte-identical, and the differences in that module are in
/// how `PrecompilesMap` stores its dynamic lookup, which this function does not touch.
fn run_precompile_capturing_halt<DB: Database>(
    precompiles: &PrecompilesMap,
    context: &mut MegaInnerContext<DB>,
    inputs: &CallInputs,
) -> Result<Option<(InterpreterResult, Option<PrecompileHalt>)>, String> {
    let Some(precompile) = precompiles.get(&inputs.bytecode_address) else {
        return Ok(None);
    };

    let (block, tx, cfg, journaled_state, _, local) = context.all_mut();

    let output = {
        let _span =
            tracing::trace_span!("precompile", name = precompile.precompile_id().name()).entered();
        precompile.call(PrecompileInput {
            data: inputs.input.as_bytes_local(local).as_ref(),
            gas: inputs.gas_limit,
            reservoir: inputs.reservoir,
            caller: inputs.caller,
            value: inputs.call_value(),
            is_static: inputs.is_static,
            internals: EvmInternals::new(journaled_state, block, cfg, tx),
            target_address: inputs.target_address,
            bytecode_address: inputs.bytecode_address,
        })
    }
    .map_err(|e| e.to_string())?;

    let halt = output.status.halt_reason().cloned();
    Ok(Some((precompile_output_to_interpreter_result(output, inputs.gas_limit), halt)))
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> PrecompileProvider<MegaContext<DB, ExtEnvs>>
    for PrecompilesMap
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, _spec: OpSpecId) -> bool {
        // The table is already correct and unchanged: MegaEvm bakes it at construction
        // from MegaSpecId. Rebuilding from OpSpecId would lose Mega-specific overrides
        // because all current Mega specs map to the same OpSpecId.
        false
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        // Dispatch-address for precompile lookup and KZG fixed-cost matching.
        // Must be `bytecode_address`, not `target_address`: under DELEGATECALL /
        // CALLCODE the two differ, and both alloy-evm 0.36 and revm-handler dispatch
        // precompiles by the frame's bytecode address.
        let address = inputs.bytecode_address;
        let gas_limit = inputs.gas_limit;
        // REX5+: cap forwarded gas at the current compute-gas remaining so a precompile
        // cannot spend more compute gas than the per-frame / TX-level budget permits.
        // Pre-REX5 keeps the original forwarding semantics for backward compatibility.
        let is_rex5_enabled = context.spec.is_enabled(MegaSpecId::REX5);
        let effective_gas_limit = if is_rex5_enabled {
            let remaining = context.additional_limit.borrow().current_call_remaining_compute_gas();
            gas_limit.min(remaining)
        } else {
            gas_limit
        };

        // The forwarded gas cap is applied by rewriting the child `CallInputs` gas limit, which
        // is where the callee reads its budget from now that `run` no longer takes it separately.
        let mut capped_inputs;
        let inputs = if effective_gas_limit == gas_limit {
            inputs
        } else {
            capped_inputs = inputs.clone();
            capped_inputs.gas_limit = effective_gas_limit;
            &capped_inputs
        };

        // Run the precompile against the context `MegaContext` wraps. `halt` is the precompile's
        // own halt reason, which the `InterpreterResult` no longer carries — the accounting arms
        // below read it instead of trying to rebuild it from the collapsed instruction result.
        let maybe_output = run_precompile_capturing_halt(self, &mut context.inner, inputs)?;

        Ok(maybe_output.map(|(mut output, halt)| {
            // Upstream revm-handler (`precompile_output_to_interpreter_result`) calls
            // `gas.spend_all()` for every non-success/non-revert precompile status, so
            // error-path Gas now reports `total_gas_spent() == limit`. Frozen REX5
            // (and pre-migration revm) left halt Gas as `Gas::new(effective)` with
            // `total_gas_spent() == 0`. Restore that externally-observable shape here
            // before any further accounting — parent still burns the full forwarded
            // `gas_limit` on halt regardless of reported remaining, so this does not
            // change EVM-gas burn, only the Gas object tracers/tests observe.
            if !output.result.is_ok_or_revert() {
                let limit = output.gas.limit();
                output.gas = Gas::new(limit);
            }
            // Normalize the returned Gas back to the caller's original `gas_limit` budget
            // ONLY on `is_ok_or_revert` paths — those are the paths where the caller's
            // refund logic (`EthFrame::return_result` and `Handler::last_frame_result`,
            // both gated on `is_ok_or_revert()`) actually consumes `gas.remaining()`.
            //
            // Halt paths (`PrecompileOOG`, `PrecompileError`) skip the refund entirely:
            // the parent burns the full forwarded `gas_limit` regardless of the Gas
            // object's reported `remaining`. After the spend_all undo above, halt Gas is
            // again `Gas::new(effective)`, which keeps the post-cap halt result
            // semantically identical to an uncapped precompile OOG, matches what tracers
            // / inspectors expect, and makes the "cap forced an OOG" behavior locally
            // indistinguishable from "user just forwarded too little gas".
            //
            // The normalize preserves the precompile's `refunded()` value (today every
            // standard revm precompile leaves refunded == 0, but custom precompiles
            // injected via `PrecompilesMap` may set a refund, and a future EIP
            // precompile could also). Memory expansion gas is not preserved because
            // precompiles do not allocate from `Gas::memory` — memory expansion is
            // the caller-interpreter's responsibility and is settled before the
            // precompile is invoked.
            if is_rex5_enabled && output.result.is_ok_or_revert() {
                let spent = output.gas.total_gas_spent();
                let refunded = output.gas.refunded();
                let mut normalized = Gas::new(gas_limit);
                normalized.set_spent(spent);
                normalized.record_refund(refunded);
                output.gas = normalized;
            }
            // Compute-gas recording on REX5+:
            //
            // * Success / revert: revm called `record_regular_cost`, so `total_gas_spent()` already
            //   reflects the actually-consumed amount. Use it.
            // * Fixed-cost precompile that reached past its wrapper gas pre-check (today: KZG with
            //   `limit() >= GAS_COST`): the call got through the gas gate and into the upstream
            //   body, so how far it got decides what was performed. Address match uses
            //   `bytecode_address` (see above) so DELEGATECALL/CALLCODE to KZG still hit this arm.
            //   Through REX6 the fixed cost is charged for every halt this arm sees and is the
            //   whole recorded charge; REX7 splits it by halt reason (below), keeping the performed
            //   part on the enforcing lane and booking the rest of the forwarded envelope — the
            //   caller-supplied `gas_limit`, not the REX5-capped effective limit — as destroyed.
            //   The REX5 cap still prevents the precompile from *doing* more work than the
            //   remaining compute budget; the cap gap is part of the forwarded envelope, not work,
            //   so it belongs with the destroyed remainder.
            // * All other error paths (non-KZG, or KZG with `limit() < GAS_COST` meaning the
            //   wrapper's pre-check itself OOG'd before the body could run): after the spend_all
            //   undo above, `total_gas_spent() == 0` again. Through REX6 the parent still
            //   permanently loses the forwarded amount, so those specs record `limit()` as
            //   enforcing usage to match the EVM-gas burn. REX7 treats the same path as
            //   performed-zero / destroyed-all: no work ran, so nothing enforces, and the forwarded
            //   envelope (`gas_limit`, again uncapped) is reported only.
            //
            // The split is computed here rather than by the interpreter-frame halt settlement.
            // A halt `Gas` is `Gas::new(limit)` after the undo above (`remaining() == limit()`
            // == the effective, capped budget), so copying that formula would double-count the
            // generic arm's REX6 charge or drop the cap gap.
            if is_rex5_enabled {
                let is_rex7 = context.spec.is_enabled(MegaSpecId::REX7);
                let mut additional_limit = context.additional_limit.borrow_mut();
                if output.result.is_ok_or_revert() {
                    additional_limit.record_compute_gas(output.gas.total_gas_spent());
                } else if address == kzg_point_evaluation::ADDRESS &&
                    output.gas.limit() >= kzg_point_evaluation::GAS_COST
                {
                    // Inside the KZG body the halt reason marks how much of the fixed-price work
                    // the call bought before failing. `BlobInvalidInputLength` is the doorway:
                    // the input length is checked first, so a rejected length means the
                    // commitment was never read and nothing was computed. Every other halt is
                    // raised at or after the versioned-hash comparison, i.e. once verification is
                    // under way, and MegaETH prices verification as the whole fixed fee however
                    // far it got.
                    //
                    // The non-doorway side is a wildcard on purpose: an upstream KZG release that
                    // adds a halt reason lands there, which can only over-charge, never
                    // under-charge. So does the unreachable `halt == None` shape (a success or
                    // revert whose `gas_used` outran the gas limit and was converted to an
                    // out-of-gas), which no MegaETH-wired precompile can produce.
                    //
                    // REX6 and earlier take neither branch's split: they charge the fixed cost for
                    // every halt this arm sees, doorway rejects included.
                    if is_rex7 && matches!(halt, Some(PrecompileHalt::BlobInvalidInputLength)) {
                        additional_limit.record_burned_gas(gas_limit);
                    } else {
                        let executed = kzg_point_evaluation::GAS_COST;
                        // This recording must not latch a limit exceed. If it did, the halt
                        // result would leave `frame_init` with a rescue that hands its whole
                        // remaining gas back to the sender, while the same envelope has just been
                        // booked as destroyed — the same gas reported twice, once refunded and
                        // once burnt.
                        //
                        // It cannot, and the REX5 forwarded-gas cap is what makes that true: the
                        // effective limit this arm tests against `GAS_COST` is
                        // `min(gas_limit, remaining)` for the very `remaining` the limit check
                        // consults, so reaching this arm at all means the fixed fee already fits
                        // inside both the frame and the transaction budget. Nothing between the
                        // cap and here moves that budget — the KZG precompile records no compute
                        // gas of its own.
                        debug_assert!(
                            additional_limit.current_call_remaining_compute_gas() >= executed,
                            "the forwarded-gas cap must keep the KZG fixed fee inside the \
                             remaining compute budget",
                        );
                        additional_limit.record_compute_gas(executed);
                        if is_rex7 {
                            additional_limit.record_burned_gas(gas_limit.saturating_sub(executed));
                        }
                    }
                } else if is_rex7 {
                    // Every wired precompile that reaches this arm halted on a pre-work input
                    // rejection, so nothing was performed and the whole forwarded envelope is
                    // destroyed. A precompile that can halt *after* doing work would need its
                    // own arm (as KZG has) or must express the failure as a revert, which the
                    // ok_or_revert branch above records as actual spend.
                    additional_limit.record_burned_gas(gas_limit);
                } else {
                    additional_limit.record_compute_gas(output.gas.limit());
                }
            } else if context.spec.is_enabled(MegaSpecId::MINI_REX) {
                context
                    .additional_limit
                    .borrow_mut()
                    .record_compute_gas(output.gas.total_gas_spent());
            }
            output
        }))
    }

    #[inline]
    fn warm_addresses(&self) -> &AddressSet {
        PrecompileProvider::<crate::MegaInnerContext<DB>>::warm_addresses(self)
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        PrecompileProvider::<crate::MegaInnerContext<DB>>::contains(self, address)
    }
}

/// A builder function to build dynamic precompiles for a given [`MegaSpecId`].
pub type DynPrecompilesBuilder =
    Arc<dyn Fn(MegaSpecId) -> HashMap<Address, DynPrecompile> + Send + Sync>;

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "std"))]
    use alloc as std;
    use std::{rc::Rc, vec::Vec};

    use super::{
        kzg_point_evaluation::GAS_COST, mini_rex, modexp, rex, run_precompile_capturing_halt,
        MegaPrecompiles,
    };
    use crate::{
        test_utils::MemoryDatabase, AdditionalLimit, EvmTxRuntimeLimits, MegaContext, MegaSpecId,
    };
    use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
    use alloy_primitives::{Address, Bytes, U256};
    use core::cell::RefCell;
    use revm::{
        handler::PrecompileProvider,
        interpreter::{CallInputs, CallScheme, CallValue, InputsImpl, InstructionResult},
        precompile::{PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileStatus},
        primitives::eip7823,
    };
    use sha2::{Digest, Sha256};

    /// Wraps precompile call data into the [`CallInputs`] shape `PrecompilesMap::run` takes,
    /// carrying the bytecode address, static flag and gas limit that used to be separate
    /// arguments.
    fn call_inputs(
        inputs: &InputsImpl,
        address: Address,
        is_static: bool,
        gas_limit: u64,
    ) -> CallInputs {
        call_inputs_with_scheme(inputs, address, address, is_static, gas_limit, CallScheme::Call)
    }

    /// Like [`call_inputs`], but lets tests set a distinct `target_address` /
    /// `bytecode_address` pair and call scheme (for DELEGATECALL / CALLCODE
    /// dispatch-boundary coverage).
    fn call_inputs_with_scheme(
        inputs: &InputsImpl,
        target_address: Address,
        bytecode_address: Address,
        is_static: bool,
        gas_limit: u64,
        scheme: CallScheme,
    ) -> CallInputs {
        CallInputs {
            input: inputs.input.clone(),
            return_memory_offset: 0..0,
            gas_limit,
            reservoir: 0,
            bytecode_address,
            known_bytecode: Default::default(),
            target_address,
            caller: inputs.caller_address,
            value: CallValue::Transfer(inputs.call_value),
            scheme,
            is_static,
            charged_new_account_state_gas: false,
        }
    }

    /// Generate valid KZG Point Evaluation test data from EIP-4844 test vectors.
    fn generate_kzg_test_input() -> InputsImpl {
        let commitment = hex::decode("8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca25f26936857bc3a7c2539ea8ec3a952b7").unwrap();
        let mut versioned_hash = Sha256::digest(&commitment).to_vec();
        versioned_hash[0] = 0x01; // VERSIONED_HASH_VERSION_KZG
        let z = hex::decode("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000")
            .unwrap();
        let y = hex::decode("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9")
            .unwrap();
        let proof = hex::decode("a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc2160744faf0070725e00b60ad9a026a15b1a8c").unwrap();

        let mut input = Vec::new();
        input.extend_from_slice(&versioned_hash);
        input.extend_from_slice(&z);
        input.extend_from_slice(&y);
        input.extend_from_slice(&commitment);
        input.extend_from_slice(&proof);

        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        InputsImpl {
            target_address: address,
            bytecode_address: Some(address),
            caller_address: address,
            input: revm::interpreter::CallInput::Bytes(Bytes::from(input)),
            call_value: Default::default(),
        }
    }

    /// Mirror of `generate_kzg_test_input()` but with the last byte of the proof
    /// flipped — structurally valid (192 bytes, matching versioned hash) but
    /// upstream verification returns `PrecompileError::BlobVerifyKzgProofFailed`.
    fn generate_invalid_proof_kzg_test_input() -> InputsImpl {
        let mut inputs = generate_kzg_test_input();
        let bytes = match inputs.input {
            revm::interpreter::CallInput::Bytes(b) => b,
            _ => panic!("expected Bytes"),
        };
        let mut buf = bytes.to_vec();
        let last = buf.len() - 1;
        buf[last] ^= 0x01;
        inputs.input = revm::interpreter::CallInput::Bytes(Bytes::from(buf));
        inputs
    }

    /// Mirror of `generate_kzg_test_input()` truncated to 191 bytes — one short of the
    /// required 192. Upstream rejects it at the doorway with
    /// `PrecompileHalt::BlobInvalidInputLength`, before the commitment is looked at.
    fn generate_wrong_length_kzg_test_input() -> InputsImpl {
        let mut inputs = generate_kzg_test_input();
        let bytes = match inputs.input {
            revm::interpreter::CallInput::Bytes(b) => b,
            _ => panic!("expected Bytes"),
        };
        let mut buf = bytes.to_vec();
        buf.pop();
        assert_eq!(buf.len(), 191, "the doorway probe must be one byte short of 192");
        inputs.input = revm::interpreter::CallInput::Bytes(Bytes::from(buf));
        inputs
    }

    /// Mirror of `generate_kzg_test_input()` with the versioned hash's trailing byte flipped —
    /// still 192 bytes, so upstream clears the length doorway and fails the following
    /// commitment/versioned-hash comparison with `PrecompileHalt::BlobMismatchedVersion`.
    fn generate_mismatched_version_kzg_test_input() -> InputsImpl {
        let mut inputs = generate_kzg_test_input();
        let bytes = match inputs.input {
            revm::interpreter::CallInput::Bytes(b) => b,
            _ => panic!("expected Bytes"),
        };
        let mut buf = bytes.to_vec();
        buf[31] ^= 0x01;
        inputs.input = revm::interpreter::CallInput::Bytes(Bytes::from(buf));
        inputs
    }

    fn set_spec_for_context<DB: alloy_evm::Database, ExtEnvs: crate::ExternalEnvTypes>(
        precompiles_map: &mut PrecompilesMap,
        _context: &MegaContext<DB, ExtEnvs>,
        spec: op_revm::OpSpecId,
    ) -> bool {
        <PrecompilesMap as PrecompileProvider<MegaContext<DB, ExtEnvs>>>::set_spec(
            precompiles_map,
            spec,
        )
    }

    #[test]
    fn test_rex_and_mini_rex_precompiles_have_distinct_statics() {
        assert!(!core::ptr::eq(rex(), mini_rex()));
    }

    #[test]
    fn test_kzg_precompile_sufficient_gas() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, 200_000));
        assert!(result.is_ok(), "Precompile should succeed with sufficient gas");
        let output = result.unwrap().unwrap();
        assert!(matches!(output.result, InstructionResult::Return), "Result should be Return");
        assert_eq!(output.gas.total_gas_spent(), GAS_COST);
    }

    #[test]
    fn test_kzg_precompile_exact_gas_limit() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, GAS_COST));
        assert!(result.is_ok(), "Precompile should succeed with exact GAS_COST");
        let output = result.unwrap().unwrap();
        assert!(matches!(output.result, InstructionResult::Return), "Result should be Return");
        assert_eq!(output.gas.total_gas_spent(), GAS_COST);
    }

    #[test]
    fn test_set_spec_preserves_mega_kzg_override() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );

        let changed =
            set_spec_for_context(&mut precompiles_map, &context, op_revm::OpSpecId::ISTHMUS);
        assert!(!changed, "Mega precompile table must remain unchanged");

        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, 200_000));
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::Return));
        assert_eq!(output.gas.total_gas_spent(), GAS_COST);
    }

    #[test]
    fn test_kzg_precompile_insufficient_gas() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, 50_000));
        assert!(result.is_ok(), "Should not panic");
        let output = result.unwrap();
        assert!(output.is_some(), "Precompile should return Some(result)");
        let interpreter_result = output.unwrap();
        assert!(
            matches!(interpreter_result.result, InstructionResult::PrecompileOOG),
            "Result should be PrecompileOOG"
        );
    }

    #[test]
    fn test_kzg_precompile_one_below_gas_limit() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, GAS_COST - 1));
        assert!(result.is_ok(), "Should not panic");
        let output = result.unwrap();
        assert!(output.is_some(), "Precompile should return Some(result)");
        let interpreter_result = output.unwrap();
        assert!(
            matches!(interpreter_result.result, InstructionResult::PrecompileOOG),
            "Result should be PrecompileOOG"
        );
    }

    #[test]
    fn test_kzg_precompile_zero_gas() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result = precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, 0));
        assert!(result.is_ok(), "Should not panic");
        let output = result.unwrap();
        assert!(output.is_some(), "Precompile should return Some(result)");
        let interpreter_result = output.unwrap();
        assert!(
            matches!(interpreter_result.result, InstructionResult::PrecompileOOG),
            "Result should be PrecompileOOG"
        );
    }

    /// Replaces the context's `additional_limit` with one whose `tx_compute_gas_limit`
    /// is set to `limit`, so tests can exercise the REX5 compute-gas cap path with a
    /// small remaining budget without having to pre-record large gas amounts.
    fn set_tx_compute_gas_limit<DB: alloy_evm::Database, ExtEnvs: crate::ExternalEnvTypes>(
        context: &mut MegaContext<DB, ExtEnvs>,
        spec: MegaSpecId,
        limit: u64,
    ) {
        let tx_limits = EvmTxRuntimeLimits {
            tx_compute_gas_limit: limit,
            ..EvmTxRuntimeLimits::from_spec(spec)
        };
        context.additional_limit = Rc::new(RefCell::new(AdditionalLimit::new(spec, tx_limits)));
    }

    #[test]
    fn test_kzg_precompile_rex5_compute_gas_cap_succeeds() {
        // REX5: forward gas_limit > GAS_COST > remaining compute. Cap should engage but
        // the precompile still fits within the remaining compute budget, so it succeeds.
        // Caller-visible Gas must be normalized back to the original gas_limit.
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        set_tx_compute_gas_limit(&mut context, MegaSpecId::REX5, GAS_COST + 1_000);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX5).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let forwarded_gas = 500_000u64;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::Return));
        // Spent reflects the precompile's actual cost.
        assert_eq!(output.gas.total_gas_spent(), GAS_COST);
        // Limit was normalized back to the caller's forwarded gas so the caller sees the
        // correct refund (forwarded_gas - GAS_COST) instead of (effective_gas_limit - GAS_COST).
        assert_eq!(output.gas.limit(), forwarded_gas);
        assert_eq!(output.gas.remaining(), forwarded_gas - GAS_COST);
        // The compute-gas tracker records the actual spent.
        assert_eq!(context.additional_limit.borrow().get_usage().compute_gas, GAS_COST,);
    }

    #[test]
    fn test_kzg_precompile_rex5_compute_gas_cap_oogs() {
        // Cap forces PrecompileOOG. Compute is charged `output.gas.limit()`
        // (= remaining budget under the cap), exactly exhausting the meter.
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        let compute_gas_limit = GAS_COST - 1;
        set_tx_compute_gas_limit(&mut context, MegaSpecId::REX5, compute_gas_limit);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX5).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, 1_000_000));
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::PrecompileOOG));
        assert_eq!(output.gas.total_gas_spent(), 0);
        assert_eq!(context.additional_limit.borrow().get_usage().compute_gas, compute_gas_limit);
    }

    #[test]
    fn test_kzg_precompile_rex5_no_cap_when_remaining_sufficient() {
        // REX5: when remaining compute already exceeds the forwarded gas_limit, the cap
        // is a no-op and the path behaves identically to pre-REX5.
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX5).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let forwarded_gas = 200_000u64;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::Return));
        assert_eq!(output.gas.total_gas_spent(), GAS_COST);
        assert_eq!(output.gas.limit(), forwarded_gas);
        assert_eq!(output.gas.remaining(), forwarded_gas - GAS_COST);
    }

    #[test]
    fn test_kzg_precompile_rex5_cap_at_exact_gas_cost() {
        // REX5 boundary: remaining compute equals exactly GAS_COST. The precompile must
        // succeed (cost == effective_gas_limit), spending GAS_COST, with normalization
        // yielding remaining = gas_limit - GAS_COST.
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        set_tx_compute_gas_limit(&mut context, MegaSpecId::REX5, GAS_COST);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX5).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let forwarded_gas = 500_000u64;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::Return));
        assert_eq!(output.gas.total_gas_spent(), GAS_COST);
        assert_eq!(output.gas.limit(), forwarded_gas);
        assert_eq!(output.gas.remaining(), forwarded_gas - GAS_COST);
        assert_eq!(context.additional_limit.borrow().get_usage().compute_gas, GAS_COST);
    }

    /// REX5 boundary mirror of `test_kzg_precompile_rex5_cap_at_exact_gas_cost`,
    /// but with an invalid-proof input so upstream returns
    /// `BlobVerifyKzgProofFailed`. `tx_compute_gas_limit == GAS_COST` forces the
    /// outer cap to `effective_gas_limit == GAS_COST`, so the wrapper's
    /// `gas_limit < GAS_COST` pre-check passes by exactly one, upstream KZG runs
    /// verification, fails on the flipped proof, and revm constructs the failure
    /// `Gas` with `Gas::new(effective_gas_limit)` — leaving `output.gas.limit() ==
    /// GAS_COST` because halt paths skip the success-side normalization to the
    /// caller's original `gas_limit`. Recorded compute-gas must equal `GAS_COST`.
    ///
    /// Lives in the inline unit-test module rather than
    /// `tests/rex5/precompile_compute_gas.rs` because the integration helper
    /// cannot reliably hit `remaining == GAS_COST` at the precompile call site:
    /// opcode-level compute-gas consumed by a wrapper contract's MSTORE / PUSH /
    /// CALL sequence shifts the actual `remaining` below `tx_compute_gas_limit`
    /// by a brittle amount. Direct `precompiles_map.run()` bypasses that
    /// pre-work and gives an exact boundary.
    #[test]
    fn test_kzg_precompile_rex5_cap_at_exact_gas_cost_fail() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        set_tx_compute_gas_limit(&mut context, MegaSpecId::REX5, GAS_COST);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX5).precompiles(),
        );
        let inputs = generate_invalid_proof_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let forwarded_gas = 500_000u64;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        // Upstream rejects the invalid proof with `BlobVerifyKzgProofFailed` →
        // `InstructionResult::PrecompileError` (NOT `PrecompileOOG`).
        assert!(
            matches!(output.result, InstructionResult::PrecompileError),
            "expected PrecompileError on invalid-proof at the cap boundary; got {:?}",
            output.result
        );
        // Halt paths skip the success-side normalization to the caller's original
        // gas_limit; after the spend_all undo, output.gas is again
        // `Gas::new(effective_gas_limit)`. effective_gas_limit was capped to
        // exactly GAS_COST, so limit() == GAS_COST and total_gas_spent() == 0.
        assert_eq!(output.gas.limit(), GAS_COST);
        assert_eq!(output.gas.total_gas_spent(), 0);
        // Joint invariant pinned here: result variant (PrecompileError) + limit value
        // (GAS_COST) + recorded compute-gas (GAS_COST) at the exact boundary where the
        // fixed-cost arm's `limit() >= GAS_COST` predicate transitions.
        assert_eq!(
            context.additional_limit.borrow().get_usage().compute_gas,
            GAS_COST,
            "at the exact-cap boundary on a verification failure, the recorded \
             compute-gas must equal GAS_COST"
        );
    }

    /// End-to-end verification that the REX5+ `Gas` normalization on `PrecompileOOG` does NOT
    /// affect caller gas accounting.
    ///
    /// Context: the REX5+ precompile wrapper re-wraps the returned `Gas` to use the
    /// caller's original `gas_limit` on success and revert paths so the caller-side
    /// `erase_cost(remaining)` refund accounts for the full forwarded budget minus
    /// actual spent. The OOG path leaves revm's vanilla `Gas::new(effective_gas_limit)`
    /// in place — `PrecompileOOG` is a halt (`return_error!`, not `return_revert!`),
    /// so neither `EthFrame::return_result` (parent CALL) nor `Handler::last_frame_result`
    /// (top-level TX) calls `erase_cost` for it. The caller is meant to burn the
    /// forwarded gas regardless of the Gas object's reported remaining.
    ///
    /// This test pins that behavior: a top-level TX directly invoking KZG with the cap
    /// forcing OOG must consume the full `tx.gas_limit`, NOT refund the user.
    #[test]
    fn test_kzg_precompile_rex5_oog_via_cap_burns_full_tx_gas() {
        use crate::MegaEvm;
        use alloy_primitives::{address, Bytes as BytesT, U256};
        use revm::context::{tx::TxEnvBuilder, BlockEnv, ContextSetters};

        let caller = address!("0000000000000000000000000000000000600000");

        let mut db = MemoryDatabase::default().account_balance(caller, U256::from(10_000_000));
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        // Tight TX-level compute-gas budget that's smaller than KZG's GAS_COST.
        let tx_limits = EvmTxRuntimeLimits {
            tx_compute_gas_limit: GAS_COST - 1,
            ..EvmTxRuntimeLimits::from_spec(MegaSpecId::REX5)
        };
        context.additional_limit =
            Rc::new(RefCell::new(AdditionalLimit::new(MegaSpecId::REX5, tx_limits)));
        context.set_block(BlockEnv { gas_limit: 1_000_000_000, ..Default::default() });
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::from(0));
            chain.operator_fee_constant = Some(U256::from(0));
        });

        // Build a valid KZG-input calldata so the precompile would proceed up to its
        // gas-cost check (which is the OOG trigger under the cap).
        let valid_kzg_input = match generate_kzg_test_input().input {
            revm::interpreter::CallInput::Bytes(b) => b,
            _ => panic!("expected Bytes"),
        };

        let tx_gas_limit = 1_000_000u64;
        let tx = TxEnvBuilder::default()
            .caller(caller)
            .call(revm::precompile::kzg_point_evaluation::ADDRESS)
            .gas_limit(tx_gas_limit)
            .gas_price(1)
            .data(valid_kzg_input)
            .build_fill();

        let mut evm = MegaEvm::new(context);
        let mut tx = crate::MegaTransaction(op_revm::OpTransaction::new(tx));
        tx.enveloped_tx = Some(BytesT::new());
        let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact ok");

        // The TX must halt (precompile OOG'd because cap forced effective_gas_limit < GAS_COST).
        assert!(!result.result.is_success(), "tx must halt: cap forced precompile OOG",);
        // Critical invariant: the receipt's gas_used reports the FULL tx.gas_limit. If
        // the Gas-normalization breaks refund accounting on the OOG path, gas_used would
        // be far smaller (sender refunded most of tx.gas_limit despite the halt).
        assert_eq!(
            result.result.tx_gas_used(),
            tx_gas_limit,
            "OOG'd precompile call must burn the full tx.gas_limit (normalization must NOT \
             leak a refund through the halt path)",
        );
    }

    #[test]
    fn test_kzg_precompile_pre_rex5_does_not_cap() {
        // Pre-REX5 (REX4): the cap is intentionally disabled for backward compatibility.
        // Even with a very small tx_compute_gas_limit, the precompile is invoked with
        // the original forwarded gas; compute-gas overshoot is detected only after the
        // fact (the existing behavior).
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
        set_tx_compute_gas_limit(&mut context, MegaSpecId::REX4, GAS_COST - 1);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX4).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let forwarded_gas = 500_000u64;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        // Precompile runs with full forwarded gas and succeeds, spending GAS_COST.
        // The post-hoc record_compute_gas call pushes the tracker over the limit, but
        // that detection happens via check_limit elsewhere, not in this path.
        assert!(matches!(output.result, InstructionResult::Return));
        assert_eq!(output.gas.total_gas_spent(), GAS_COST);
        assert_eq!(output.gas.limit(), forwarded_gas);
    }

    /// REX5 DELEGATECALL-to-KZG boundary: `target_address` differs from
    /// `bytecode_address`. Precompile dispatch and the KZG fixed-cost compute-gas
    /// arm must both key off `bytecode_address`. With an invalid proof and
    /// `forwarded_gas > GAS_COST` (no tight cap), a wrong arm would record
    /// `limit()` (the full forwarded gas) instead of `GAS_COST`.
    #[test]
    fn test_kzg_precompile_rex5_delegatecall_uses_bytecode_address() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX5).precompiles(),
        );
        let inputs = generate_invalid_proof_kzg_test_input();
        let kzg = revm::precompile::kzg_point_evaluation::ADDRESS;
        // Distinct non-precompile target so target != bytecode under DELEGATECALL.
        let target = Address::repeat_byte(0xAB);
        let forwarded_gas = 200_000u64;

        let result = precompiles_map.run(
            &mut context,
            &call_inputs_with_scheme(
                &inputs,
                target,
                kzg,
                true,
                forwarded_gas,
                CallScheme::DelegateCall,
            ),
        );
        let output = result.expect("run ok").expect("Some output");
        assert!(
            matches!(output.result, InstructionResult::PrecompileError),
            "expected PrecompileError on invalid-proof DELEGATECALL; got {:?}",
            output.result
        );
        // spend_all undo: externally-observable spent is 0; limit stays effective.
        assert_eq!(output.gas.total_gas_spent(), 0);
        assert_eq!(output.gas.limit(), forwarded_gas);
        // Fixed-cost arm fired via bytecode_address match — not the else arm
        // that would have recorded `forwarded_gas`.
        assert_eq!(
            context.additional_limit.borrow().get_usage().compute_gas,
            GAS_COST,
            "DELEGATECALL-to-KZG must charge fixed GAS_COST via bytecode_address"
        );
    }

    /// Same boundary as DELEGATECALL, for CALLCODE: target != bytecode, and the
    /// KZG fixed-cost arm must still fire.
    #[test]
    fn test_kzg_precompile_rex5_callcode_uses_bytecode_address() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX5).precompiles(),
        );
        let inputs = generate_invalid_proof_kzg_test_input();
        let kzg = revm::precompile::kzg_point_evaluation::ADDRESS;
        let target = Address::repeat_byte(0xCD);
        let forwarded_gas = 200_000u64;

        let result = precompiles_map.run(
            &mut context,
            &call_inputs_with_scheme(
                &inputs,
                target,
                kzg,
                true,
                forwarded_gas,
                CallScheme::CallCode,
            ),
        );
        let output = result.expect("run ok").expect("Some output");
        assert!(
            matches!(output.result, InstructionResult::PrecompileError),
            "expected PrecompileError on invalid-proof CALLCODE; got {:?}",
            output.result
        );
        assert_eq!(output.gas.total_gas_spent(), 0);
        assert_eq!(output.gas.limit(), forwarded_gas);
        assert_eq!(
            context.additional_limit.borrow().get_usage().compute_gas,
            GAS_COST,
            "CALLCODE-to-KZG must charge fixed GAS_COST via bytecode_address"
        );
    }

    /// REX7 KZG verification failure: the fixed fee is the work performed (enforcing) and
    /// the rest of the caller-supplied envelope is destroyed. The reported total therefore
    /// equals the forwarded envelope, not just the fixed fee.
    #[test]
    fn test_kzg_precompile_rex7_verification_failure_splits_the_parent_loss() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX7);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX7).precompiles(),
        );
        let inputs = generate_invalid_proof_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let forwarded_gas = 1_000_000u64;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        assert!(
            matches!(output.result, InstructionResult::PrecompileError),
            "expected PrecompileError on invalid-proof; got {:?}",
            output.result
        );

        let additional = context.additional_limit.borrow();
        assert_eq!(
            additional.get_usage().compute_gas,
            forwarded_gas,
            "reported compute is the whole forwarded envelope",
        );
        assert_eq!(
            additional.burned_compute_gas(),
            forwarded_gas - GAS_COST,
            "the unused envelope is destroyed, not enforced",
        );
    }

    /// REX7 generic error (blake2f malformed input): no work ran, so nothing enforces and the
    /// whole caller-supplied envelope is destroyed.
    #[test]
    fn test_blake2f_precompile_rex7_malformed_input_destroys_the_envelope() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX7);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX7).precompiles(),
        );
        let address = Address::with_last_byte(9);
        let inputs = InputsImpl {
            target_address: address,
            bytecode_address: Some(address),
            caller_address: address,
            input: revm::interpreter::CallInput::Bytes(Bytes::from(vec![0xAAu8; 32])),
            call_value: Default::default(),
        };
        let forwarded_gas = 1_000_000u64;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        assert!(
            matches!(output.result, InstructionResult::PrecompileError),
            "expected PrecompileError on wrong-length blake2f; got {:?}",
            output.result
        );

        let additional = context.additional_limit.borrow();
        assert_eq!(
            additional.get_usage().compute_gas,
            forwarded_gas,
            "reported compute is the whole forwarded envelope",
        );
        assert_eq!(
            additional.burned_compute_gas(),
            forwarded_gas,
            "generic error performed no work, so the whole envelope is destroyed",
        );
    }

    /// REX7 + REX5 cap: `effective < gas_limit`, verification still runs. Destroyed is
    /// `gas_limit − GAS_COST`, which includes the cap gap. Recording `effective − GAS_COST`
    /// instead would drop that third piece of the forwarded envelope.
    #[test]
    fn test_kzg_precompile_rex7_cap_gap_is_destroyed_not_dropped() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX7);
        set_tx_compute_gas_limit(&mut context, MegaSpecId::REX7, GAS_COST + 1_000);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX7).precompiles(),
        );
        let inputs = generate_invalid_proof_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let forwarded_gas = 500_000u64;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        assert!(
            matches!(output.result, InstructionResult::PrecompileError),
            "expected PrecompileError on invalid-proof under the cap; got {:?}",
            output.result
        );
        // Halt Gas stays on the capped effective limit.
        assert_eq!(output.gas.limit(), GAS_COST + 1_000);

        let additional = context.additional_limit.borrow();
        assert_eq!(
            additional.get_usage().compute_gas - additional.burned_compute_gas(),
            GAS_COST,
            "only the fixed fee enforces",
        );
        assert_eq!(
            additional.burned_compute_gas(),
            forwarded_gas - GAS_COST,
            "destroyed includes the cap gap (forwarded − effective) plus the unused effective \
             remainder",
        );
    }

    /// The fixed-cost arm's `record_compute_gas(GAS_COST)` must never latch a limit exceed.
    /// A latch there would make `frame_init` return a halt whose remaining gas is rescued for the
    /// sender while the same envelope has already been booked as destroyed, reporting the gas
    /// twice.
    ///
    /// The forwarded-gas cap is what rules it out, and the two halves of that coupling are pinned
    /// here at the one gas unit where they meet. With the remaining budget exactly at the fixed
    /// fee, the arm fires and lands exactly on the limit — never over it. One unit lower, the cap
    /// pushes the effective limit below the fixed fee, so the wrapper's own gas gate fires and the
    /// arm is not taken at all: there is no shape in which the recording is reached with less
    /// budget than it records.
    #[test]
    fn test_kzg_fixed_cost_arm_cannot_latch_at_the_cap_boundary() {
        for spec in [MegaSpecId::REX5, MegaSpecId::REX6, MegaSpecId::REX7] {
            let inputs = generate_invalid_proof_kzg_test_input();
            let address = revm::precompile::kzg_point_evaluation::ADDRESS;
            let forwarded_gas = 500_000u64;

            // Remaining budget exactly at the fixed fee: the tightest cap that still admits the
            // arm.
            let mut db = MemoryDatabase::default();
            let mut context = MegaContext::new(&mut db, spec);
            set_tx_compute_gas_limit(&mut context, spec, GAS_COST);
            let mut precompiles_map =
                PrecompilesMap::from_static(MegaPrecompiles::new_with_spec(spec).precompiles());
            let output = precompiles_map
                .run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas))
                .expect("run ok")
                .expect("Some output");
            assert!(
                matches!(output.result, InstructionResult::PrecompileError),
                "{spec:?}: verification must fail past the gas gate; got {:?}",
                output.result,
            );
            let additional = context.additional_limit.borrow();
            assert_eq!(
                additional.get_usage().compute_gas - additional.burned_compute_gas(),
                GAS_COST,
                "{spec:?}: the fixed-cost arm fired and only the fixed fee enforces",
            );
            assert!(
                !additional.limit_exceeded(),
                "{spec:?}: the fixed fee exactly exhausts the budget, which is not an exceed",
            );
            drop(additional);

            // One unit lower: the cap forces the wrapper's gas gate, so the fixed-cost arm is
            // never reached.
            let mut db = MemoryDatabase::default();
            let mut context = MegaContext::new(&mut db, spec);
            set_tx_compute_gas_limit(&mut context, spec, GAS_COST - 1);
            let mut precompiles_map =
                PrecompilesMap::from_static(MegaPrecompiles::new_with_spec(spec).precompiles());
            let output = precompiles_map
                .run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas))
                .expect("run ok")
                .expect("Some output");
            assert!(
                matches!(output.result, InstructionResult::PrecompileOOG),
                "{spec:?}: one unit below the fixed fee must stop at the gas gate; got {:?}",
                output.result,
            );
            assert!(
                output.gas.limit() < GAS_COST,
                "{spec:?}: the capped effective limit is what excludes the fixed-cost arm",
            );
            let additional = context.additional_limit.borrow();
            assert!(
                !additional.limit_exceeded(),
                "{spec:?}: the arm the cap excluded cannot latch either",
            );
        }
    }

    /// REX6 KZG verification failure stays on the historical single-lane recording: the
    /// fixed fee is enforcing and nothing is destroyed.
    #[test]
    fn test_kzg_precompile_rex6_verification_failure_stays_single_lane() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX6);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX6).precompiles(),
        );
        let inputs = generate_invalid_proof_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let forwarded_gas = 1_000_000u64;

        let result =
            precompiles_map.run(&mut context, &call_inputs(&inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        assert!(
            matches!(output.result, InstructionResult::PrecompileError),
            "expected PrecompileError on invalid-proof; got {:?}",
            output.result
        );

        let additional = context.additional_limit.borrow();
        assert_eq!(
            additional.get_usage().compute_gas,
            GAS_COST,
            "REX6 still records only the fixed fee",
        );
        assert_eq!(additional.burned_compute_gas(), 0, "REX6 has no destroyed lane");
    }

    // ── KZG failure split: doorway reject vs. verification under way ────────────────
    //
    // A KZG call that clears the wrapper's gas gate can still fail in two very different
    // places, and REX7 prices them differently. The probes below pin each variant on its own
    // side of the split, pin the frozen REX6 amounts for the same inputs, and pin the upstream
    // check order the split is derived from.

    /// Drives a KZG call that fails through the wired precompile table on `spec`.
    ///
    /// Returns the collapsed instruction result together with the tracker's reported and
    /// destroyed compute gas, so each probe can state the split as
    /// `reported - destroyed == enforced`.
    fn record_kzg_failure(
        spec: MegaSpecId,
        inputs: &InputsImpl,
        forwarded_gas: u64,
    ) -> (InstructionResult, u64, u64) {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, spec);
        let mut precompiles_map =
            PrecompilesMap::from_static(MegaPrecompiles::new_with_spec(spec).precompiles());
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result =
            precompiles_map.run(&mut context, &call_inputs(inputs, address, true, forwarded_gas));
        let output = result.expect("run ok").expect("Some output");
        assert!(!output.result.is_ok_or_revert(), "the probe must fail; got {:?}", output.result,);

        let additional = context.additional_limit.borrow();
        (output.result, additional.get_usage().compute_gas, additional.burned_compute_gas())
    }

    /// The premise the REX7 split rests on: upstream checks the input length before it reads
    /// the commitment, so a wrong length is a doorway reject and a mismatched versioned hash
    /// is not. If upstream ever reorders those checks, the split's "nothing was computed"
    /// claim stops holding and this turns red before the accounting probes do.
    #[test]
    fn test_kzg_halt_reasons_distinguish_the_doorway_from_verification() {
        let map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX7).precompiles(),
        );
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        for (label, inputs, expected) in [
            (
                "wrong length",
                generate_wrong_length_kzg_test_input(),
                PrecompileHalt::BlobInvalidInputLength,
            ),
            (
                "mismatched versioned hash",
                generate_mismatched_version_kzg_test_input(),
                PrecompileHalt::BlobMismatchedVersion,
            ),
            (
                "invalid proof",
                generate_invalid_proof_kzg_test_input(),
                PrecompileHalt::BlobVerifyKzgProofFailed,
            ),
        ] {
            let mut db = MemoryDatabase::default();
            let mut context = MegaContext::new(&mut db, MegaSpecId::REX7);
            let (_, halt) = run_precompile_capturing_halt(
                &map,
                &mut context.inner,
                &call_inputs(&inputs, address, true, 1_000_000),
            )
            .expect("run ok")
            .expect("Some output");
            assert_eq!(halt, Some(expected), "{label}: unexpected KZG halt reason");
        }
    }

    /// REX7 splits KZG failures by where they were raised: a doorway reject performed no work,
    /// everything past the doorway is priced at the whole fixed fee.
    #[test]
    fn test_kzg_precompile_rex7_splits_failures_by_halt_reason() {
        let forwarded = 1_000_000u64;

        let (result, reported, destroyed) = record_kzg_failure(
            MegaSpecId::REX7,
            &generate_wrong_length_kzg_test_input(),
            forwarded,
        );
        assert!(
            matches!(result, InstructionResult::PrecompileError),
            "a wrong-length reject is not an out-of-gas; got {result:?}",
        );
        assert_eq!(reported, forwarded, "reported compute is the whole forwarded envelope");
        assert_eq!(destroyed, forwarded, "a doorway reject performed no work");
        assert_eq!(reported - destroyed, 0, "so nothing enforces");

        for (label, inputs) in [
            ("mismatched versioned hash", generate_mismatched_version_kzg_test_input()),
            ("invalid proof", generate_invalid_proof_kzg_test_input()),
        ] {
            let (_, reported, destroyed) = record_kzg_failure(MegaSpecId::REX7, &inputs, forwarded);
            assert_eq!(reported, forwarded, "{label}: reported compute is the whole envelope");
            assert_eq!(
                reported - destroyed,
                GAS_COST,
                "{label}: verification was under way, so the fixed fee is the work performed",
            );
        }
    }

    /// REX6 pin for the same three inputs: the frozen spec charges the fixed fee for every KZG
    /// failure it sees, doorway rejects included, and has no destroyed lane. Any drift in the
    /// REX7 arm that leaked back into the shared code path shows up here.
    #[test]
    fn test_kzg_precompile_rex6_charges_the_fixed_cost_for_every_failure() {
        let forwarded = 1_000_000u64;
        for (label, inputs) in [
            ("wrong length", generate_wrong_length_kzg_test_input()),
            ("mismatched versioned hash", generate_mismatched_version_kzg_test_input()),
            ("invalid proof", generate_invalid_proof_kzg_test_input()),
        ] {
            let (_, reported, destroyed) = record_kzg_failure(MegaSpecId::REX6, &inputs, forwarded);
            assert_eq!(reported, GAS_COST, "{label}: REX6 records only the fixed fee");
            assert_eq!(destroyed, 0, "{label}: REX6 has no destroyed lane");
        }
    }

    /// The knife edge of the fixed-cost arm's `limit() >= GAS_COST` predicate, taken on a
    /// doorway-rejected input.
    ///
    /// At exactly `GAS_COST` the wrapper's `gas_limit < GAS_COST` pre-check passes by one, the
    /// call reaches the length doorway, and the split books the envelope as destroyed. One gas
    /// lower the wrapper halts out of gas before the doorway, which is the generic error arm.
    /// Both arms destroy the same envelope under REX7, so the edge is invisible there — but
    /// REX6 charges the fixed fee on one side and the (equal) forwarded limit on the other, and
    /// pinning both keeps the predicate's boundary from drifting unnoticed.
    #[test]
    fn test_kzg_precompile_wrong_length_at_the_fixed_cost_boundary() {
        let inputs = generate_wrong_length_kzg_test_input();

        let (result, reported, destroyed) = record_kzg_failure(MegaSpecId::REX7, &inputs, GAS_COST);
        assert!(
            matches!(result, InstructionResult::PrecompileError),
            "at exactly GAS_COST the wrapper's gas gate passes; got {result:?}",
        );
        assert_eq!(reported, GAS_COST);
        assert_eq!(destroyed, GAS_COST, "the doorway reject destroys the whole envelope");

        let (result, reported, destroyed) =
            record_kzg_failure(MegaSpecId::REX7, &inputs, GAS_COST - 1);
        assert!(
            matches!(result, InstructionResult::PrecompileOOG),
            "one gas below GAS_COST the wrapper's gate halts first; got {result:?}",
        );
        assert_eq!(reported, GAS_COST - 1);
        assert_eq!(destroyed, GAS_COST - 1, "the generic arm destroys the envelope too");

        // Frozen side of the same edge.
        let (_, reported, destroyed) = record_kzg_failure(MegaSpecId::REX6, &inputs, GAS_COST);
        assert_eq!(reported, GAS_COST, "REX6 charges the fixed fee at the boundary");
        assert_eq!(destroyed, 0);
        let (_, reported, destroyed) = record_kzg_failure(MegaSpecId::REX6, &inputs, GAS_COST - 1);
        assert_eq!(reported, GAS_COST - 1, "REX6 charges the forwarded limit below it");
        assert_eq!(destroyed, 0);
    }

    // ── seam parity: the mirror must still be the upstream call ──────────────────────
    //
    // `run_precompile_capturing_halt` reimplements alloy-evm's `PrecompilesMap::run` so the
    // precompile's halt reason survives the conversion. That is only safe while the
    // reimplementation produces exactly what delegating would have produced, and nothing in
    // the type system holds it there — an alloy-evm bump can change the upstream body
    // underneath it. The probe below drives both paths over the same inputs and compares the
    // whole `InterpreterResult`, so an upstream change the mirror has not picked up turns red
    // here rather than silently shifting consensus.

    /// Address the parity matrix installs a dynamic precompile at. Outside the builtin table,
    /// so with the dynamic precompile absent the same case doubles as a dispatch-miss probe.
    const PARITY_DYN_ADDRESS: Address = Address::with_last_byte(0x7e);

    /// Builds the table both paths run against.
    ///
    /// `with_dynamic` installs a reverting precompile at [`PARITY_DYN_ADDRESS`], which also
    /// flips the map into its dynamic representation — the `PrecompilesMap::get` branch the
    /// builtin table never reaches, and the only way to produce a reverting precompile at all
    /// (no builtin returns `PrecompileStatus::Revert`).
    ///
    /// It echoes the forwarded call value into its output and the forwarded reservoir into its
    /// gas, so both become observable in the compared `InterpreterResult`. No builtin precompile
    /// reads the call value, so without this the value cases in the matrix would only prove the
    /// two paths agree on an input neither of them can act on.
    fn parity_precompiles(with_dynamic: bool) -> PrecompilesMap {
        let mut map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::REX7).precompiles(),
        );
        if with_dynamic {
            map.apply_precompile(&PARITY_DYN_ADDRESS, |_| {
                Some(DynPrecompile::new(PrecompileId::Custom("parity-revert".into()), |input| {
                    let mut echoed = Vec::from(b"reverted".as_slice());
                    echoed.extend_from_slice(&input.value.to_be_bytes::<32>());
                    Ok(PrecompileOutput::revert(
                        input.gas / 4,
                        Bytes::from(echoed),
                        input.reservoir,
                    ))
                }))
            });
        }
        map
    }

    /// Sets the EIP-8037 state-gas reservoir on an already-built [`CallInputs`].
    ///
    /// `reservoir` and `value` are two of the nine `PrecompileInput` fields the mirror forwards,
    /// and no builtin precompile reads either — so an upstream step keyed on one of them would
    /// pass a matrix that leaves both at zero. The cases built with these helpers vary them
    /// instead, on both an `is_ok_or_revert` arm and a failing arm.
    fn with_reservoir(mut inputs: CallInputs, reservoir: u64) -> CallInputs {
        inputs.reservoir = reservoir;
        inputs
    }

    /// Sets the call value on an already-built [`CallInputs`], as a non-static CALL.
    fn with_value(mut inputs: CallInputs, value: U256) -> CallInputs {
        inputs.value = CallValue::Transfer(value);
        inputs.is_static = false;
        inputs
    }

    /// One case per distinguishable path through the upstream body: dispatch miss, revert,
    /// success, each KZG failure variant, the wrapper's own gas gate, a generic
    /// malformed-input halt, an out-of-gas halt, and the DELEGATECALL bytecode-vs-target split.
    /// Each of the two forwarded inputs no builtin consults — the state-gas reservoir and the
    /// call value — additionally appears on a succeeding and on a failing case.
    fn parity_cases() -> Vec<(&'static str, CallInputs)> {
        let kzg = revm::precompile::kzg_point_evaluation::ADDRESS;
        let ecrecover = Address::with_last_byte(1);
        let identity = Address::with_last_byte(4);
        let blake2f = Address::with_last_byte(9);
        let not_a_precompile = Address::with_last_byte(0xff);

        let plain = |address: Address, data: Vec<u8>| InputsImpl {
            target_address: address,
            bytecode_address: Some(address),
            caller_address: address,
            input: revm::interpreter::CallInput::Bytes(Bytes::from(data)),
            call_value: Default::default(),
        };

        vec![
            (
                "dispatch miss",
                call_inputs(
                    &plain(not_a_precompile, vec![1, 2, 3]),
                    not_a_precompile,
                    true,
                    1_000_000,
                ),
            ),
            (
                "dynamic revert",
                call_inputs(
                    &plain(PARITY_DYN_ADDRESS, vec![7; 8]),
                    PARITY_DYN_ADDRESS,
                    true,
                    1_000_000,
                ),
            ),
            ("success", call_inputs(&plain(identity, vec![0xAB; 64]), identity, true, 1_000_000)),
            ("kzg success", call_inputs(&generate_kzg_test_input(), kzg, true, 1_000_000)),
            (
                "kzg wrong length",
                call_inputs(&generate_wrong_length_kzg_test_input(), kzg, true, 1_000_000),
            ),
            (
                "kzg mismatched versioned hash",
                call_inputs(&generate_mismatched_version_kzg_test_input(), kzg, true, 1_000_000),
            ),
            (
                "kzg invalid proof",
                call_inputs(&generate_invalid_proof_kzg_test_input(), kzg, true, 1_000_000),
            ),
            (
                "kzg below the fixed cost",
                call_inputs(&generate_kzg_test_input(), kzg, true, GAS_COST - 1),
            ),
            (
                "blake2f malformed input",
                call_inputs(&plain(blake2f, vec![0xAA; 32]), blake2f, true, 1_000_000),
            ),
            (
                "ecrecover out of gas",
                call_inputs(&plain(ecrecover, vec![0u8; 128]), ecrecover, true, 100),
            ),
            (
                "delegatecall to kzg",
                call_inputs_with_scheme(
                    &generate_invalid_proof_kzg_test_input(),
                    Address::repeat_byte(0xAB),
                    kzg,
                    true,
                    200_000,
                    CallScheme::DelegateCall,
                ),
            ),
            (
                "success with reservoir",
                with_reservoir(
                    call_inputs(&plain(identity, vec![0xCD; 96]), identity, true, 1_000_000),
                    250_000,
                ),
            ),
            (
                // The dynamic precompile echoes `input.reservoir` straight into its output, so
                // this is the one case where a mirrored reservoir is directly observable in the
                // returned `InterpreterResult` rather than only forwarded.
                "dynamic revert with reservoir",
                with_reservoir(
                    call_inputs(
                        &plain(PARITY_DYN_ADDRESS, vec![7; 8]),
                        PARITY_DYN_ADDRESS,
                        true,
                        1_000_000,
                    ),
                    250_000,
                ),
            ),
            (
                "kzg invalid proof with reservoir",
                with_reservoir(
                    call_inputs(&generate_invalid_proof_kzg_test_input(), kzg, true, 1_000_000),
                    250_000,
                ),
            ),
            (
                "success with value",
                with_value(
                    call_inputs(&plain(identity, vec![0xEF; 96]), identity, false, 1_000_000),
                    U256::from(7u64),
                ),
            ),
            (
                // The dynamic precompile echoes `input.value` into its output bytes, so this is
                // the case that observes a mirrored call value rather than only forwarding it.
                "dynamic revert with value",
                with_value(
                    call_inputs(
                        &plain(PARITY_DYN_ADDRESS, vec![7; 8]),
                        PARITY_DYN_ADDRESS,
                        false,
                        1_000_000,
                    ),
                    U256::from(7u64),
                ),
            ),
            (
                "blake2f malformed input with value",
                with_value(
                    call_inputs(&plain(blake2f, vec![0xAA; 32]), blake2f, false, 1_000_000),
                    U256::from(7u64),
                ),
            ),
        ]
    }

    #[test]
    fn test_precompile_mirror_matches_upstream_delegation() {
        for with_dynamic in [false, true] {
            for (label, inputs) in parity_cases() {
                // Mirror path.
                let mut mirror_db = MemoryDatabase::default();
                let mut mirror_ctx = MegaContext::new(&mut mirror_db, MegaSpecId::REX7);
                let mirror_map = parity_precompiles(with_dynamic);
                let mirrored =
                    run_precompile_capturing_halt(&mirror_map, &mut mirror_ctx.inner, &inputs)
                        .expect("the mirror must not fail fatally");

                // Delegated path: alloy-evm's own provider impl, reached exactly as the
                // pre-mirror code reached it.
                let mut delegate_db = MemoryDatabase::default();
                let mut delegate_ctx = MegaContext::new(&mut delegate_db, MegaSpecId::REX7);
                let mut delegate_map = parity_precompiles(with_dynamic);
                let delegated = PrecompileProvider::<crate::MegaInnerContext<_>>::run(
                    &mut delegate_map,
                    &mut delegate_ctx.inner,
                    &inputs,
                )
                .expect("delegation must not fail fatally");

                match (mirrored, delegated) {
                    (None, None) => {}
                    (Some((mirror_result, halt)), Some(delegate_result)) => {
                        assert_eq!(
                            mirror_result.result, delegate_result.result,
                            "{label} (dynamic table: {with_dynamic}): instruction result",
                        );
                        assert_eq!(
                            mirror_result.output, delegate_result.output,
                            "{label} (dynamic table: {with_dynamic}): output bytes",
                        );
                        assert_eq!(
                            mirror_result.gas, delegate_result.gas,
                            "{label} (dynamic table: {with_dynamic}): gas",
                        );
                        // Whole-struct equality also covers `Gas`'s private tracker and memory
                        // fields, which the accessors above do not reach.
                        assert_eq!(
                            mirror_result, delegate_result,
                            "{label} (dynamic table: {with_dynamic})",
                        );
                        // The captured signal must agree with the code the conversion collapsed
                        // it into; a halt reason next to a success would mean the mirror read
                        // the wrong output.
                        assert_eq!(
                            halt.is_some(),
                            !mirror_result.result.is_ok_or_revert(),
                            "{label} (dynamic table: {with_dynamic}): captured halt reason must \
                             match the collapsed result",
                        );
                    }
                    (mirror, delegate) => panic!(
                        "{label} (dynamic table: {with_dynamic}): dispatch disagreed — mirror \
                         returned {:?}, delegation returned {:?}",
                        mirror.is_some(),
                        delegate.is_some(),
                    ),
                }
            }
        }
    }

    /// Direct unit coverage for `PrecompileProvider::contains` on the Mega
    /// `PrecompilesMap` wrapper.
    ///
    /// The method is a thin forwarder; both always-true and always-false mutants
    /// of its body previously survived because no test consulted the return value.
    /// Pin known precompile addresses as true and a non-precompile as false under
    /// every MegaETH-relevant precompile table (`MINI_REX` / `REX` share the same
    /// Isthmus-era set for the classic 0x01-0x0a range plus KZG).
    /// Direct pin of `MegaPrecompiles::set_spec` return value and rebuild behaviour.
    ///
    /// Kills cargo-mutants survivors that force the method to always return `false`,
    /// always return `true`, or invert the `spec == self.spec` equality check.
    ///
    /// `MegaPrecompiles` is wired for contexts whose `Cfg::Spec = MegaSpecId`. `MegaContext`
    /// itself stores `CfgEnv<OpSpecId>`, so the provider trait is exercised through a plain
    /// revm `Context` with `CfgEnv<MegaSpecId>`.
    #[test]
    fn test_mega_precompiles_set_spec_return_value_and_rebuild() {
        use revm::{
            context::{BlockEnv, CfgEnv, Context, TxEnv},
            database::EmptyDB,
        };

        type Ctx = Context<BlockEnv, TxEnv, CfgEnv<MegaSpecId>, EmptyDB>;

        let mut precompiles = MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX);

        // Same spec: must report no change and leave the table alone.
        let changed_same =
            PrecompileProvider::<Ctx>::set_spec(&mut precompiles, MegaSpecId::MINI_REX);
        assert!(!changed_same, "set_spec(same) must return false — no rebuild needed");
        assert_eq!(
            precompiles.spec,
            MegaSpecId::MINI_REX,
            "set_spec(same) must leave the stored spec unchanged",
        );

        // Different spec: must report a change and rebuild.
        let changed_diff = PrecompileProvider::<Ctx>::set_spec(&mut precompiles, MegaSpecId::REX5);
        assert!(changed_diff, "set_spec(different) must return true — table was rebuilt");
        assert_eq!(
            precompiles.spec,
            MegaSpecId::REX5,
            "set_spec(different) must store the new spec",
        );

        // Idempotent second call with the new spec.
        let changed_again = PrecompileProvider::<Ctx>::set_spec(&mut precompiles, MegaSpecId::REX5);
        assert!(!changed_again, "set_spec(same after rebuild) must return false");
    }

    #[test]
    fn test_precompiles_map_contains_known_and_unknown_addresses() {
        // Classic Ethereum precompiles present in every MegaETH table.
        const ECRECOVER: Address =
            alloy_primitives::address!("0000000000000000000000000000000000000001");
        const SHA256: Address =
            alloy_primitives::address!("0000000000000000000000000000000000000002");
        const IDENTITY: Address =
            alloy_primitives::address!("0000000000000000000000000000000000000004");
        // KZG point evaluation (EIP-4844) — overridden by MegaPrecompiles.
        let kzg = revm::precompile::kzg_point_evaluation::ADDRESS;
        // Address outside the precompile range.
        const NON_PRECOMPILE: Address =
            alloy_primitives::address!("00000000000000000000000000000000000000ff");

        type Ctx<'a> = MegaContext<&'a mut MemoryDatabase, crate::EmptyExternalEnv>;

        for spec in [MegaSpecId::MINI_REX, MegaSpecId::REX, MegaSpecId::REX5, MegaSpecId::REX6] {
            let precompiles_map =
                PrecompilesMap::from_static(MegaPrecompiles::new_with_spec(spec).precompiles());

            assert!(
                PrecompileProvider::<Ctx<'_>>::contains(&precompiles_map, &ECRECOVER),
                "{spec:?}: ecrecover (0x01) must be reported as a precompile",
            );
            assert!(
                PrecompileProvider::<Ctx<'_>>::contains(&precompiles_map, &SHA256),
                "{spec:?}: sha256 (0x02) must be reported as a precompile",
            );
            assert!(
                PrecompileProvider::<Ctx<'_>>::contains(&precompiles_map, &IDENTITY),
                "{spec:?}: identity (0x04) must be reported as a precompile",
            );
            assert!(
                PrecompileProvider::<Ctx<'_>>::contains(&precompiles_map, &kzg),
                "{spec:?}: KZG point evaluation must be reported as a precompile",
            );
            assert!(
                !PrecompileProvider::<Ctx<'_>>::contains(&precompiles_map, &NON_PRECOMPILE),
                "{spec:?}: 0xff must not be reported as a precompile",
            );
        }
    }

    // ── frozen legacy Osaka ModExp: check order and boundaries ──────────────────────
    //
    // The wrapper's contract is an exact check order — minimum-gas, header parse, EIP-7823
    // input-size limits, then the zero-base/zero-modulus flat charge — and every step's
    // boundary is consensus-visible. The probes below drive the wired precompile directly, so
    // each boundary is pinned by its own sub-millisecond case rather than only through a
    // transaction.

    /// The EIP-7823 input-size limit, shared by the wrapper and the upstream implementation.
    const MODEXP_SIZE_LIMIT: u64 = eip7823::INPUT_SIZE_LIMIT as u64;

    /// A gas limit above every charge the probes below can incur.
    const MODEXP_AMPLE_GAS: u64 = 1_000_000;

    /// Builds `ModExp` calldata carrying only the 96-byte header (the three big-endian
    /// lengths). The base, exponent and modulus bodies are absent, so the implementation
    /// right-pads them with zero bytes.
    fn modexp_header(base_len: u64, exp_len: u64, mod_len: u64) -> Vec<u8> {
        let mut input = Vec::with_capacity(96);
        for len in [base_len, exp_len, mod_len] {
            input.extend_from_slice(&U256::from(len).to_be_bytes::<32>());
        }
        input
    }

    /// Runs the frozen legacy Osaka `ModExp` on a header-only input.
    fn run_modexp(base_len: u64, exp_len: u64, mod_len: u64, gas_limit: u64) -> PrecompileOutput {
        modexp::OSAKA_LEGACY
            .execute(&modexp_header(base_len, exp_len, mod_len), gas_limit, 0)
            .expect("the legacy ModExp wrapper reports failures as halts, never as an error")
    }

    /// The upstream EIP-7883 formula charge for the given lengths with a zero exponent head.
    fn modexp_formula_gas(base_len: u64, exp_len: u64, mod_len: u64) -> u64 {
        revm::precompile::modexp::osaka_gas_calc(base_len, exp_len, mod_len, &U256::ZERO)
    }

    /// A gas limit below the Osaka minimum halts before anything else is inspected.
    #[test]
    fn test_modexp_gas_limit_below_min_gas_halts() {
        let output = run_modexp(0, 64, 0, modexp::MIN_GAS - 1);
        assert_eq!(
            output.status,
            PrecompileStatus::Halt(PrecompileHalt::OutOfGas),
            "a gas limit one below MIN_GAS must halt out of gas",
        );
    }

    /// The minimum-gas check is exclusive at its boundary: a gas limit of exactly `MIN_GAS`
    /// funds the call, and the zero-base/zero-modulus short-circuit charges all of it.
    #[test]
    fn test_modexp_gas_limit_exactly_min_gas_is_funded() {
        let output = run_modexp(0, 64, 0, modexp::MIN_GAS);
        assert_eq!(
            output.status,
            PrecompileStatus::Success,
            "a gas limit of exactly MIN_GAS must fund the call",
        );
        assert_eq!(output.gas_used, modexp::MIN_GAS);
        assert!(output.bytes.is_empty(), "a zero-length modulus returns no output");
    }

    /// Zero base and zero modulus lengths are charged the flat minimum, not the EIP-7883
    /// formula cost — the historical behavior every spec is pinned to.
    #[test]
    fn test_modexp_zero_base_and_mod_charge_flat_min_gas() {
        let output = run_modexp(0, 64, 0, MODEXP_AMPLE_GAS);
        assert_eq!(output.status, PrecompileStatus::Success);
        assert_eq!(output.gas_used, modexp::MIN_GAS, "the short-circuit charge must be flat");
        assert!(output.bytes.is_empty());
        assert!(
            modexp_formula_gas(0, 64, 0) > modexp::MIN_GAS,
            "the formula charge must exceed the flat one, or this probe proves nothing",
        );
    }

    /// The flat charge needs *both* lengths to be zero. A zero base with a non-zero modulus
    /// takes the formula path, which returns a modulus-sized output instead of no output.
    #[test]
    fn test_modexp_short_circuit_needs_both_base_and_mod_zero() {
        let output = run_modexp(0, 1, 32, MODEXP_AMPLE_GAS);
        assert_eq!(output.status, PrecompileStatus::Success);
        assert_eq!(
            output.bytes.len(),
            32,
            "a non-zero modulus length must produce a modulus-sized output, not the flat \
             short-circuit's empty one",
        );
    }

    /// The EIP-7823 base-length limit is inclusive: a length of exactly the limit is accepted
    /// and priced by the formula.
    #[test]
    fn test_modexp_base_len_at_size_limit_is_accepted() {
        let output = run_modexp(MODEXP_SIZE_LIMIT, 0, 0, MODEXP_AMPLE_GAS);
        assert_eq!(
            output.status,
            PrecompileStatus::Success,
            "a base length of exactly the EIP-7823 limit must be accepted",
        );
        let formula_gas = modexp_formula_gas(MODEXP_SIZE_LIMIT, 0, 0);
        assert_eq!(output.gas_used, formula_gas);
        assert!(
            formula_gas > modexp::MIN_GAS,
            "a limit-sized base must price above the flat charge",
        );
    }

    /// The EIP-7823 modulus-length limit is inclusive, and the accepted call is priced by the
    /// formula rather than the flat charge.
    #[test]
    fn test_modexp_mod_len_at_size_limit_is_accepted() {
        let output = run_modexp(0, 0, MODEXP_SIZE_LIMIT, MODEXP_AMPLE_GAS);
        assert_eq!(
            output.status,
            PrecompileStatus::Success,
            "a modulus length of exactly the EIP-7823 limit must be accepted",
        );
        let formula_gas = modexp_formula_gas(0, 0, MODEXP_SIZE_LIMIT);
        assert_eq!(output.gas_used, formula_gas);
        assert!(formula_gas > modexp::MIN_GAS);
        assert_eq!(
            output.bytes.len(),
            MODEXP_SIZE_LIMIT as usize,
            "the output is padded to the modulus length",
        );
    }

    /// The EIP-7823 exponent-length limit is inclusive: at exactly the limit the call is
    /// accepted and reaches the zero-base/zero-modulus short-circuit.
    #[test]
    fn test_modexp_exp_len_at_size_limit_is_accepted() {
        let output = run_modexp(0, MODEXP_SIZE_LIMIT, 0, MODEXP_AMPLE_GAS);
        assert_eq!(
            output.status,
            PrecompileStatus::Success,
            "an exponent length of exactly the EIP-7823 limit must be accepted",
        );
        assert_eq!(output.gas_used, modexp::MIN_GAS);
    }

    /// Ordinary short lengths sit below every EIP-7823 limit and are accepted.
    #[test]
    fn test_modexp_small_lengths_are_accepted() {
        let output = run_modexp(1, 1, 1, MODEXP_AMPLE_GAS);
        assert_eq!(
            output.status,
            PrecompileStatus::Success,
            "single-byte base, exponent and modulus must be accepted",
        );
        assert_eq!(output.bytes.len(), 1);
    }

    /// The size limits are checked *before* the flat-charge short-circuit: an oversized
    /// exponent length halts even though base and modulus lengths are zero.
    #[test]
    fn test_modexp_oversized_exp_len_halts_before_short_circuit() {
        let output = run_modexp(0, MODEXP_SIZE_LIMIT + 1, 0, MODEXP_AMPLE_GAS);
        assert_eq!(
            output.status,
            PrecompileStatus::Halt(PrecompileHalt::ModexpEip7823LimitSize),
            "an exponent length above the limit must halt, not take the short-circuit",
        );
    }

    /// Base and modulus lengths above the limit are rejected the same way.
    #[test]
    fn test_modexp_oversized_base_or_mod_len_halts() {
        for (base_len, mod_len) in [(MODEXP_SIZE_LIMIT + 1, 0), (0, MODEXP_SIZE_LIMIT + 1)] {
            let output = run_modexp(base_len, 0, mod_len, MODEXP_AMPLE_GAS);
            assert_eq!(
                output.status,
                PrecompileStatus::Halt(PrecompileHalt::ModexpEip7823LimitSize),
                "base_len={base_len}, mod_len={mod_len} exceeds the EIP-7823 limit",
            );
        }
    }
}
