//! `witness-engine` — drives a live reth node through the Engine API and measures what
//! `debug_executionWitness` costs it.
//!
//! This is the "Route A" counterpart to `witness-bench`. Where that harness reproduces the persist
//! path in process, this one hands blocks to a real node and lets the engine own persistence, which
//! removes every class of bookkeeping bug that comes from assembling `insert_block` /
//! `write_state` / `write_trie_updates` / stage checkpoints by hand — and gets storage-v2 static
//! file routing right for free.
//!
//! Per block, in this order:
//!
//! 1. `engine_newPayloadV4` — the node executes and validates the block.
//! 2. `engine_forkchoiceUpdatedV3` — **required**; without it the block is not on the canonical
//!    in-memory chain and step 3 cannot find it.
//! 3. `debug_executionWitness` — the measurement.
//!
//! Run the node with `--engine.persistence-threshold 0 --engine.memory-block-buffer-target 0` so
//! every block reaches disk before the next one arrives, and with
//! `--engine.accept-execution-requests-hash` so the header's `requests_hash` can be passed instead
//! of reconstructing execution requests without a consensus layer.
//!
//! What this binary times is end-to-end RPC latency. The phase breakdown — and the split between
//! work and I/O — comes from the node's own `reth::witness::timing` debug events, which are emitted
//! from inside `into_execution_witness` and the RPC handler. Start the node with
//! `RETH_WITNESS_TIMING=1` and `--log.stdout.filter reth::witness::timing=debug` to collect them.

use alloy_consensus::BlockHeader;
use alloy_rpc_types_engine::{Claims, ExecutionPayload, JwtSecret};
use clap::Parser;
use eyre::{bail, Context};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

mod blocks;
use blocks::load_block_from_disk;

#[derive(Debug, Parser)]
#[command(
    author,
    about = "Drive a reth node through the Engine API and measure debug_executionWitness"
)]
struct Args {
    /// Directory of `<number>.json` blocks, as produced by `fetch-blocks.sh`.
    #[arg(long)]
    blocks_dir: PathBuf,

    /// First block to submit. Must be the node's current head + 1.
    #[arg(long)]
    from: u64,

    /// Last block to submit (inclusive). Defaults to `--from`.
    #[arg(long)]
    to: Option<u64>,

    /// Authenticated engine endpoint.
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    engine_url: String,

    /// Public JSON-RPC endpoint, where the `debug` namespace must be enabled.
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    rpc_url: String,

    /// Path to the node's `jwt.hex`.
    #[arg(long)]
    jwt: PathBuf,

    /// Call `debug_executionWitness` this many times per block.
    ///
    /// The first call is cold. Later ones re-read the same trie with the node's caches already
    /// warm, so the gap between them is the share of the cost that is I/O rather than work.
    #[arg(long, default_value_t = 1)]
    witness_repeat: u32,

    /// Write one JSON object per block here. Defaults to stdout.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Submit blocks but skip the witness call, to measure the driver's own floor.
    #[arg(long)]
    skip_witness: bool,
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let to = args.to.unwrap_or(args.from);
    if to < args.from {
        bail!("--to ({to}) is before --from ({})", args.from);
    }

