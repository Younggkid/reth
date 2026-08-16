//! `witness-bench` — measures the marginal cost a block builder pays to attach an execution
//! witness to a block it is about to submit.
//!
//! The harness replays real blocks in **lockstep**: at loop entry the database tip is exactly the
//! parent of the block being replayed, so every state read hits [`LatestStateProviderRef`] and no
//! `revert_state()` overlay is ever materialized. That is the same provider a builder sees when it
//! builds on the current head, and it is the only regime in which these timings mean anything.
//!
//! Each block is decomposed into phases that are timed independently:
//!
//! | phase | who pays it | notes |
//! |---|---|---|
//! | `fetch` | harness | reading the block back out of static files |
//! | `exec` | builder anyway | EVM execution, minus the witness closure |
//! | `witness` | **marginal** | [`ExecutionWitnessRecord::into_execution_witness`] |
//! | `encode` | **marginal** | serializing the witness (`--encode-witness`) |
//! | `state_root` | builder anyway | needed for the header regardless |
//! | `persist` | harness | advancing the tip; a builder never does this |
//!
//! The number to report is `witness + encode`, and the ratio `witness / state_root`, which is how
//! much of the trie pass a builder could reclaim by fusing witness collection into the state-root
//! computation it already runs.
//!
//! `--repeat` re-runs the witness phase alone against the same executed state. That separates the
//! cost of the trie walk from the cost of faulting its nodes in: the first pass pays both, later
//! passes only the former. It composes with `--commit`, because the block is still executed and
//! persisted exactly once.
//!
//! `witness` is a single phase rather than the record / trie-walk / ancestor-header split an
//! earlier version of this harness timed. [`ExecutionWitnessRecord`] now borrows the finished
//! `State` and does all three inside one call, and that call is exactly what
//! `debug_executionWitness` invokes — so measuring it whole tracks the shipped path instead of a
//! hand-assembled approximation of it.

use alloy_consensus::BlockHeader;
use alloy_primitives::B256;
use clap::Parser;
use eyre::bail;
use reth_chainspec::ChainSpec;
use reth_cli_commands::common::{AccessRights, Environment, EnvironmentArgs};
use reth_consensus::FullConsensus;
use reth_db::DatabaseEnv;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{execute::Executor, ConfigureEvm};
use reth_evm_ethereum::EthEvmConfig;
use reth_execution_types::ExecutionOutcome;
use reth_node_ethereum::EthereumNode;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReader, BlockWriter, ChainSpecProvider, DBProvider,
    DatabaseProviderFactory, HeaderProvider, HistoryWriter, OriginalValuesKnown, ProviderFactory,
    StageCheckpointReader, StageCheckpointWriter, StateWriteConfig, StateWriter,
    StaticFileProviderFactory, StaticFileWriter, StorageSettingsCache, TransactionVariant,
    TrieWriter,
};
use reth_revm::{database::StateProviderDatabase, witness::ExecutionWitnessRecord};
use reth_stages_types::{StageCheckpoint, StageId};
use reth_storage_api::{HashedPostStateProvider, StateRootProvider};
use reth_trie::ExecutionWitnessMode;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use tracing::{info, warn};

mod blocks;
use blocks::load_block_from_disk;

/// The provider factory `EnvironmentArgs` hands back for an Ethereum node.
type Factory = ProviderFactory<NodeTypesWithDBAdapter<EthereumNode, DatabaseEnv>>;

/// The witness format the harness measures. `Legacy` is what `debug_executionWitness` emits today.
const WITNESS_MODE: ExecutionWitnessMode = ExecutionWitnessMode::Legacy;

/// Stages that must all sit at the same height for the plain state, the hashed state and the tries
/// to describe the same block. A snapshot taken mid-pipeline will not satisfy this, and replaying
/// on top of it silently produces wrong state roots and meaningless witnesses.
const ALIGNED_STAGES: &[StageId] = &[
    StageId::Execution,
    StageId::AccountHashing,
    StageId::StorageHashing,
    StageId::MerkleExecute,
    StageId::Finish,
];

