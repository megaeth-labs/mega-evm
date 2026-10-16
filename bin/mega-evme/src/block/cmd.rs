//! `mega-evme replay --block`: replay whole blocks and compare them with the chain.

use std::path::PathBuf;

use clap::Args;
use tracing::info;

use super::{
    inputs::BlockSource,
    record::{records, BlockRecord, TxRecord},
    satin,
    state::{BlockState, ParentStateRpc},
};
use crate::{
    common::{EvmeError, LimitsOverride, Result, RpcArgs},
    engine::Engine,
};

/// Block replay configuration
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Block Replay Options")]
pub struct BlockArgs {
    /// Replay whole blocks instead of one transaction: `N`, or `N..M` for blocks N through M.
    /// Each block runs on the engine its spec names: the one `--override.spec` forces, or the
    /// one the chain's schedule gives at the block's timestamp.
    #[arg(
        long = "block",
        value_name = "N[..M]",
        value_parser = parse_block_range,
        conflicts_with_all = [
            "tx_hash", "dump_fixture", "trace", "dump", "gas_limit", "value", "input",
            "input_file", "capture_file", "replay_file",
        ],
    )]
    pub block: Option<BlockRange>,

    /// Block cache directory. A block is read from it when it is there, and a block fetched over
    /// `--rpc` is written to it, with the state its replay read beyond the block's trace, so the
    /// next replay of it is offline. Layout: `blocks/<N / 10000>/<N>.json.zst` and
    /// `codes/<hh>/<hash>.bin`.
    #[arg(long = "block-cache", value_name = "DIR", requires = "block")]
    pub block_cache: Option<PathBuf>,

    /// Compare every replayed transaction (status, gas used, cumulative gas used, logs) and the
    /// block's receipts root with the chain's, and exit with code 2 when a block differs.
    #[arg(long = "verify", requires = "block")]
    pub verify: bool,
}

/// An inclusive range of block numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRange {
    /// The first block.
    pub from: u64,
    /// The last block.
    pub to: u64,
}

/// Parses `N` or `N..M`, with `M >= N`.
pub fn parse_block_range(s: &str) -> std::result::Result<BlockRange, String> {
    let number = |part: &str| {
        part.trim().parse::<u64>().map_err(|e| format!("invalid block number {part:?}: {e}"))
    };
    let (from, to) = match s.split_once("..") {
        Some((from, to)) => (number(from)?, number(to)?),
        None => (number(s)?, number(s)?),
    };
    if to < from {
        return Err(format!("the range {s} ends before it starts"));
    }
    if from == 0 {
        return Err("block 0 has no parent to replay it on".to_string());
    }
    Ok(BlockRange { from, to })
}

/// What a replay of blocks came to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplaySummary {
    /// Blocks replayed.
    pub blocks: usize,
    /// Blocks whose every transaction and receipts root match the chain.
    pub matching: usize,
    /// Blocks that could not be replayed, with why.
    pub failed: Vec<(u64, String)>,
}

impl ReplaySummary {
    /// The process exit code: 1 when a block could not be replayed, 2 when `verify` and a block
    /// differs from the chain, 0 otherwise.
    pub fn exit_code(&self, verify: bool) -> i32 {
        if !self.failed.is_empty() {
            1
        } else if verify && self.matching != self.blocks {
            2
        } else {
            0
        }
    }
}

/// Replays `range` and prints a record per transaction and per block (`json`) or a line per
/// block.
pub async fn replay_blocks(
    range: BlockRange,
    args: &BlockArgs,
    rpc_args: &RpcArgs,
    bucket_capacities: &[(u32, u64)],
    spec_override: Option<&str>,
    limits_override: Option<&LimitsOverride>,
    json: bool,
) -> Result<ReplaySummary> {
    let (provider, chain_id) = if rpc_args.rpc_url.is_some() {
        let (provider, chain_id) = rpc_args.build_lookup_provider().await?;
        (Some(provider), Some(chain_id))
    } else if args.block_cache.is_some() {
        (None, None)
    } else {
        return Err(EvmeError::InvalidInput(
            "'replay --block' needs '--block-cache <DIR>', '--rpc <URL>', or both".to_string(),
        ));
    };
    let forced_engine = spec_override.map(Engine::of_spec).transpose()?;
    let source = BlockSource::new(args.block_cache.clone(), provider, chain_id);
    let mut codes = source.code_store();
    let mut summary = ReplaySummary::default();

    for number in range.from..=range.to {
        summary.blocks += 1;
        let outcome = async {
            let inputs = source.load(number, &mut codes).await?;
            let engine = forced_engine
                .unwrap_or_else(|| Engine::of_block(inputs.chain_id, inputs.header.timestamp));
            let fallback =
                source.provider().and_then(|p| ParentStateRpc::new(p.clone(), number - 1));
            let mut state =
                BlockState::new(inputs.prestate.clone(), std::mem::take(&mut codes), fallback);
            let executed = execute_block(
                engine,
                &inputs,
                bucket_capacities,
                spec_override,
                limits_override,
                &mut state,
            );
            let rpc_reads = state.fallback_reads;
            let BlockState { prestate, codes: mut store, .. } = state;
            let result = match executed {
                Ok((executed, spec)) => {
                    if !inputs.from_cache || rpc_reads > 0 {
                        source.store(&inputs, &prestate, &mut store)?;
                    }
                    Ok(records(&inputs, &executed, engine, &spec, rpc_reads))
                }
                Err(e) => Err(e),
            };
            codes = store;
            result
        }
        .await;
        match outcome {
            Ok((txs, block)) => {
                if block.matches_chain {
                    summary.matching += 1;
                }
                print_block(&txs, &block, json);
            }
            Err(e) => {
                let message = e.to_string();
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "kind": "block", "block": number, "error": message })
                    );
                } else {
                    println!("block {number}  error: {message}");
                }
                summary.failed.push((number, message));
            }
        }
    }
    info!(?summary, "Block replay finished");
    if !json {
        println!(
            "{} block(s) replayed: {} match the chain, {} differ, {} failed",
            summary.blocks,
            summary.matching,
            summary.blocks - summary.matching - summary.failed.len(),
            summary.failed.len()
        );
    }
    Ok(summary)
}

