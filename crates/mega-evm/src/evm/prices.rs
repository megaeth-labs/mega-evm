//! The two byte prices the Satin gas table is built from, and a measurement-only way to build it
//! at prices other than the constants.
//!
//! Every Satin schedule entry that depends on a byte price reads it through
//! [`active_satin_prices`]: today the EIP-8037 state-gas entries, later the history entries.
//!
//! Without the `satin-price-override` feature [`active_satin_prices`] is a constant, so a build
//! that does not opt in prices with [`COST_PER_STATE_BYTE`] and [`COST_PER_HISTORY_BYTE`] and has
//! no way to do otherwise. With the feature, a process may fix other prices once, before it
//! executes anything, either with [`install_satin_prices`] or through the [`CPSB_ENV_VAR`] and
//! [`CPHB_ENV_VAR`] environment variables. Fixing nothing, or fixing the constants, leaves every
//! entry exactly where it was.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{fmt, str::FromStr};
use std::string::String;

use crate::constants::{COST_PER_HISTORY_BYTE, COST_PER_STATE_BYTE};

/// Environment variable read for the cost per state byte when no price was installed.
pub const CPSB_ENV_VAR: &str = "MEGA_SATIN_CPSB";

/// Environment variable read for the cost per history byte when no price was installed.
pub const CPHB_ENV_VAR: &str = "MEGA_SATIN_CPHB";

/// Thousandths of a gas unit in one gas unit.
const MILLI_PER_GAS: u64 = 1_000;

/// Decimal places a price may be written with: one per factor of ten in [`MILLI_PER_GAS`].
const MAX_DECIMALS: usize = 3;

/// The largest item a schedule entry prices, in bytes. A fixed-size charge is bounded by it, which
/// is what makes [`BytePrice::fixed_size_gas`] total.
const MAX_FIXED_SIZE_BYTES: u64 = MILLI_PER_GAS;

/// A gas price per byte.
///
/// Kept in thousandths of a gas unit, so a price such as 312.5 can be expressed. A charge is the
/// byte count times the price, rounded to the nearest whole gas unit with halves rounded up. A
/// whole-gas price never rounds: its charge is exactly `bytes * price` and overflows exactly where
/// that product does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BytePrice {
    milli_gas: u64,
}

impl BytePrice {
    /// A price of `gas` whole gas units per byte.
    ///
    /// # Panics
    ///
    /// If `gas` thousandths of a gas unit do not fit in a `u64`.
    pub const fn from_gas(gas: u64) -> Self {
        match gas.checked_mul(MILLI_PER_GAS) {
            Some(milli_gas) => Self { milli_gas },
            None => panic!("byte price does not fit in thousandths of a gas unit"),
        }
    }

    /// A price of `milli_gas` thousandths of a gas unit per byte.
    pub const fn from_milli_gas(milli_gas: u64) -> Self {
        Self { milli_gas }
    }

    /// The price in thousandths of a gas unit per byte.
    pub const fn milli_gas(self) -> u64 {
        self.milli_gas
    }

    /// The gas `bytes` bytes cost at this price, or `None` if it does not fit in a `u64`.
    #[inline]
    pub const fn gas_for(self, bytes: u64) -> Option<u64> {
        if self.milli_gas.is_multiple_of(MILLI_PER_GAS) {
            return bytes.checked_mul(self.milli_gas / MILLI_PER_GAS);
        }
        let milli_gas = bytes as u128 * self.milli_gas as u128;
        let gas = (milli_gas + MILLI_PER_GAS as u128 / 2) / MILLI_PER_GAS as u128;
        if gas > u64::MAX as u128 {
            None
        } else {
            Some(gas as u64)
        }
    }