    let secret = JwtSecret::from_file(&args.jwt)
        .map_err(|err| eyre::eyre!("cannot read {}: {err}", args.jwt.display()))?;
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(600)).build()?;

    let mut out: Box<dyn Write> = match &args.out {
        Some(path) => Box::new(BufWriter::new(File::create(path)?)),
        None => Box::new(std::io::stdout()),
    };

    for number in args.from..=to {
        let block = load_block_from_disk(&args.blocks_dir, number)?;
        let hash = block.hash();
        let gas_used = block.header().gas_used();
        // Passing the hash rather than the requests array is what makes replay possible with no
        // consensus layer attached; the node must be started with
        // `--engine.accept-execution-requests-hash`.
        let requests_hash = block
            .header()
            .requests_hash()
            .ok_or_else(|| eyre::eyre!("block {number} has no requests hash"))?;

        // Read out everything needed before consuming the recovered block: `from_block_slow` wants
        // the consensus block, and taking it by value avoids cloning a full block per iteration.
        let block = block.into_block();
        let tx_count = block.body.transactions.len();

        // `from_block_slow` recomputes the payload from the block, including re-encoding every
        // transaction to the raw RLP the Engine API expects — which is why the block cannot simply
        // be forwarded as the JSON we fetched it as.
        let (payload, sidecar) = ExecutionPayload::from_block_slow(&block);
        let ExecutionPayload::V3(payload) = payload else {
            bail!(
                "block {number} did not convert to an ExecutionPayloadV3; a fork boundary here \
                   means newPayloadV4 is the wrong method for it"
            );
        };

        let versioned_hashes = sidecar.versioned_hashes().cloned().unwrap_or_default();
        let parent_beacon_block_root = sidecar
            .parent_beacon_block_root()
            .ok_or_else(|| eyre::eyre!("block {number} has no parent beacon block root"))?;
        let start = Instant::now();
        let status = rpc(
            &client,
            &args.engine_url,
            Some(&secret),
            "engine_newPayloadV4",
            json!([payload, versioned_hashes, parent_beacon_block_root, requests_hash]),
        )?;
        let us_new_payload = start.elapsed();

        let payload_status = status.get("status").and_then(Value::as_str).unwrap_or("<none>");
        if payload_status != "VALID" {
            bail!("block {number} newPayload returned {payload_status}: {status}");
        }

        // Without this the block exists but is not canonical, and `debug_executionWitness` cannot
        // resolve it by number.
        let start = Instant::now();
        let fcu = rpc(
            &client,
            &args.engine_url,
            Some(&secret),
            "engine_forkchoiceUpdatedV3",
            json!([
                { "headBlockHash": hash, "safeBlockHash": hash, "finalizedBlockHash": hash },
                null
            ]),
        )?;
        let us_fcu = start.elapsed();

        let fcu_status =
            fcu.get("payloadStatus").and_then(|s| s.get("status")).and_then(Value::as_str);
        if fcu_status != Some("VALID") {
            bail!("block {number} forkchoiceUpdated returned {fcu_status:?}: {fcu}");
        }

        let mut witness_us = Vec::new();
        let mut shape = json!(null);
        if !args.skip_witness {
            for iteration in 0..args.witness_repeat {
                let start = Instant::now();
                let witness = rpc(
                    &client,
                    &args.rpc_url,
                    None,
                    "debug_executionWitness",
                    json!([format!("0x{number:x}")]),
                )?;
                witness_us.push(start.elapsed());

                if iteration == 0 {
                    let len =
                        |key: &str| witness.get(key).and_then(Value::as_array).map_or(0, Vec::len);
                    let bytes = |key: &str| {
                        witness.get(key).and_then(Value::as_array).map_or(0, |a| {
                            // Hex strings, "0x" plus two characters per byte.
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(|s| s.len().saturating_sub(2) / 2)
                                .sum()
                        })
                    };
                    shape = json!({
                        "nodes": len("state"),
                        "state_bytes": bytes("state"),
                        "codes": len("codes"),
                        "codes_bytes": bytes("codes"),
                        "keys": len("keys"),
                        "headers": len("headers"),
                    });
                }
            }
        }

        let us = |d: Duration| d.as_secs_f64() * 1e6;
        writeln!(
            out,
            "{}",
            json!({
                "number": number,
                "hash": hash,
                "gas_used": gas_used,
                "tx_count": tx_count,
                "us_new_payload": us(us_new_payload),
                "us_fcu": us(us_fcu),
                "us_witness": witness_us.first().copied().map(us),
                "us_witness_iters": witness_us.iter().copied().map(us).collect::<Vec<_>>(),
                // Warm floor: the cheapest repeat after the first. The gap from `us_witness` is the
                // part of the cost that was faulting data in rather than computing.
                "us_witness_warm": witness_us.get(1..).and_then(|rest| rest.iter().min().copied()).map(us),
                "witness": shape,
            })
        )?;
        out.flush()?;
    }

    Ok(())
}

/// One JSON-RPC round trip. `secret` set means the authenticated engine port, which needs a fresh
/// bearer token per call — the claim window is narrow and encoding is cheap.
fn rpc(
    client: &reqwest::blocking::Client,
    url: &str,
    secret: Option<&JwtSecret>,
    method: &str,
    params: Value,
) -> eyre::Result<Value> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let mut request = client.post(url).json(&body);

    if let Some(secret) = secret {
        let token = secret
            .encode(&Claims::with_current_timestamp())
            .wrap_err("cannot sign the engine API claim")?;
        request = request.bearer_auth(token);
    }

    let response: Value =
        request.send().wrap_err_with(|| format!("{method} to {url} failed"))?.json()?;

    if let Some(error) = response.get("error") {
        bail!("{method} returned an error: {error}");
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| eyre::eyre!("{method} returned no result: {response}"))
}