/// Executes the block `inputs` describes on `engine` over `state`, returning what it produced
/// and the spec it ran under: `spec_override` when given, otherwise, on the legacy engine, the
/// spec the chain's schedule gives at the block's timestamp.
///
/// `limits_override` replaces the protocol limits a Satin block runs under; a legacy block,
/// whose limits its spec fixes, is refused with one.
pub fn execute_block(
    engine: Engine,
    inputs: &super::inputs::BlockInputs,
    bucket_capacities: &[(u32, u64)],
    spec_override: Option<&str>,
    limits_override: Option<&LimitsOverride>,
    state: &mut BlockState,
) -> Result<(super::exec::ExecutedBlock, String)> {
    match engine {
        Engine::Satin => {
            let executed = satin::execute(
                inputs.chain_id,
                &inputs.header,
                &inputs.transactions,
                bucket_capacities,
                limits_override,
                state,
            )?;
            Ok((executed, mega_evm::MegaSpecId::SATIN.to_string()))
        }
        Engine::Legacy if limits_override.is_some() => Err(EvmeError::InvalidInput(format!(
            "block {} runs on the legacy engine, whose limits its spec fixes; --override.limits \
             applies to Satin only",
            inputs.header.number
        ))),
        Engine::Legacy => run_legacy_block(inputs, bucket_capacities, spec_override, state),
    }
}

#[cfg(feature = "legacy")]
fn run_legacy_block(
    inputs: &super::inputs::BlockInputs,
    bucket_capacities: &[(u32, u64)],
    spec_override: Option<&str>,
    state: &mut BlockState,
) -> Result<(super::exec::ExecutedBlock, String)> {
    let spec =
        super::legacy::resolve_spec(inputs.chain_id, inputs.header.timestamp, spec_override)?;
    let executed = super::legacy::execute(
        inputs.chain_id,
        &inputs.header,
        &inputs.transactions,
        bucket_capacities,
        spec_override,
        state,
    )?;
    Ok((executed, spec))
}

#[cfg(not(feature = "legacy"))]
fn run_legacy_block(
    inputs: &super::inputs::BlockInputs,
    _bucket_capacities: &[(u32, u64)],
    _spec_override: Option<&str>,
    _state: &mut BlockState,
) -> Result<(super::exec::ExecutedBlock, String)> {
    Err(EvmeError::InvalidInput(format!(
        "block {} runs on the legacy engine, and this build runs Satin only (built without \
         the `legacy` feature)",
        inputs.header.number
    )))
}

/// Prints a block's records: every record as a JSON line, or one line for the block.
fn print_block(txs: &[TxRecord], block: &BlockRecord, json: bool) {
    if json {
        for tx in txs {
            println!("{}", serde_json::to_string(tx).expect("a record serializes"));
        }
        println!("{}", serde_json::to_string(block).expect("a record serializes"));
        return;
    }
    let verdict = if block.matches_chain {
        "matches the chain".to_string()
    } else {
        let root = if block.receipts_root == block.chain_receipts_root {
            ""
        } else {
            ", receipts root differs"
        };
        format!("{} of {} transactions differ{root}", block.differing, block.transactions)
    };
    let ledgers = block.satin.as_ref().map_or_else(String::new, |satin| {
        let overridden = if satin.limits_override.is_some() { ", limits overridden" } else { "" };
        format!(
            " [regular {} state {} history {}{overridden}]",
            satin.regular_gas, satin.state_gas, satin.history_gas
        )
    });
    println!(
        "block {}  {} {}  {} txs  gas {}{ledgers} (chain {})  {verdict}",
        block.block,
        block.engine,
        block.spec,
        block.transactions,
        block.gas_used,
        block.chain_gas_used,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_ranges_parse() {
        assert_eq!(parse_block_range("7").unwrap(), BlockRange { from: 7, to: 7 });
        assert_eq!(parse_block_range("7..9").unwrap(), BlockRange { from: 7, to: 9 });
        assert!(parse_block_range("9..7").is_err());
        assert!(parse_block_range("0").is_err(), "block 0 has no parent");
        assert!(parse_block_range("x").is_err());
    }

    #[test]
    fn test_exit_codes() {
        let clean = ReplaySummary { blocks: 2, matching: 2, failed: vec![] };
        assert_eq!(clean.exit_code(true), 0);
        let differing = ReplaySummary { blocks: 2, matching: 1, failed: vec![] };
        assert_eq!(differing.exit_code(true), 2);
        assert_eq!(differing.exit_code(false), 0, "a difference is a failure only when verifying");
        let failed = ReplaySummary { blocks: 2, matching: 1, failed: vec![(1, "x".into())] };
        assert_eq!(failed.exit_code(true), 1);
    }
}
