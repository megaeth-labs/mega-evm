//! Custom precompiles for `MegaETH` EVM.
//!
//! This module provides custom precompile implementations with `MegaETH`-specific
//! gas cost overrides.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{boxed::Box, string::String, sync::Arc};

use crate::{ExternalEnvTypes, MegaContext, MegaSpecId};
use alloy_evm::{
    precompiles::{DynPrecompile, PrecompilesMap},
    Database,
};
use delegate::delegate;
use once_cell::race::OnceBox;
use op_revm::OpSpecId;
use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, Gas, InterpreterResult},
    precompile::Precompiles,
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

        // `PrecompilesMap` implements the provider for revm's plain `Context`; delegate to the
        // context `MegaContext` wraps.
        let maybe_output = PrecompileProvider::<crate::MegaInnerContext<DB>>::run(
            self,
            &mut context.inner,
            inputs,
        )?;

        Ok(maybe_output.map(|mut output| {
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
            //   `limit() >= GAS_COST`): record the declared fixed cost. revm's `PrecompileError`
            //   halt still consumes the parent's forwarded `gas_limit` from the EVM-gas meter, so
            //   compute-gas here is intentionally a separate number from the EVM-gas burn. Address
            //   match uses `bytecode_address` (see above) so DELEGATECALL/CALLCODE to KZG still hit
            //   this arm.
            // * All other error paths (non-KZG, or KZG with `limit() < GAS_COST` meaning the
            //   wrapper's pre-check itself OOG'd before verification could run): after the
            //   spend_all undo above, `total_gas_spent() == 0` again. The parent still permanently
            //   loses the forwarded amount, so record `limit()` to match the EVM-gas burn (do not
            //   use `total_gas_spent()` here — the structural `limit()` is the intentional charge,
            //   independent of whether upstream spend_all'd).
            if is_rex5_enabled {
                let compute_gas = if output.result.is_ok_or_revert() {
                    output.gas.total_gas_spent()
                } else if address == kzg_point_evaluation::ADDRESS &&
                    output.gas.limit() >= kzg_point_evaluation::GAS_COST
                {
                    // KZG with the wrapper's `gas_limit < GAS_COST` pre-check passed: upstream
                    // verification ran and returned a non-OOG error
                    // (`BlobInvalidInputLength` / `BlobMismatchedVersion` /
                    // `BlobVerifyKzgProofFailed`). Charge the fixed cost regardless of which
                    // error variant fired. Using the structural predicate
                    // (`limit() >= GAS_COST`) instead of an error-variant match keeps this arm
                    // robust against upstream KZG adding new non-OOG variants.
                    kzg_point_evaluation::GAS_COST
                } else {
                    output.gas.limit()
                };
                context.additional_limit.borrow_mut().record_compute_gas(compute_gas);
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

    use super::{kzg_point_evaluation::GAS_COST, mini_rex, modexp, rex, MegaPrecompiles};
    use crate::{
        test_utils::MemoryDatabase, AdditionalLimit, EvmTxRuntimeLimits, MegaContext, MegaSpecId,
    };
    use alloy_evm::precompiles::PrecompilesMap;
    use alloy_primitives::{Address, Bytes, U256};
    use core::cell::RefCell;
    use revm::{
        handler::PrecompileProvider,
        interpreter::{CallInputs, CallScheme, CallValue, InputsImpl, InstructionResult},
        precompile::{PrecompileHalt, PrecompileOutput, PrecompileStatus},
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
