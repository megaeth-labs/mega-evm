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
use op_revm::{OpContext, OpSpecId};
use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{Gas, InputsImpl, InterpreterResult},
    precompile::Precompiles,
    primitives::{Address, HashMap},
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
            MegaSpecId::REX5 => rex(),
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
    mini_rex()
}

/// Precompiles for the `MINI_REX` spec.
pub fn mini_rex() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = op_revm::precompiles::isthmus().clone();
        // Use the OSAKA modexp precompile for MINI_REX
        precompiles
            .extend([revm::precompile::modexp::OSAKA, kzg_point_evaluation::KZG_POINT_EVALUATION]);
        Box::new(precompiles)
    })
}

/// Customized KZG point evaluation precompile module.
pub mod kzg_point_evaluation {
    use revm::{
        precompile::{PrecompileError, PrecompileWithAddress},
        primitives::Address,
    };

    /// Address of the KZG point evaluation precompile (re-export of the upstream
    /// constant). Local re-export so call-sites can refer to `GAS_COST` and
    /// `ADDRESS` through the same module path.
    pub const ADDRESS: Address = revm::precompile::kzg_point_evaluation::ADDRESS;

    /// Gas cost for the KZG point evaluation precompile.
    pub const GAS_COST: u64 = 100_000;

    /// KZG point evaluation precompile. This is the modified version of the original precompile
    /// with a custom gas cost.
    pub const KZG_POINT_EVALUATION: PrecompileWithAddress =
        PrecompileWithAddress(ADDRESS, |input, gas_limit| {
            if gas_limit < GAS_COST {
                return Err(PrecompileError::OutOfGas);
            }
            let mut output = revm::precompile::kzg_point_evaluation::run(input, gas_limit)?;
            output.gas_used = GAS_COST;
            Ok(output)
        });
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
                address: &Address,
                inputs: &InputsImpl,
                is_static: bool,
                gas_limit: u64,
            ) -> Result<Option<Self::Output>, String>;
            fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>>;
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
        address: &Address,
        inputs: &InputsImpl,
        is_static: bool,
        gas_limit: u64,
    ) -> Result<Option<Self::Output>, String> {
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

        let maybe_output = PrecompileProvider::<OpContext<DB>>::run(
            self,
            context,
            address,
            inputs,
            is_static,
            effective_gas_limit,
        )?;

        Ok(maybe_output.map(|mut output| {
            // Normalize the returned Gas back to the caller's original `gas_limit` budget
            // ONLY on `is_ok_or_revert` paths — those are the paths where the caller's
            // refund logic (`EthFrame::return_result` and `Handler::last_frame_result`,
            // both gated on `is_ok_or_revert()`) actually consumes `gas.remaining()`.
            //
            // Halt paths (`PrecompileOOG`, `PrecompileError`) skip the refund entirely:
            // the parent burns the full forwarded `gas_limit` regardless of the Gas
            // object's reported `remaining`. Leaving revm's vanilla `Gas::new(effective)`
            // on those paths keeps the post-cap halt result semantically identical to
            // an uncapped precompile OOG, matches what tracers / inspectors expect, and
            // makes the "cap forced an OOG" behavior locally indistinguishable from
            // "user just forwarded too little gas".
            //
            // The normalize preserves the precompile's `refunded()` value (today every
            // standard revm precompile leaves refunded == 0, but custom precompiles
            // injected via `PrecompilesMap` may set a refund, and a future EIP
            // precompile could also). Memory expansion gas is not preserved because
            // precompiles do not allocate from `Gas::memory` — memory expansion is
            // the caller-interpreter's responsibility and is settled before the
            // precompile is invoked.
            if is_rex5_enabled && output.result.is_ok_or_revert() {
                let spent = output.gas.spent();
                let refunded = output.gas.refunded();
                let mut normalized = Gas::new(gas_limit);
                normalized.set_spent(spent);
                normalized.record_refund(refunded);
                output.gas = normalized;
            }
            // Compute-gas recording on REX5+:
            //
            // * Success / revert: revm called `record_cost`, so `spent()` already reflects the
            //   actually-consumed amount. Use it.
            // * Fixed-cost precompile that reached past its wrapper gas pre-check (today: KZG with
            //   `limit() >= GAS_COST`): record the declared fixed cost. revm's `PrecompileError`
            //   halt still consumes the parent's forwarded `gas_limit` from the EVM-gas meter, so
            //   compute-gas here is intentionally a separate number from the EVM-gas burn.
            // * All other error paths (non-KZG, or KZG with `limit() < GAS_COST` meaning the
            //   wrapper's pre-check itself OOG'd before verification could run): revm did not call
            //   `record_cost`, so `spent() == 0`. The parent still permanently loses the forwarded
            //   amount, so record `limit()` to match the EVM-gas burn.
            if is_rex5_enabled {
                let compute_gas = if output.result.is_ok_or_revert() {
                    output.gas.spent()
                } else if address == &kzg_point_evaluation::ADDRESS &&
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
                context.additional_limit.borrow_mut().record_compute_gas(output.gas.spent());
            }
            output
        }))
    }

    #[inline]
    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        PrecompileProvider::<OpContext<DB>>::warm_addresses(self)
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        PrecompileProvider::<OpContext<DB>>::contains(self, address)
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

    use super::{kzg_point_evaluation::GAS_COST, MegaPrecompiles};
    use crate::{
        test_utils::MemoryDatabase, AdditionalLimit, EvmTxRuntimeLimits, MegaContext, MegaSpecId,
    };
    use alloy_evm::precompiles::PrecompilesMap;
    use alloy_primitives::Bytes;
    use core::cell::RefCell;
    use revm::{
        handler::PrecompileProvider,
        interpreter::{InputsImpl, InstructionResult},
    };
    use sha2::{Digest, Sha256};

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
    fn test_kzg_precompile_sufficient_gas() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result = precompiles_map.run(&mut context, &address, &inputs, true, 200_000);
        assert!(result.is_ok(), "Precompile should succeed with sufficient gas");
        let output = result.unwrap().unwrap();
        assert!(matches!(output.result, InstructionResult::Return), "Result should be Return");
        assert_eq!(output.gas.spent(), GAS_COST);
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, GAS_COST);
        assert!(result.is_ok(), "Precompile should succeed with exact GAS_COST");
        let output = result.unwrap().unwrap();
        assert!(matches!(output.result, InstructionResult::Return), "Result should be Return");
        assert_eq!(output.gas.spent(), GAS_COST);
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
        let result = precompiles_map.run(&mut context, &address, &inputs, true, 200_000);
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::Return));
        assert_eq!(output.gas.spent(), GAS_COST);
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, 50_000);
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, GAS_COST - 1);
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, 0);
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, forwarded_gas);
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::Return));
        // Spent reflects the precompile's actual cost.
        assert_eq!(output.gas.spent(), GAS_COST);
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, 1_000_000);
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::PrecompileOOG));
        assert_eq!(output.gas.spent(), 0);
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, forwarded_gas);
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::Return));
        assert_eq!(output.gas.spent(), GAS_COST);
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, forwarded_gas);
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::Return));
        assert_eq!(output.gas.spent(), GAS_COST);
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, forwarded_gas);
        let output = result.expect("run ok").expect("Some output");
        // Upstream rejects the invalid proof with `BlobVerifyKzgProofFailed` →
        // `InstructionResult::PrecompileError` (NOT `PrecompileOOG`).
        assert!(
            matches!(output.result, InstructionResult::PrecompileError),
            "expected PrecompileError on invalid-proof at the cap boundary; got {:?}",
            output.result
        );
        // Halt paths skip the success-side normalization, so output.gas keeps revm's
        // vanilla `Gas::new(effective_gas_limit)`. effective_gas_limit was capped to
        // exactly GAS_COST, so limit() == GAS_COST. spent() is 0 because revm did not
        // call record_cost on the error path.
        assert_eq!(output.gas.limit(), GAS_COST);
        assert_eq!(output.gas.spent(), 0);
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
        use crate::{MegaEvm, MegaTransaction};
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
        let mut tx = MegaTransaction::new(tx);
        tx.enveloped_tx = Some(BytesT::new());
        let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact ok");

        // The TX must halt (precompile OOG'd because cap forced effective_gas_limit < GAS_COST).
        assert!(!result.result.is_success(), "tx must halt: cap forced precompile OOG",);
        // Critical invariant: the receipt's gas_used reports the FULL tx.gas_limit. If
        // the Gas-normalization breaks refund accounting on the OOG path, gas_used would
        // be far smaller (sender refunded most of tx.gas_limit despite the halt).
        assert_eq!(
            result.result.gas_used(),
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

        let result = precompiles_map.run(&mut context, &address, &inputs, true, forwarded_gas);
        let output = result.expect("run ok").expect("Some output");
        // Precompile runs with full forwarded gas and succeeds, spending GAS_COST.
        // The post-hoc record_compute_gas call pushes the tracker over the limit, but
        // that detection happens via check_limit elsewhere, not in this path.
        assert!(matches!(output.result, InstructionResult::Return));
        assert_eq!(output.gas.spent(), GAS_COST);
        assert_eq!(output.gas.limit(), forwarded_gas);
    }
}
