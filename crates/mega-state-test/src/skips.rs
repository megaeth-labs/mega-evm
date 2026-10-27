//! The tests the runner does not execute, and why.
//!
//! Every skip is one the revm fork's reference runner makes too, for the same reason, so the two
//! runners execute the same population and their executed and skipped counts can be compared
//! number for number. Satin inherits every one of the reasons from revm: none of them is a place
//! Satin differs from Ethereum, which is what a [deviation](crate::deviations) is.

use mega_evm::revm::primitives::{b256, keccak256, B256};
use serde::Serialize;

/// Why a test was not executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// The test creates an account at an address whose pre-state holds storage and nothing else
    /// (EIP-7610). revm reads storage slot by slot and never asks the database whether an account
    /// has any, so it cannot see the collision.
    CreateCollisionWithStorage,
    /// The fixture's transaction cannot be built — an invalid signature, a blob or an EIP-7702
    /// transaction without a recipient — and the fixture expects it to be invalid.
    UnbuildableInvalidTransaction,
    /// A value in the fixture overflows the width revm gives it.
    MalformedValue,
    /// The test passes but takes minutes.
    Slow,
}

impl SkipReason {
    /// Every reason.
    pub const ALL: [Self; 4] = [
        Self::CreateCollisionWithStorage,
        Self::UnbuildableInvalidTransaction,
        Self::MalformedValue,
        Self::Slow,
    ];

    /// The reason's name, as the summary prints it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::CreateCollisionWithStorage => "create-collision-with-storage",
            Self::UnbuildableInvalidTransaction => "unbuildable-invalid-transaction",
            Self::MalformedValue => "malformed-value",
            Self::Slow => "slow",
        }
    }
}

/// The directory of the EIP-7610 collision tests, every one of which collides with storage.
const CREATE_COLLISION_DIR: &str = "paris/eip7610_create_collision";

/// Raw-file identities of the reference runner's four slow fixture names, including the two
/// older variants. These are `ethereum/legacytests` `GeneralStateTests` at
/// `1f581b8ccdc4c63acf5f2c5c1b155c690c32a8eb`: the Cancun files and the two Constantinople files.
/// The byte lengths avoid hashing every unrelated fixture; the hash decides whether to skip.
const SLOW_FILES: [(usize, B256); 6] = [
    // Cancun/GeneralStateTests/VMTests/vmPerformance/loopMul.json
    (21579, b256!("2bf893c9a593ae34e289a8938a57ec2e29ee093e2dfe4f2154dfde2a39775c79")),
    // Cancun/GeneralStateTests/stQuadraticComplexityTest/Call50000_sha256.json
    (10237, b256!("39ee8aaa9d540a9ff4df3b25cee4b85dbb400728b7d9650e19b8277eaf35eaa0")),
    // Cancun/GeneralStateTests/stTimeConsuming/CALLBlake2f_MaxRounds.json
    (9525, b256!("1db1a1c4c8d1ecfab848565dc5d266a9fb60959995d03619e2837a7d863d70fd")),
    // Cancun/GeneralStateTests/stTimeConsuming/static_Call50000_sha256.json
    (11751, b256!("9f20d4d227a41fd7caa577612255d013b38d90668971db19f9228e7061be92b4")),
    // Constantinople/GeneralStateTests/stQuadraticComplexityTest/Call50000_sha256.json
    (5400, b256!("937c269ce29890164f14617f1da277189d49d4049b63507309479e2b29d36346")),
    // Constantinople/GeneralStateTests/stTimeConsuming/static_Call50000_sha256.json
    (5290, b256!("2e2d37b3c3ed48c2da697e2a443d653b300b4aee7d7c56da8d9eff30dc49b93d")),
];

/// Whether `contents` is one of the reference runner's known slow fixtures, independent of its
/// file name. A different file that borrows a slow fixture's name must still execute.
pub fn skip_slow_file(contents: &[u8]) -> Option<SkipReason> {
    if !SLOW_FILES.iter().any(|(size, _)| *size == contents.len()) {
        return None;
    }
    let hash = keccak256(contents);
    SLOW_FILES
        .iter()
        .any(|(size, known)| *size == contents.len() && *known == hash)
        .then_some(SkipReason::Slow)
}

