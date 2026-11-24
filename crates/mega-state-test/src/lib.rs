//! Self-contained EEST-compatible state-test fixtures and runner for mega-evm.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod types;

pub mod runner;

pub mod utils;