#[derive(Debug, Parser)]
#[command(author, about = "Measure the marginal cost of attaching an execution witness")]
struct Args {
    #[command(flatten)]
    env: EnvironmentArgs<EthereumChainSpecParser>,

    /// First block to replay. Must be `tip + 1`. Not needed with `--check-tip-root`.
    #[arg(long)]
    from: Option<u64>,

    /// Last block to replay (inclusive). Defaults to `--from`.
    #[arg(long)]
    to: Option<u64>,

    /// Run the witness pass this many times per block.
    ///
    /// Only the witness pass repeats. The block is executed once and persisted once; every
    /// iteration re-walks the trie against the same executed state and the same parent provider,
    /// so the repeats are directly comparable and composable with `--commit`.
    ///
    /// Iteration 1 is the cold measurement. Later ones run with the trie nodes already in the page
    /// cache, which is the regime a builder producing a second candidate at the same parent is
    /// actually in. The gap between them is how much of the witness cost is I/O rather than work.
    #[arg(long, default_value_t = 1)]
    repeat: u32,

    /// Persist each block and advance the database tip.
    ///
    /// IRREVERSIBLE. Required for ranges longer than one block, because block N+1 can only be
    /// measured once N's post-state is the latest state.
    #[arg(long)]
    commit: bool,

    /// Also write history indices when persisting.
    ///
    /// Not needed for a forward-only replay; only matters if you later want historical state at a
    /// block the run has already passed. Costs time per block, so it is off by default.
    #[arg(long)]
    history_indices: bool,

    /// Serialize the witness to JSON and time it, to separate serialization from the trie pass.
    /// JSON because that is the form a witness actually leaves the node in over `debug_`.
    #[arg(long)]
    encode_witness: bool,

    /// Verify the computed state root against the block header. Costs nothing extra: the root is
    /// computed either way.
    #[arg(long, default_value_t = true)]
    verify_root: bool,

    /// Read blocks from this directory instead of from the node's own storage.
    ///
    /// One file per block, named either `<number>.json` — an `eth_getBlockByNumber(n, true)`
    /// response, saved verbatim — or `<number>.rlp`, an RLP-encoded block. JSON is preferred: the
    /// loader checks the reconstructed block against the hash the RPC reported, which proves the
    /// header survived the round trip, and checks the body against that header, which proves the
    /// transactions did. Senders are recovered on load, inside `us_fetch`.
    ///
    /// With `--commit` the block is also inserted into storage if it is not already there, which
    /// is required: later blocks resolve `BLOCKHASH` and their ancestor header range against what
    /// is on disk.
    #[arg(long)]
    blocks_dir: Option<PathBuf>,

    /// Write one JSON object per measurement here. Defaults to stdout.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Also save each block's witness to `<dir>/<number>.json.zst`.
    ///
    /// Written once per block regardless of `--repeat`, and timed as its own phase so the
    /// compression and the disk write never land in the marginal-cost number. Budget a few MB per
    /// block before compression.
    #[arg(long)]
    witness_dir: Option<PathBuf>,

    /// zstd level for `--witness-dir`. 3 is the zstd default and is roughly free; higher levels
    /// cost real CPU per block for a few percent of size.
    #[arg(long, default_value_t = 3)]
    witness_zstd_level: i32,

    /// Also write receipts and account/storage changesets when persisting.
    ///
    /// Off by default because a forward-only replay never reads any of them, and writing them is
    /// not free. On storage v2 leaving this off also takes a purpose-built fast path in
    /// `write_state` (`providers/database/provider.rs:2559`): with all three disabled it writes
    /// only bytecodes and skips `to_plain_state_and_reverts`, which otherwise iterates every
    /// touched account and storage slot. The canonical state still lands, via `write_hashed_state`
    /// on v2 and via `write_state_changes` on v1.
    ///
    /// Turn it on if you want the datadir to stay unwindable past the replayed range.
    #[arg(long)]
    write_side_tables: bool,

