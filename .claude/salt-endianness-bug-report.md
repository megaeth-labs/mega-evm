# Bug Report: Non-deterministic hashing on big-endian targets in `salt`

## Summary

The `salt` crate's `bucket_id` hash function produces platform-dependent results due to native-endian byte interpretation in the vendored AHash implementation.
On big-endian targets, the same key bytes would produce different bucket IDs than on little-endian targets, violating the consensus determinism guarantee.

## Severity

**Low** in practice (all current deployment targets are little-endian), but **high** in principle — a consensus-critical hash function must be deterministic across all platforms.

## Affected Code

**File:** `salt/src/state/ahash/convert.rs`

```rust
macro_rules! convert {
    ($a:ty, $b:ty) => {
        impl Convert<$b> for $a {
            #[inline(always)]
            fn convert(self) -> $b {
                zerocopy::transmute!(self)  // ← native-endian bit reinterpretation
            }
        }
    };
}
```

This macro generates `Convert` impls used by `ReadFromSlice` to convert `&[u8; N]` → `u16/u32/u64/u128`:

```rust
impl ReadFromSlice for [u8] {
    fn read_u64(&self) -> (u64, &[u8]) {
        let (value, rest) = self.split_at(8);
        (as_array!(value, 8).convert(), rest)  // ← [u8; 8] → u64 via transmute
    }
    // ... same pattern for read_u16, read_u32, read_u128, read_last_*
}
```

`zerocopy::transmute!` is semantically equivalent to `core::mem::transmute` — it reinterprets the raw bits without any byte-order conversion.
On a **little-endian** target, `[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]` becomes `0x0807060504030201`.
On a **big-endian** target, the same bytes become `0x0102030405060708`.

## Impact

The `bucket_id` function flows through:

```
bucket_id(key: &[u8])
  → hash(key)
    → Hasher::write(key)
      → ReadFromSlice::read_u64 / read_u128 / read_u32 / read_u16
        → [u8; N].convert() → zerocopy::transmute!()  ← endian-dependent
```

This means:
- `bucket_id` returns different values on LE vs BE for the same key bytes.
- `hash()` with nonce (used for probe sequences) is also affected.
- Any state tree built on a LE node cannot be verified by a BE node and vice versa.

## Reproduction

Not directly reproducible without a BE target.
However, the bug can be demonstrated logically:

```rust
// On little-endian: [u8; 8] → u64
let bytes: [u8; 8] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
let val: u64 = zerocopy::transmute!(bytes);
// LE result: 0x0807060504030201
// BE result: 0x0102030405060708 (different!)
```

## Suggested Fix

Replace `zerocopy::transmute!` with explicit little-endian conversion in `convert.rs`:

```rust
// Before (native-endian, non-deterministic):
impl Convert<u64> for [u8; 8] {
    fn convert(self) -> u64 {
        zerocopy::transmute!(self)
    }
}

// After (always little-endian, deterministic):
impl Convert<u64> for [u8; 8] {
    fn convert(self) -> u64 {
        u64::from_le_bytes(self)
    }
}
```

This must be applied to all `ReadFromSlice` conversion paths: `u16`, `u32`, `u64`, `u128`, and their `read_last_*` variants.

Alternatively, add a compile-time guard to prevent accidental use on BE targets:

```rust
#[cfg(not(target_endian = "little"))]
compile_error!("salt hasher assumes little-endian byte order");
```

## Notes

- The upstream `ahash` crate (v0.8.12) has the same pattern — it uses `transmute` for performance and does not claim cross-platform hash stability.
  The salt crate inherited this from ahash when vendoring the fallback hasher.
- The `mega-evm` crate (PR [#225](https://github.com/megaeth-labs/mega-evm/pull/225)) already uses `from_le_bytes` in its inlined copy, making it deterministic across all platforms.
- The `zerocopy` dependency (v0.7) can be dropped entirely if all conversions are replaced with `from_le_bytes`, since it is only used for these `transmute!` calls in the hasher module.
