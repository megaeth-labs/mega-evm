use mega_evm::revm::primitives::Address;
use serde::{de, Deserialize, Serialize, Serializer};

/// Deserializes a [string][String] as a [u64].
pub fn deserialize_str_as_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: de::Deserializer<'de>,
{
    let string = String::deserialize(deserializer)?;

    if let Some(stripped) = string.strip_prefix("0x") {
        u64::from_str_radix(stripped, 16)
    } else {
        string.parse()
    }
    .map_err(serde::de::Error::custom)
}

/// Deserializes a [string][String] as an optional [Address].
pub fn deserialize_maybe_empty<'de, D>(deserializer: D) -> Result<Option<Address>, D::Error>
where
    D: de::Deserializer<'de>,
{
    let string = String::deserialize(deserializer)?;
    if string.is_empty() {
        Ok(None)
    } else {
        string.parse().map_err(de::Error::custom).map(Some)
    }
}

/// Serializes an optional [Address], writing `""` for `None` (contract creation)
/// so it round-trips with [`deserialize_maybe_empty`]. The default `Option`
/// serializer would emit `null`, which that deserializer rejects — breaking any
/// re-serialization of a CREATE transaction (e.g. `--dump-fixture` / `--fill`).
pub fn serialize_maybe_empty<S>(addr: &Option<Address>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match addr {
        Some(address) => address.serialize(serializer),
        None => serializer.serialize_str(""),
    }
}