    /// Execute the block this many times against the parent *before* generating the witness,
    /// discarding the results.
    ///
    /// Models a builder that has already executed these transactions as part of building the block
    /// it is about to witness — the realistic case, and the upper bound on how much prior work can
    /// warm the witness. It reads through the same `StateProviderDatabase` the witness does, so it
    /// warms the same memory-mapped pages.
    ///
    /// Timed separately as `us_pre_execute` and never folded into any witness figure.
    #[arg(long, default_value_t = 0)]
    pre_execute: u32,

    /// During `--pre-execute`, also compute the state root, as a builder does when sealing.
    ///
    /// This is the half that matters. Executing a block reads plain and hashed state *leaves*; it
    /// never walks `AccountsTrie` / `StoragesTrie`. Only the state-root computation does — through
    /// the same `DatabaseTrieCursorFactory` the witness uses. Since the trie step is ~99% of witness
    /// cost, this flag is the difference between a warm-up that touches the right pages and one
    /// that does not.
    ///
    /// `--pre-execute` alone models simulation; with this, a full build.
    #[arg(long)]
    pre_root: bool,

    /// Skip the stage-alignment preflight. You almost certainly do not want this.
    #[arg(long)]
    skip_preflight: bool,

    /// Open the datadir without the static-file/database consistency check.
    ///
    /// For diagnostics only. Without this, opening a datadir that fails the check either errors
    /// (read-only) or silently runs a healing unwind (read-write) — neither of which you want
    /// while investigating what went wrong.
    #[arg(long)]
    allow_inconsistent: bool,

    /// Recompute the state root of the current tip from the trie tables and compare it against
    /// that block's header, then exit. Read-only.
    ///
    /// This is the one check that says whether a snapshot is usable at all. Every witness and
    /// every state root the harness produces is derived from `AccountsTrie` / `StoragesTrie`, so
    /// if those do not already hash to the tip's header root, nothing downstream can be right.
    #[arg(long)]
    check_tip_root: bool,
}

/// One measurement of one block.
#[derive(Debug, Default)]
struct Sample {
    number: u64,
    gas_used: u64,
    tx_count: usize,
    witness_nodes: usize,
    witness_bytes: usize,
    codes: usize,
    codes_bytes: usize,
    keys: usize,
    headers: usize,
    /// How far back `BLOCKHASH` reached, in blocks. Zero if it was never called.
    blockhash_depth: u64,
    encoded_bytes: usize,
    /// Bytes actually written by `--witness-dir`, after compression.
    saved_bytes: usize,
    state_root_ok: bool,
    computed_state_root: B256,
    expected_state_root: B256,
    t_fetch: Duration,
    t_exec: Duration,
    /// One entry per `--repeat`. The first is cold; the rest run against a warm page cache.
    t_witness: Vec<Duration>,

    t_encode: Duration,
    t_state_root: Duration,
    t_persist: Duration,
    t_pre_execute: Duration,
    pre_root: bool,
    t_save: Duration,
}

impl Sample {
    /// The cold witness pass — the first and only one a builder gets for a given parent.
    fn witness_cold(&self) -> Duration {
        self.t_witness.first().copied().unwrap_or_default()
    }

    /// The floor the witness pass converges to once the trie nodes it touches are cached.
    ///
    /// `None` with `--repeat 1`, since a single pass says nothing about the warm case.
    fn witness_warm(&self) -> Option<Duration> {
        self.t_witness.get(1..).and_then(|rest| rest.iter().min().copied())
    }

    /// Everything the builder would not otherwise have paid.
    fn marginal(&self) -> Duration {
        self.witness_cold() + self.t_encode
    }

