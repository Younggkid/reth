//! Loading blocks from a `--blocks-dir` dataset, shared by both binaries in this crate.
//!
//! Kept separate because the engine driver and the in-process harness must agree exactly on what a
//! block is and on which checks it passed; a second copy of this would be a second definition of
//! "verified block".

use alloy_primitives::B256;
use alloy_rlp::Decodable;
use eyre::bail;
use reth_consensus_common::validation::validate_body_against_header;
use reth_ethereum_primitives::Block;
use reth_primitives_traits::{Block as _, RecoveredBlock};
use std::path::Path;

/// Loads `<dir>/<number>.rlp` and recovers its senders.
///
/// Sender recovery is real work — a few ms for a full block — that the node's own storage has
/// already done and cached in `TransactionSenders`. It is part of `us_fetch` here, and is neither
/// builder cost nor witness cost; a builder recovers senders once, when the transaction enters the
/// pool.
pub(crate) fn load_block_from_disk(dir: &Path, number: u64) -> eyre::Result<RecoveredBlock<Block>> {
    let rlp = dir.join(format!("{number}.rlp"));
    let json = dir.join(format!("{number}.json"));

    let (block, reported_hash) = if rlp.exists() {
        let bytes = std::fs::read(&rlp)
            .map_err(|err| eyre::eyre!("cannot read {}: {err}", rlp.display()))?;
        let block = Block::decode(&mut bytes.as_slice())
            .map_err(|err| eyre::eyre!("{} is not an RLP-encoded block: {err}", rlp.display()))?;
        (block, None)
    } else if json.exists() {
        decode_rpc_block(&json)?
    } else {
        bail!("no {} and no {}", rlp.display(), json.display());
    };

    if block.header.number != number {
        bail!("the file for block {number} actually holds block {}", block.header.number);
    }

    // Two checks, because neither covers the other.
    //
    // The hash proves the *header* survived the round trip: every header field feeds it, so a
    // match means nothing was lost or mis-encoded. It says nothing about the body — `hash_slow`
    // hashes the `transactions_root` that came out of the JSON, it does not recompute it from the
    // transactions we decoded. A truncated or reordered transaction list passes it untouched and
    // then surfaces thousands of blocks later as an unexplained state root mismatch.
    if let Some(reported) = reported_hash {
        let computed = block.header.hash_slow();
        if computed != reported {
            bail!(
                "block {number} does not hash to what the RPC reported\n  computed {computed}\n  \
                 reported {reported}\nThe JSON decoded into a block that is not the block it \
                 claims to be."
            );
        }
    }

    // So tie the body back to the header as well: transactions root, withdrawals root, ommers
    // hash. This is the check that makes the decoded transaction list trustworthy, and it is the
    // same one the consensus layer applies to a body fetched from a peer.
    validate_body_against_header(block.body(), &block.header).map_err(|err| {
        eyre::eyre!(
            "block {number} body does not match its header: {err}\nThe transactions decoded from \
             the JSON are not the transactions this block commits to."
        )
    })?;

    block
        .try_into_recovered()
        .map_err(|_| eyre::eyre!("failed to recover senders for block {number}"))
}

/// Decodes an `eth_getBlockByNumber(.., true)` response into a consensus block.
///
/// Accepts either the whole JSON-RPC envelope or just the `result` object, so a plain
/// `curl … > 123.json` works with no post-processing.
pub(crate) fn decode_rpc_block(path: &Path) -> eyre::Result<(Block, Option<B256>)> {
    let raw =
        std::fs::read(path).map_err(|err| eyre::eyre!("cannot read {}: {err}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|err| eyre::eyre!("{} is not JSON: {err}", path.display()))?;

    if let Some(error) = value.get("error") {
        bail!("{} holds a JSON-RPC error: {error}", path.display());
    }
    let result = value.get("result").cloned().unwrap_or(value);
    if result.is_null() {
        bail!("{} holds a null result — that node does not have this block", path.display());
    }

    let rpc_block: alloy_rpc_types_eth::Block = serde_json::from_value(result)
        .map_err(|err| eyre::eyre!("{} is not an RPC block: {err}", path.display()))?;

    let hash = rpc_block.header.hash;
    let block = rpc_block.into_consensus().map_transactions(|tx| tx.into_inner().into());
    Ok((block, Some(hash)))
}
