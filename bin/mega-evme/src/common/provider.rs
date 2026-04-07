//! Unified RPC provider factory for mega-evme.

use alloy_provider::{DynProvider, ProviderBuilder};
use clap::Parser;
use tracing::debug;

use super::{EvmeError, Result};

/// Configuration for building an RPC provider.
#[derive(Parser, Debug, Clone)]
#[command(next_help_heading = "RPC Options")]
pub struct RpcArgs {
    /// RPC URL
    #[arg(
        long = "rpc",
        visible_aliases = ["rpc-url"],
        alias = "fork.rpc",
        env = "RPC_URL",
        default_value = "http://localhost:8545"
    )]
    pub rpc_url: String,
}

/// OP-stack provider type used throughout mega-evme.
pub type OpProvider = DynProvider<op_alloy_network::Optimism>;

impl RpcArgs {
    /// Build an OP-stack RPC provider.
    pub fn build_provider(&self) -> Result<OpProvider> {
        let url = self.rpc_url.parse().map_err(|e| {
            EvmeError::RpcError(format!("Invalid RPC URL '{}': {}", self.rpc_url, e))
        })?;

        debug!(rpc_url = %self.rpc_url, "Building RPC provider");

        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<op_alloy_network::Optimism>()
            .connect_http(url);

        Ok(DynProvider::new(provider))
    }
}