    /// One JSON row per witness run — long format, one line per `--repeat` iteration.
    ///
    /// `us_witness` is the only per-iteration field. Everything else is a property of the block and
    /// was measured once; it is repeated on every row so each line stands alone, which means
    /// aggregating those columns across rows double-counts. Group by `number` and take the first.
    fn rows(&self) -> Vec<serde_json::Value> {
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        let repeats = self.t_witness.len();

        self.t_witness
            .iter()
            .enumerate()
            .map(|(index, elapsed)| {
                serde_json::json!({
                    "number": self.number,
                    "iteration": index + 1,
                    "repeats": repeats,
                    // The measurement this row exists for.
                    "us_witness": us(*elapsed),
                    // True for the one pass that had to fault its trie nodes in from disk.
                    "cold": index == 0,

                    "gas_used": self.gas_used,
                    "tx_count": self.tx_count,
                    "witness_nodes": self.witness_nodes,
                    "witness_bytes": self.witness_bytes,
                    "codes": self.codes,
                    "codes_bytes": self.codes_bytes,
                    "keys": self.keys,
                    "headers": self.headers,
                    "blockhash_depth": self.blockhash_depth,
                    "encoded_bytes": self.encoded_bytes,
                    "saved_bytes": self.saved_bytes,
                    "state_root_ok": self.state_root_ok,
                    "computed_state_root": self.computed_state_root.to_string(),
                    "expected_state_root": self.expected_state_root.to_string(),

                    "us_fetch": us(self.t_fetch),
                    "us_exec": us(self.t_exec),
                    "us_encode": us(self.t_encode),
                    "us_state_root": us(self.t_state_root),
                    "us_persist": us(self.t_persist),
                    "us_pre_execute": us(self.t_pre_execute),
                    "pre_root": self.pre_root,
                    "us_save": us(self.t_save),

                    "us_witness_cold": us(self.witness_cold()),
                    "us_witness_warm": self.witness_warm().map(us),
                    "us_marginal": us(self.marginal()),
                    // The headline ratio: how much of the trie pass duplicates work the builder
                    // already does for the header's state root.
                    //
                    // Read it with the ordering in mind. The witness pass runs first and pulls the
                    // trie nodes into the page cache; the state root then walks much of the same
                    // trie warm. So this ratio flatters the state root, and the warm variant below
                    // is the fairer cache-equalized comparison.
                    "witness_over_state_root": self.witness_cold().as_secs_f64() /
                        self.t_state_root.as_secs_f64(),
                    "witness_warm_over_state_root": self.witness_warm()
                        .map(|warm| warm.as_secs_f64() / self.t_state_root.as_secs_f64()),
                })
            })
            .collect()
    }
}

