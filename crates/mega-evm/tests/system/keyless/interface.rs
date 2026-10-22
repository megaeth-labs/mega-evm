//! The errors `IKeylessDeploy` declares, against the error set the `KeylessDeploy` spec page lists
//! for Satin.

use std::collections::BTreeSet;

use mega_evm::system::keyless::IKeylessDeploy::IKeylessDeployErrors;

/// The `KeylessDeploy` spec page.
const KEYLESS_DEPLOY_MD: &str =
    include_str!("../../../../../docs/spec/system-contracts/keyless-deploy.md");

/// The summary line of the page's block that lists Satin's error set.
const SATIN_ERROR_SET: &str = "<summary>Satin (unstable): error set</summary>";

/// The error signatures the page's Satin block lists: every bullet that is a single code span.
fn documented_errors() -> BTreeSet<&'static str> {
    let start = KEYLESS_DEPLOY_MD.find(SATIN_ERROR_SET).expect("the page lists Satin's error set");
    let block = &KEYLESS_DEPLOY_MD[start..];
    let block = &block[..block.find("</details>").expect("the block closes")];
    block.lines().filter_map(|line| line.strip_prefix("- `")?.strip_suffix('`')).collect()
}

/// The generated binding declares exactly the errors the spec page lists for Satin, so an error
/// added to or removed from the interface without the page, or from the page without the
/// interface, fails here.
#[test]
fn test_the_interface_declares_exactly_the_documented_errors() {
    let declared: BTreeSet<&str> = IKeylessDeployErrors::SIGNATURES.iter().copied().collect();
    assert_eq!(declared.len(), IKeylessDeployErrors::SIGNATURES.len(), "a signature repeats");
    assert_eq!(declared, documented_errors());
}
