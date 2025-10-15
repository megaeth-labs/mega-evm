//! Custom precompiles for `MegaETH` EVM.
//!
//! This module provides custom precompile implementations with `MegaETH`-specific
//! gas cost overrides.

use crate::MegaSpecId;
use delegate::delegate;
use once_cell::race::OnceBox;
use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{InputsImpl, InterpreterResult},
    precompile::Precompiles,
    primitives::Address,
};

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;

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
    use revm::precompile::PrecompileWithAddress;

    /// Gas cost for the KZG point evaluation precompile.
    pub const GAS_COST: u64 = 100_000;

    /// KZG point evaluation precompile. This is the modified version of the original precompile
    /// with a custom gas cost.
    pub const KZG_POINT_EVALUATION: PrecompileWithAddress = PrecompileWithAddress(
        revm::precompile::kzg_point_evaluation::ADDRESS,
        |input, gas_limit| {
            let mut output = revm::precompile::kzg_point_evaluation::run(input, gas_limit)?;
            output.gas_used = GAS_COST;
            Ok(output)
        },
    );
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