fn main() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let args = Args::parse();

    let access = match (args.commit, args.allow_inconsistent) {
        (true, false) => AccessRights::RW,
        // v2.2.0 has no read-write inconsistent variant; a committing run must pass the check.
        (true, true) => AccessRights::RW,
        (false, false) => AccessRights::RO,
        (false, true) => AccessRights::RoInconsistent,
    };
    // `init` needs a runtime for parallel storage I/O. Nothing here is async, so a default one
    // built up front is enough; it is dropped when `main` returns.
    let runtime = reth_tasks::RuntimeBuilder::new(reth_tasks::RuntimeConfig::default()).build()?;
    let Environment { provider_factory, .. } = args.env.init::<EthereumNode>(access, runtime)?;

    if args.check_tip_root {
        return check_tip_root(&provider_factory);
    }

    let from = args.from.ok_or_else(|| eyre::eyre!("--from is required"))?;
    let to = args.to.unwrap_or(from);
    if to < from {
        bail!("--to ({to}) is before --from ({from})");
    }
    if to > from && !args.commit {
        bail!(
            "replaying {from}..={to} requires --commit: without advancing the tip every block \
             after the first would be executed against the wrong parent state"
        );
    }
    if args.repeat == 0 {
        bail!("--repeat must be at least 1");
    }

    let chain_spec = provider_factory.chain_spec();
    let evm_config = EthEvmConfig::ethereum(chain_spec.clone());
    let consensus = EthBeaconConsensus::new(chain_spec);

    let tip = preflight(&provider_factory, from, args.skip_preflight)?;

    // Which write path a run took is the first thing you want to know when a state root later
    // disagrees, and it is not recoverable from the output otherwise.
    let storage_v2 = provider_factory.cached_storage_settings().storage_v2;
    info!(
        tip,
        from,
        to,
        commit = args.commit,
        storage_v2,
        side_tables = args.write_side_tables,
        canonical_state = if storage_v2 { "hashed (v2)" } else { "plain (v1)" },
        "starting replay"
    );

    let mut out: Box<dyn Write> = match &args.out {
        Some(path) => Box::new(BufWriter::new(File::create(path)?)),
        None => Box::new(std::io::stdout()),
    };

    // Fail here rather than after the first block's worth of work.
    if let Some(dir) = &args.witness_dir {
        std::fs::create_dir_all(dir)
            .map_err(|err| eyre::eyre!("cannot create {}: {err}", dir.display()))?;
    }

    for number in from..=to {
        let sample = measure(&provider_factory, &evm_config, &consensus, number, &args)?;

        for row in sample.rows() {
            writeln!(out, "{row}")?;
        }
        out.flush()?;

        if !sample.state_root_ok && args.verify_root {
            bail!(
                "state root mismatch at block {number}: the trie tables are not consistent \
                 with the plain state, every measurement after this point is meaningless"
            );
        }
    }

    Ok(())
}

/// Recomputes the tip's state root straight from the trie tables and compares it to the header.
///
/// With an empty [`HashedPostState`] the prefix sets are empty, so the walker only has to descend
/// the nodes the root actually depends on rather than rebuilding the whole trie.
fn check_tip_root(factory: &Factory) -> eyre::Result<()> {
    let provider = factory.database_provider_ro()?;
    let tip = provider.best_block_number()?;
    let header =
        provider.header_by_number(tip)?.ok_or_else(|| eyre::eyre!("no header for tip {tip}"))?;

    let state = factory.latest()?;
    let start = Instant::now();
    let computed = state.state_root(Default::default())?;
    let elapsed = start.elapsed();

    let expected = header.state_root;
    if computed == expected {
        info!(tip, ?elapsed, root = %computed, "trie tables match the tip header");
        Ok(())
    } else {
        bail!(
            "trie tables do NOT match the tip header at block {tip}\n  \
             computed {computed}\n  expected {expected}\n\
             The snapshot's tries are not consistent with its header chain, so no witness or \
             state root derived from them can be correct."
        )
    }
}

/// Checks that the datadir is in a state where lockstep replay is meaningful, and returns the tip.
fn preflight(factory: &Factory, from: u64, skip: bool) -> eyre::Result<u64> {
    let provider = factory.database_provider_ro()?;
    let tip = provider.best_block_number()?;

    if skip {
        warn!("preflight skipped");
        return Ok(tip);
    }

    // All of the execution-side stages must describe the same block. Reth's pipeline runs them in
    // sequence, so a snapshot captured between two of them leaves the plain state ahead of the
    // hashed state and the tries — which produces wrong state roots without any error.
    let mut heights = Vec::new();
    for stage in ALIGNED_STAGES {
        let height = provider
            .get_stage_checkpoint(*stage)?
            .map(|checkpoint| checkpoint.block_number)
            .unwrap_or_default();
        heights.push((stage.to_string(), height));
    }
    if heights.iter().any(|(_, height)| *height != heights[0].1) {
        bail!(
            "stage checkpoints are not aligned, this datadir was captured mid-pipeline: {heights:?}\n\
             Run the pipeline forward (or unwind) until all of these agree before replaying."
        );
    }

    if from != tip + 1 {
        bail!(
            "--from is {from} but the tip is {tip}; lockstep replay must start at tip + 1 ({}). \
             Starting anywhere else means executing against a state that is not the block's parent.",
            tip + 1
        );
    }

    Ok(tip)
}

