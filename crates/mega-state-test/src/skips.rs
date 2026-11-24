//! The tests the runner does not execute, and why.
//!
//! Every skip is one the revm fork's reference runner makes too, for the same reason, so the two
//! runners execute the same population and their executed and skipped counts can be compared
//! number for number. Satin inherits every one of the reasons from revm: none of them is a place
//! Satin differs from Ethereum, which is what a [deviation](crate::deviations) is.

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

/// Whether the file at `path` is skipped whole, and why: the reference runner's list, file for
/// file.
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
        "Call50000_sha256.json" |
        "static_Call50000_sha256.json" |
        "loopMul.json" |
        "CALLBlake2f_MaxRounds.json" => Some(SkipReason::Slow),
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
        assert_eq!(skip_file("a/loopMul.json"), Some(SkipReason::Slow));
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
}
