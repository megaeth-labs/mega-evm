//! The evm for the Megaeth

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod context;
pub use context::*;

mod evm;
pub use evm::*;

mod handler;
pub use handler::*;

mod spec;
pub use spec::*;

mod types;
pub use types::*;