/// Advances only the stage checkpoints that were exactly at the parent block.
///
/// `update_pipeline_stages` iterates [`StageId::ALL`] and upserts *every* stage to the given block
/// (`providers/database/provider.rs:2382`), which is wrong here in both directions:
///
/// - Stages deliberately left behind get dragged forward. A `reth download --minimal` snapshot
///   resets `TransactionLookup` / `IndexAccountHistory` / `IndexStorageHistory` to 0 because those
///   indices were never downloaded; advancing them claims they are built when they are not.
/// - Stages legitimately ahead get dragged back. A datadir whose Headers/Bodies ran past Execution
///   would have them pulled down to the replay height, which leaves static files looking ahead of
///   the database and fails the next consistency check.
///
/// Advancing exactly those stages that sat at `number - 1` preserves whatever shape the datadir
/// arrived in, and covers `Finish` by construction: the preflight requires `--from == tip + 1`, and
/// the tip *is* the `Finish` checkpoint.
fn advance_stage_checkpoints(
    provider: &(impl StageCheckpointReader + StageCheckpointWriter),
    number: u64,
) -> eyre::Result<Vec<StageId>> {
    let parent = number - 1;
    let mut advanced = Vec::new();

    for stage in StageId::ALL {
        let Some(checkpoint) = provider.get_stage_checkpoint(stage)? else { continue };
        if checkpoint.block_number != parent {
            continue;
        }
        provider
            .save_stage_checkpoint(stage, StageCheckpoint { block_number: number, ..checkpoint })?;
        advanced.push(stage);
    }

    Ok(advanced)
}