    /// The gas a fixed-size item of `bytes` bytes costs at this price, for items of at most
    /// [`MAX_FIXED_SIZE_BYTES`] bytes.
    ///
    /// Such a charge cannot overflow: a price is at most `u64::MAX` thousandths of a gas unit, so
    /// a thousand bytes of it is at most `u64::MAX` gas. Schedule entries are priced through this,
    /// which is why no price can make one fail.
    ///
    /// # Panics
    ///
    /// If `bytes` exceeds [`MAX_FIXED_SIZE_BYTES`].
    pub const fn fixed_size_gas(self, bytes: u64) -> u64 {
        assert!(bytes <= MAX_FIXED_SIZE_BYTES, "a fixed-size item is at most a thousand bytes");
        match self.gas_for(bytes) {
            Some(gas) => gas,
            None => unreachable!(),
        }
    }
}

impl fmt::Display for BytePrice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.milli_gas / MILLI_PER_GAS;
        let fraction = self.milli_gas % MILLI_PER_GAS;
        if fraction == 0 {
            return write!(f, "{whole}");
        }
        let mut digits = [0u8; MAX_DECIMALS];
        let mut rest = fraction;
        for digit in digits.iter_mut().rev() {
            *digit = b'0' + (rest % 10) as u8;
            rest /= 10;
        }
        let len = digits.iter().rposition(|d| *d != b'0').map_or(0, |last| last + 1);
        let digits = core::str::from_utf8(&digits[..len]).expect("ASCII digits");
        write!(f, "{whole}.{digits}")
    }
}

impl FromStr for BytePrice {
    type Err = String;

    /// Parses a non-negative decimal with at most three decimal places, such as `1530` or `312.5`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
        let is_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        if whole.is_empty() || !is_digits(whole) || !is_digits(fraction) {
            return Err(format!("`{s}` is not a non-negative decimal number"));
        }
        if s.contains('.') && fraction.is_empty() {
            return Err(format!("`{s}` has no digits after the decimal point"));
        }
        if fraction.len() > MAX_DECIMALS {
            return Err(format!("`{s}` has more than {MAX_DECIMALS} decimal places"));
        }
        let too_large = || format!("`{s}` is too large");
        let whole: u64 = whole.parse().map_err(|_| too_large())?;
        let mut milli_fraction = 0u64;
        for (place, digit) in fraction.bytes().enumerate() {
            let scale = 10u64.pow((MAX_DECIMALS - 1 - place) as u32);
            milli_fraction += u64::from(digit - b'0') * scale;
        }
        let milli_gas = whole
            .checked_mul(MILLI_PER_GAS)
            .and_then(|milli| milli.checked_add(milli_fraction))
            .ok_or_else(too_large)?;
        Ok(Self { milli_gas })
    }
}

/// The cost per state byte and the cost per history byte the Satin gas table is built from.
///
/// `cphb` is carried and installable, but prices no schedule entry yet: history gas lands as its
/// own mechanism, and until then the schedule's history entry stays zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SatinPrices {
    /// Cost per state byte: what one byte added to the world state costs.
    pub cpsb: BytePrice,
    /// Cost per history byte: what one byte appended to the chain's history costs.
    pub cphb: BytePrice,
}

/// Why a set of Satin prices was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SatinPriceError {
    /// A price did not parse.
    Invalid {
        /// Where the price came from.
        name: String,
        /// What was wrong with it.
        reason: String,
    },
    /// The process had already fixed different prices.
    AlreadyFixed {
        /// The prices in effect.
        installed: SatinPrices,
        /// The prices that were asked for.
        requested: SatinPrices,
    },
}

impl fmt::Display for SatinPriceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { name, reason } => write!(f, "invalid {name}: {reason}"),
            Self::AlreadyFixed { installed, requested } => write!(
                f,
                "Satin prices are already fixed at cpsb = {}, cphb = {}; cannot change them to \
                 cpsb = {}, cphb = {}",
                installed.cpsb, installed.cphb, requested.cpsb, requested.cphb
            ),
        }
    }
}

impl core::error::Error for SatinPriceError {}

impl SatinPrices {
    /// The prices the Satin spec defines: [`COST_PER_STATE_BYTE`] and [`COST_PER_HISTORY_BYTE`].
    pub const CONSTANTS: Self = Self {
        cpsb: BytePrice::from_gas(COST_PER_STATE_BYTE),
        cphb: BytePrice::from_gas(COST_PER_HISTORY_BYTE),
    };

