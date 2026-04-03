// Derived from AHash v0.8.12 - see NOTICE.md for full attribution

pub(crate) trait ReadFromSlice {
    fn read_u16(&self) -> (u16, &[u8]);
    fn read_u32(&self) -> (u32, &[u8]);
    fn read_u64(&self) -> (u64, &[u8]);
    fn read_u128(&self) -> (u128, &[u8]);
    fn read_last_u32(&self) -> u32;
    fn read_last_u64(&self) -> u64;
    fn read_last_u128(&self) -> u128;
}

macro_rules! as_array {
    ($input:expr, $len:expr) => {{
        {
            #[inline(always)]
            fn as_array<T>(slice: &[T]) -> &[T; $len] {
                core::convert::TryFrom::try_from(slice).unwrap()
            }
            as_array($input)
        }
    }};
}

// The original salt crate uses zeocopy::transmute! (native endian). We use from_le_bytes
// to match behavior on all current targets (x86_64, riscv64 — both little-endian) without
// requiring the zerocopy dependency.
#[inline(always)]
fn u16_from_le_bytes(bytes: &[u8; 2]) -> u16 {
    u16::from_le_bytes(*bytes)
}

#[inline(always)]
fn u32_from_le_bytes(bytes: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*bytes)
}

#[inline(always)]
fn u64_from_le_bytes(bytes: &[u8; 8]) -> u64 {
    u64::from_le_bytes(*bytes)
}

#[inline(always)]
fn u128_from_le_bytes(bytes: &[u8; 16]) -> u128 {
    u128::from_le_bytes(*bytes)
}

impl ReadFromSlice for [u8] {
    #[inline(always)]
    fn read_u16(&self) -> (u16, &[u8]) {
        let (value, rest) = self.split_at(2);
        (u16_from_le_bytes(as_array!(value, 2)), rest)
    }

    #[inline(always)]
    fn read_u32(&self) -> (u32, &[u8]) {
        let (value, rest) = self.split_at(4);
        (u32_from_le_bytes(as_array!(value, 4)), rest)
    }

    #[inline(always)]
    fn read_u64(&self) -> (u64, &[u8]) {
        let (value, rest) = self.split_at(8);
        (u64_from_le_bytes(as_array!(value, 8)), rest)
    }

    #[inline(always)]
    fn read_u128(&self) -> (u128, &[u8]) {
        let (value, rest) = self.split_at(16);
        (u128_from_le_bytes(as_array!(value, 16)), rest)
    }

    #[inline(always)]
    fn read_last_u32(&self) -> u32 {
        let (_, value) = self.split_at(self.len() - 4);
        u32_from_le_bytes(as_array!(value, 4))
    }

    #[inline(always)]
    fn read_last_u64(&self) -> u64 {
        let (_, value) = self.split_at(self.len() - 8);
        u64_from_le_bytes(as_array!(value, 8))
    }

    #[inline(always)]
    fn read_last_u128(&self) -> u128 {
        let (_, value) = self.split_at(self.len() - 16);
        u128_from_le_bytes(as_array!(value, 16))
    }
}