/// Replays one block and times each phase separately.
fn measure(
    factory: &Factory,
    evm_config: &EthEvmConfig,
    consensus: &EthBeaconConsensus<ChainSpec>,
    number: u64,
    args: &Args,
) -> eyre::Result<Sample> {
    let mut sample = Sample { number, ..Default::default() };

    let start = Instant::now();
    let block = match &args.blocks_dir {
        Some(dir) => load_block_from_disk(dir, number)?,
        None => factory
            .recovered_block(number.into(), TransactionVariant::NoHash)?
            .ok_or_else(|| eyre::eyre!("block {number} not found — no header/body on disk"))?,
    };
    sample.t_fetch = start.elapsed();

    sample.gas_used = block.header().gas_used();
    sample.tx_count = block.body().transactions().count();

    // Warm-up pass, if asked for. Deliberately uses its own state provider and executor so the
    // measured path below starts exactly as it otherwise would; the only thing carried over is
    // page-cache residency, which is the whole point.
    if args.pre_execute > 0 {
        let start = Instant::now();
        for _ in 0..args.pre_execute {
            let pre_state = factory.latest()?;
            let pre_db = StateProviderDatabase::new(&pre_state);
            let output =
                evm_config.batch_executor(pre_db).execute_with_state_closure(&block, |_| {})?;

            if args.pre_root {
                let post = pre_state.hashed_post_state(&output.state);
                let _ = pre_state.state_root_with_updates(post)?;
            }
        }
        sample.t_pre_execute = start.elapsed();
        sample.pre_root = args.pre_root;
    }

    // Parent state. In lockstep this is `LatestStateProviderRef` over the plain state and trie
    // tables directly — no changeset overlay, which is exactly a builder's situation at the head.
    let state = factory.latest()?;
    let db = StateProviderDatabase::new(&state);
    let executor = evm_config.batch_executor(db);

    // Execution and witness generation share one pass. The closure runs once, after execution,
    // against the final `State`, so timing it inside gives us a clean split.
    //
    // `--repeat` loops here rather than around the whole block. Only the witness pass is worth
    // repeating: it reads `&State` and the parent provider and mutates neither, so every iteration
    // sees identical inputs and produces an identical witness. Execution and persistence are not
    // repeatable in the same sense — the block advances the tip exactly once — which is why the
    // repeat lives inside the closure instead of wrapping `measure`.
    //
    // The closure cannot fail, so the `ProviderResult` is carried out and unwrapped below rather
    // than swallowed. `witness` stays `None` only if the executor never ran the closure at all.
    let mut witness = None;
    let mut shapes = Vec::with_capacity(args.repeat as usize);
    let mut t_witness = Vec::with_capacity(args.repeat as usize);
    let start = Instant::now();
    let output = executor.execute_with_state_closure(&block, |statedb| {
        for _ in 0..args.repeat {
            let started = Instant::now();
            // The per-step breakdown and the page-fault counters come from the shipped function
            // itself, which emits them on `reth::witness::timing` under RETH_WITNESS_TIMING. There
            // is deliberately no reimplementation here to drift out of sync with it.
            let generated = ExecutionWitnessRecord::from_executed_state(statedb, WITNESS_MODE)
                .into_execution_witness(&state, factory, number, WITNESS_MODE)
                .map_err(eyre::Report::from);
            t_witness.push(started.elapsed());

            // Cheap determinism check across repeats: the same state and the same provider must
            // yield the same witness shape every time. A drift here would mean the trie walk
            // depends on something other than its inputs, which would invalidate every timing.
            shapes.push(
                generated
                    .as_ref()
                    .ok()
                    .map(|w| (w.state.len(), w.codes.len(), w.keys.len(), w.headers.len())),
            );
            witness = Some(generated);
        }
    })?;
    let t_witness_total: Duration = t_witness.iter().sum();
    sample.t_witness = t_witness;
    sample.t_exec = start.elapsed().saturating_sub(t_witness_total);

    if let Some(first) = shapes.first() {
        if let Some(pos) = shapes.iter().position(|shape| shape != first) {
            bail!(
                "witness generation is not deterministic for block {number}: repeat 1 produced \
                 {first:?} but repeat {} produced {:?}",
                pos + 1,
                shapes[pos]
            );
        }
    }

    FullConsensus::<EthPrimitives>::validate_block_post_execution(
        consensus,
        &block,
        &output.result,
        None,
    )?;

    let witness = witness
        .ok_or_else(|| eyre::eyre!("executor never ran the state closure for block {number}"))??;

    sample.witness_nodes = witness.state.len();
    sample.witness_bytes = witness.state.iter().map(|node| node.len()).sum();
    sample.codes = witness.codes.len();
    sample.codes_bytes = witness.codes.iter().map(|code| code.len()).sum();
    sample.keys = witness.keys.len();
    sample.headers = witness.headers.len();
    // The header range is `lowest..number`, so one header means BLOCKHASH never reached past the
    // parent. Anything beyond that is how far back it went.
    sample.blockhash_depth = (witness.headers.len() as u64).saturating_sub(1);

    // Serialize once if either flag wants it, but only charge `t_encode` when `--encode-witness`
    // asked for the measurement — otherwise saving witnesses would silently inflate the marginal
    // cost with work a builder does not do.
    let encoded = if args.encode_witness || args.witness_dir.is_some() {
        // JSON, because that is how a witness actually leaves the node over `debug_`.
        let start = Instant::now();
        let encoded = serde_json::to_vec(&witness)?;
        let elapsed = start.elapsed();
        if args.encode_witness {
            sample.t_encode = elapsed;
            sample.encoded_bytes = encoded.len();
        }
        Some(encoded)
    } else {
        None
    };

    // Written once per block, not once per repeat: the witness is identical across iterations, and
    // this is harness bookkeeping rather than anything a builder pays for.
    if let (Some(dir), Some(encoded)) = (&args.witness_dir, &encoded) {
        let path = dir.join(format!("{number}.json.zst"));
        let start = Instant::now();
        let compressed = zstd::encode_all(encoded.as_slice(), args.witness_zstd_level)
            .map_err(|err| eyre::eyre!("cannot compress the witness for block {number}: {err}"))?;
        std::fs::write(&path, &compressed)
            .map_err(|err| eyre::eyre!("cannot write {}: {err}", path.display()))?;
        sample.t_save = start.elapsed();
        sample.saved_bytes = compressed.len();
    }

    // The state root the builder needs for the header regardless of any witness.
    let post_state = state.hashed_post_state(&output.state);
    let start = Instant::now();
    let (root, trie_updates) = state.state_root_with_updates(post_state.clone())?;
    sample.t_state_root = start.elapsed();
    let expected_root = block.header().state_root();
    sample.state_root_ok = root == expected_root;
    sample.computed_state_root = root;
    sample.expected_state_root = expected_root;

    if args.commit {
        if !sample.state_root_ok {
            bail!(
                "refusing to persist block {number} with a mismatched state root\n  \
                 computed {root}\n  expected {expected_root}\n\
                 The parent state this was executed against is not the state block {number} \
                 commits to. Check the tip with --check-tip-root --allow-inconsistent: if the tip's \
                 own root is good the divergence is in this block's execution, and if it is bad the \
                 previous block's persist did not write a consistent trie."
            );
        }

        let start = Instant::now();
        let provider_rw = factory.database_provider_rw()?;

        // A block that came from `--blocks-dir` is not on disk yet, and it has to be: the next
        // block resolves `BLOCKHASH` and its ancestor header range against storage. A block read
        // out of storage is already there, and inserting it again would duplicate the static file
        // entries — which is why this harness does not use the engine's `save_blocks`, whose first
        // step is an unconditional `insert_block`.
        //
        // Must happen before `write_state`: receipts are looked up by body indices.
        // Only insert what is genuinely absent. A datadir whose Headers/Bodies stages ran ahead of
        // Execution — which is exactly what a snapshot captured mid-pipeline looks like — already
        // holds some of the range, and inserting those again would duplicate the static file
        // entries. A block present under a different hash is a fork, not a duplicate, and must not
        // be papered over.
        if args.blocks_dir.is_some() {
            match provider_rw.block_hash(number)? {
                Some(stored) if stored == block.hash() => {}
                Some(stored) => bail!(
                    "block {number} is already in storage as {stored}, but the blocks-dir has \
                     {}. Replaying a different block on top of this datadir would fork it.",
                    block.hash()
                ),
                None => {
                    provider_rw.insert_block(&block)?;
                }
            }
        }

        let outcome = ExecutionOutcome::single(number, output);
        let write_config = StateWriteConfig {
            write_receipts: args.write_side_tables,
            write_account_changesets: args.write_side_tables,
            write_storage_changesets: args.write_side_tables,
        };
        provider_rw.write_state(&outcome, OriginalValuesKnown::No, write_config)?;
        // Written unconditionally, but for different reasons per layout. On v1 the plain state is
        // canonical and this harness stands in for the hashing stages, which nothing else runs. On
        // v2 the hashed tables *are* the canonical state — `write_state` deliberately skips the
        // plain tables (`use_hashed_state()`), so this call carries the entire state transition.
        // Either way the next block's state root reads them.
        provider_rw.write_hashed_state(&post_state.into_sorted())?;
        provider_rw.write_trie_updates(trie_updates)?;
        if args.history_indices {
            provider_rw.update_history_indices(number..=number)?;
        }
        // Moves the tip: `best_block_number` reads the Finish checkpoint, and that is what decides
        // whether the next block gets `LatestStateProvider` or a historical overlay.
        advance_stage_checkpoints(&provider_rw, number)?;

        let static_file_provider = factory.static_file_provider();
        static_file_provider.commit()?;
        provider_rw.commit()?;
        sample.t_persist = start.elapsed();
    }

    Ok(sample)
}
