use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::TestUnit;

/// The top level test suite struct
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSuite(pub BTreeMap<String, TestUnit>);