/// Whether the file at `path` is skipped whole, and why: the reference runner's list, file for
/// file, except that slow fixtures are identified by [`skip_slow_file`] from their content.
pub fn skip_file(path: &str) -> Option<SkipReason> {
    if path.replace('\\', "/").contains(CREATE_COLLISION_DIR) {
        return Some(SkipReason::CreateCollisionWithStorage);
    }
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name {
        "RevertInCreateInInit_Paris.json" |
        "RevertInCreateInInit.json" |
        "dynamicAccountOverwriteEmpty.json" |
        "dynamicAccountOverwriteEmpty_Paris.json" |
        "RevertInCreateInInitCreate2Paris.json" |
        "create2collisionStorage.json" |
        "RevertInCreateInInitCreate2.json" |
        "create2collisionStorageParis.json" |
        "InitCollision.json" |
        "InitCollisionParis.json" |
        "test_init_collision_create_opcode.json" => Some(SkipReason::CreateCollisionWithStorage),
        "ValueOverflow.json" | "ValueOverflowParis.json" => Some(SkipReason::MalformedValue),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skip_file_matches_the_collision_directory() {
        assert_eq!(
            skip_file("fixtures/state_tests/paris/eip7610_create_collision/test_init_collision_create_tx.json"),
            Some(SkipReason::CreateCollisionWithStorage)
        );
        assert_eq!(
            skip_file("devnet/state_tests/for_amsterdam/paris/eip7610_create_collision/revert_in_create/x.json"),
            Some(SkipReason::CreateCollisionWithStorage)
        );
    }

    #[test]
    fn test_skip_file_matches_names_not_substrings() {
        assert_eq!(
            skip_file("static/state_tests/stCreate2/create2collisionStorageParis.json"),
            Some(SkipReason::CreateCollisionWithStorage)
        );
        assert_eq!(skip_file("a/ValueOverflow.json"), Some(SkipReason::MalformedValue));
        assert_eq!(skip_file("a/loopMul.json"), None);
        assert_eq!(skip_file("a/xloopMul.json"), None);
        assert_eq!(skip_file("a/loopMul.json.bak"), None);
        assert_eq!(skip_file("osaka/eip7951_p256verify_precompiles/test_valid.json"), None);
        assert_eq!(skip_file(""), None);
    }

    #[test]
    fn test_reason_names_are_distinct() {
        let names: std::collections::BTreeSet<_> = SkipReason::ALL.map(SkipReason::name).into();
        assert_eq!(names.len(), SkipReason::ALL.len());
    }

    /// The checked-in files retain their upstream bytes, so this catches a wrong pin as well as
    /// a regression to matching the filename or accepting edited content.
    #[test]
    fn test_pinned_slow_fixtures_follow_content_when_renamed_and_reject_edits() {
        let files: &[&[u8]] = &[
            include_bytes!("../tests/fixtures/slow/Cancun/GeneralStateTests/VMTests/vmPerformance/loopMul.json"),
            include_bytes!("../tests/fixtures/slow/Cancun/GeneralStateTests/stQuadraticComplexityTest/Call50000_sha256.json"),
            include_bytes!("../tests/fixtures/slow/Cancun/GeneralStateTests/stTimeConsuming/CALLBlake2f_MaxRounds.json"),
            include_bytes!("../tests/fixtures/slow/Cancun/GeneralStateTests/stTimeConsuming/static_Call50000_sha256.json"),
            include_bytes!("../tests/fixtures/slow/Constantinople/GeneralStateTests/stQuadraticComplexityTest/Call50000_sha256.json"),
            include_bytes!("../tests/fixtures/slow/Constantinople/GeneralStateTests/stTimeConsuming/static_Call50000_sha256.json"),
        ];
        assert_eq!(files.len(), SLOW_FILES.len(), "exercise every pinned content identity");
        for bytes in files {
            assert_eq!(skip_slow_file(bytes), Some(SkipReason::Slow));
            let renamed = "renamed-fixture.json";
            assert_eq!(
                skip_file(renamed).or_else(|| skip_slow_file(bytes)),
                Some(SkipReason::Slow)
            );
            let mut edited = bytes.to_vec();
            edited[0] ^= 1;
            assert_eq!(skip_slow_file(&edited), None, "same-length content edits must execute");
        }
    }
}