    /// Whether these are the prices the Satin spec defines.
    pub fn is_constants(&self) -> bool {
        *self == Self::CONSTANTS
    }

    /// Reads the prices from [`CPSB_ENV_VAR`] and [`CPHB_ENV_VAR`]; an unset variable keeps the
    /// constant price.
    #[cfg(feature = "std")]
    pub fn from_env() -> Result<Self, SatinPriceError> {
        let read = |name: &str, constant: BytePrice| match std::env::var(name) {
            Ok(value) => value
                .trim()
                .parse()
                .map_err(|reason| SatinPriceError::Invalid { name: name.to_string(), reason }),
            Err(std::env::VarError::NotPresent) => Ok(constant),
            Err(err) => {
                Err(SatinPriceError::Invalid { name: name.to_string(), reason: err.to_string() })
            }
        };
        Ok(Self {
            cpsb: read(CPSB_ENV_VAR, Self::CONSTANTS.cpsb)?,
            cphb: read(CPHB_ENV_VAR, Self::CONSTANTS.cphb)?,
        })
    }
}

#[cfg(feature = "satin-price-override")]
static FIXED: std::sync::OnceLock<SatinPrices> = std::sync::OnceLock::new();

/// Fixes the prices the Satin gas table is built from for the rest of the process.
///
/// Succeeds if nothing has fixed the prices yet, or if they are already fixed at `prices`. The
/// prices become fixed the first time a schedule is built, so this must run before anything is
/// executed.
#[cfg(feature = "satin-price-override")]
pub fn install_satin_prices(requested: SatinPrices) -> Result<(), SatinPriceError> {
    let installed = *FIXED.get_or_init(|| requested);
    if installed == requested {
        Ok(())
    } else {
        Err(SatinPriceError::AlreadyFixed { installed, requested })
    }
}

/// The prices the Satin gas table is built from.
///
/// Without the `satin-price-override` feature, always [`SatinPrices::CONSTANTS`].
///
/// # Panics
///
/// With the feature, if nothing was installed and an environment variable holds an invalid price:
/// silently building at the constants would pass off one measurement as another.
#[inline]
pub fn active_satin_prices() -> SatinPrices {
    #[cfg(feature = "satin-price-override")]
    {
        *FIXED.get_or_init(|| SatinPrices::from_env().unwrap_or_else(|err| panic!("{err}")))
    }
    #[cfg(not(feature = "satin-price-override"))]
    {
        SatinPrices::CONSTANTS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prices_parse_as_decimals_with_up_to_three_places() {
        let parse = |s: &str| s.parse::<BytePrice>().map(BytePrice::milli_gas);
        assert_eq!(parse("1530"), Ok(1_530_000));
        assert_eq!(parse("312.5"), Ok(312_500));
        assert_eq!(parse("0.125"), Ok(125));
        assert_eq!(parse("0"), Ok(0));
        assert_eq!(parse("007.050"), Ok(7_050));
        for bad in
            ["", ".5", "5.", "-1", "+1", "1e3", "1.2345", "abc", "1 000", "18446744073709552"]
        {
            assert!(parse(bad).is_err(), "`{bad}` must be rejected");
        }
    }

    #[test]
    fn test_prices_print_as_they_parse() {
        for s in ["1530", "312.5", "0.125", "0", "88", "1.05"] {
            assert_eq!(s.parse::<BytePrice>().unwrap().to_string(), s);
        }
    }

    /// A whole-gas price charges exactly `bytes * price`, overflowing exactly where the product
    /// does, which is what makes the constants reproduce the unconditional arithmetic.
    #[test]
    fn test_a_whole_price_charges_the_plain_product() {
        let bytes = [0, 1, 23, 64, 120, 524_288, u64::MAX / 1_530, u64::MAX / 1_530 + 1, u64::MAX];
        for gas in [0, 1, 88, 1_530, 40_000, u64::MAX / MILLI_PER_GAS] {
            let price = BytePrice::from_gas(gas);
            for &n in &bytes {
                assert_eq!(price.gas_for(n), n.checked_mul(gas), "{n} bytes at {gas}");
            }
        }
    }

    #[test]
    fn test_a_fractional_price_rounds_half_up() {
        let price: BytePrice = "312.5".parse().unwrap();
        assert_eq!(price.gas_for(1), Some(313));
        assert_eq!(price.gas_for(2), Some(625));
        assert_eq!(price.gas_for(64), Some(20_000));
        assert_eq!(price.gas_for(23), Some(7_188));
        let price: BytePrice = "0.001".parse().unwrap();
        assert_eq!(price.gas_for(499), Some(0));
        assert_eq!(price.gas_for(500), Some(1));
        assert_eq!(price.gas_for(u64::MAX), Some(u64::MAX / 1000 + 1));
        let price: BytePrice = "1.5".parse().unwrap();
        assert_eq!(price.gas_for(u64::MAX), None);
    }

    #[test]
    fn test_the_constants_are_the_spec_prices() {
        let constants = SatinPrices::CONSTANTS;
        assert_eq!(constants.cpsb.gas_for(1), Some(COST_PER_STATE_BYTE));
        assert_eq!(constants.cphb.gas_for(1), Some(COST_PER_HISTORY_BYTE));
        assert!(constants.is_constants());
        assert!(!SatinPrices { cpsb: BytePrice::from_gas(1), ..constants }.is_constants());
    }

    /// No price can make a fixed-size charge overflow, which is why the schedule needs no
    /// validation.
    #[test]
    fn test_a_thousand_bytes_of_any_price_fits() {
        assert_eq!(BytePrice::from_milli_gas(u64::MAX).fixed_size_gas(1_000), u64::MAX);
        let whole = BytePrice::from_gas(u64::MAX / MILLI_PER_GAS);
        assert_eq!(whole.fixed_size_gas(1_000), u64::MAX / MILLI_PER_GAS * MILLI_PER_GAS);
    }

    /// Without the override feature the active prices are the constants and nothing can change
    /// them; with it, an unset environment leaves them at the constants too.
    #[test]
    fn test_the_active_prices_default_to_the_constants() {
        assert_eq!(active_satin_prices(), SatinPrices::CONSTANTS);
    }

    /// Installing the prices already in effect is accepted, so a measurement harness may install
    /// unconditionally; installing different ones after that is refused, naming both.
    #[cfg(feature = "satin-price-override")]
    #[test]
    fn test_installing_the_prices_in_effect_is_idempotent() {
        assert_eq!(install_satin_prices(SatinPrices::CONSTANTS), Ok(()));
        assert_eq!(install_satin_prices(SatinPrices::CONSTANTS), Ok(()));

        let requested = SatinPrices { cpsb: "312.5".parse().unwrap(), ..SatinPrices::CONSTANTS };
        assert_eq!(
            install_satin_prices(requested),
            Err(SatinPriceError::AlreadyFixed { installed: SatinPrices::CONSTANTS, requested })
        );
        assert_eq!(active_satin_prices(), SatinPrices::CONSTANTS);
    }

    #[test]
    fn test_price_errors_name_the_variable_and_the_installed_prices() {
        let invalid = SatinPriceError::Invalid {
            name: CPSB_ENV_VAR.to_string(),
            reason: "`x` is not a non-negative decimal number".to_string(),
        };
        assert_eq!(
            invalid.to_string(),
            "invalid MEGA_SATIN_CPSB: `x` is not a non-negative decimal number"
        );
        let requested = SatinPrices { cpsb: "312.5".parse().unwrap(), ..SatinPrices::CONSTANTS };
        let already =
            SatinPriceError::AlreadyFixed { installed: SatinPrices::CONSTANTS, requested };
        assert_eq!(
            already.to_string(),
            "Satin prices are already fixed at cpsb = 1530, cphb = 88; cannot change them to \
             cpsb = 312.5, cphb = 88"
        );
    }
}
