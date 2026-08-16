#!/usr/bin/env bash
# Fetches execution blocks for witness-bench's --blocks-dir, and checks them against Xatu.
#
# Two sources, because neither one is sufficient on its own:
#
#   * JSON-RPC `eth_getBlockByNumber(n, true)` — the block itself. Saved verbatim as <n>.json; no
#     conversion happens here on purpose, witness-bench decodes the JSON itself.
#
#   * Xatu `canonical_beacon_block` — a manifest of what each block *should* be. Xatu is an
#     analytics dataset: it has no calldata, no signatures and no prev_randao / receipts_root /
#     logs_bloom, so a block cannot be reconstructed from it. What it does have is the canonical
#     execution-payload identity — hash, parent hash, state root, fee recipient, gas used/limit,
#     base fee, blob gas used/excess and the transaction count — derived from the finalized beacon
#     chain rather than from whatever node the RPC endpoint round-robins us onto.
#
# That split matters. A public RPC endpoint is a load balancer over nodes that may disagree, and a
# block body that decodes cleanly and hashes to the hash the RPC reported is still worthless if it
# is a reorged sibling. witness-bench's own hash check proves the JSON round trip was faithful; it
# cannot prove the block is canonical. The Xatu manifest is the independent second opinion, and it
# also gives the chain-of-parent-hashes check that a lockstep replay depends on: one off-chain
# block anywhere in the range kills the run when it is reached, hours in.
#
#   ./fetch-blocks.sh [from] [to] [outdir] [rpc-url] [parallelism]
#
# Defaults to the 24785395..24787000 range (all of which lives in Xatu's 2026/4/1 partition).
# Re-running skips blocks already present and verified, so it resumes after an interruption.
#
# The manifest step needs a `duckdb` binary (set $DUCKDB to point at one). Without it the fetch
# still works and only the weaker self-consistency checks run.

set -uo pipefail

FROM=${1:-24785395}
TO=${2:-24787000}
OUTDIR=${3:-./blocks}
RPC=${4:-https://ethereum-rpc.publicnode.com}
JOBS=${5:-4}

DUCKDB=${DUCKDB:-duckdb}
XATU=https://data.ethpandaops.io/xatu/mainnet/databases/default/canonical_beacon_block
MANIFEST="$OUTDIR/manifest.tsv"

mkdir -p "$OUTDIR"

rpc() {
    curl -sS --max-time 60 -X POST -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" "$RPC"
}

# ---------------------------------------------------------------------------- manifest

# Xatu partitions canonical_beacon_block daily on slot start time, so the range has to be mapped
# from block numbers to UTC days. Rather than assume a slot-to-block relation — missed slots break
# any such formula — ask the RPC for the first block's timestamp and walk days forward until the
# manifest reaches `to`. Paths are unpadded (2026/4/1, not 2026/04/01).
build_manifest() {
    if ! command -v "$DUCKDB" >/dev/null 2>&1; then
        echo "warning: no '$DUCKDB' on PATH — skipping the Xatu manifest, blocks will not be" >&2
        echo "         checked for canonicity. Set \$DUCKDB or install the duckdb CLI." >&2
        return 1
    fi

    local hex ts day urls=() covered=0 guard=0
    hex=$(printf '0x%x' "$FROM")
    ts=$(rpc eth_getBlockByNumber "[\"$hex\",false]" |
        grep -o '"timestamp":"0x[0-9a-f]*"' | head -1 | grep -o '0x[0-9a-f]*')
    if [[ -z ${ts:-} ]]; then
        echo "warning: could not read the timestamp of block $FROM — skipping the manifest" >&2
        return 1
    fi
    ts=$((ts))

    # One day per iteration; `guard` only stops a runaway loop if Xatu 404s forever.
    while ((covered < TO && guard < 32)); do
        day=$(date -u -d "@$((ts + guard * 86400))" +%Y/%-m/%-d)
        urls+=("'$XATU/$day.parquet'")
        # Cheap probe: a missing partition means the range runs past what Xatu has published.
        if ! curl -sfI --max-time 30 "$XATU/$day.parquet" >/dev/null; then
            echo "warning: Xatu has no partition for $day — manifest may be incomplete" >&2
            unset 'urls[-1]'
            break
        fi
        # Only the last day queried can extend coverage, so re-querying the whole set is wasteful;
        # ask this one partition for its highest block instead.
        covered=$("$DUCKDB" -noheader -list -c \
            "SELECT coalesce(max(execution_payload_block_number), 0)
             FROM read_parquet('$XATU/$day.parquet')" 2>/dev/null)
        covered=${covered:-0}
        ((guard++))
    done

    if ((${#urls[@]} == 0)); then
        echo "warning: no Xatu partitions found — skipping the manifest" >&2
        return 1
    fi

    local list
    list=$(IFS=,; echo "${urls[*]}")
    "$DUCKDB" -noheader -list -c "
        COPY (
            SELECT execution_payload_block_number AS number,
                   execution_payload_block_hash   AS hash,
                   execution_payload_parent_hash  AS parent_hash,
                   execution_payload_state_root   AS state_root,
                   execution_payload_gas_used     AS gas_used,
                   execution_payload_gas_limit    AS gas_limit,
                   execution_payload_fee_recipient AS fee_recipient,
                   -- UInt128, stored little-endian; decoded by the verifier below.
                   hex(execution_payload_base_fee_per_gas) AS base_fee_per_gas_le,
                   execution_payload_blob_gas_used    AS blob_gas_used,
                   execution_payload_excess_blob_gas  AS excess_blob_gas,
                   execution_payload_transactions_count AS transactions_count,
                   slot, slot_start_date_time AS timestamp
            FROM read_parquet([$list])
            WHERE execution_payload_block_number BETWEEN $FROM AND $TO
            ORDER BY number
        ) TO '$MANIFEST' (FORMAT CSV, DELIMITER '\t', HEADER);
    " >/dev/null || { echo "warning: manifest query failed" >&2; return 1; }

    local have want
    have=$(($(wc -l <"$MANIFEST") - 1))
    want=$((TO - FROM + 1))
    echo "manifest: $have / $want blocks from Xatu canonical_beacon_block"
    ((have == want))
}

# ---------------------------------------------------------------------------- fetch

fetch_one() {
    local n=$1 out="$OUTDIR/$1.json" hex
    # Already have it, and it is not a stub from a failed run.
    if [[ -s $out ]] && grep -q '"hash"' "$out"; then
        return 0
    fi
    hex=$(printf '0x%x' "$n")

    local attempt
    for attempt in 1 2 3 4 5; do
        if curl -sS --max-time 60 -X POST -H 'content-type: application/json' \
            --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBlockByNumber\",\"params\":[\"$hex\",true]}" \
            "$RPC" -o "$out.tmp" && grep -q '"hash"' "$out.tmp"; then
            mv "$out.tmp" "$out"
            return 0
        fi
        # Public endpoints rate-limit; back off rather than hammer.
        sleep $((attempt * 2))
    done

    rm -f "$out.tmp"
    echo "FAILED $n" >&2
    return 1
}
export -f fetch_one
export OUTDIR RPC

build_manifest
manifest_ok=$?

seq "$FROM" "$TO" | xargs -P "$JOBS" -I{} bash -c 'fetch_one {}'

# ---------------------------------------------------------------------------- verify

# Done in one pass at the end rather than per fetch: the parent-hash chain is a property of the
# whole range, and a single interpreter start beats 1600 of them. Anything that fails here is
# deleted, so re-running the script re-fetches exactly the broken blocks.
python3 - "$OUTDIR" "$FROM" "$TO" "$MANIFEST" <<'PY'
import json, os, sys

outdir, first, last, manifest_path = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]

manifest = {}
if os.path.exists(manifest_path):
    with open(manifest_path) as fh:
        cols = next(fh).rstrip("\n").split("\t")
        for line in fh:
            row = dict(zip(cols, line.rstrip("\n").split("\t")))
            manifest[int(row["number"])] = row

missing, bad = [], []
prev_hash = None

for n in range(first, last + 1):
    path = os.path.join(outdir, f"{n}.json")
    try:
        with open(path) as fh:
            body = json.load(fh)
    except (OSError, ValueError) as err:
        missing.append((n, str(err)))
        prev_hash = None
        continue

    block = body.get("result", body)
    if not isinstance(block, dict) or "hash" not in block:
        bad.append((n, f"no block in the response: {str(body)[:120]}"))
        os.remove(path)
        prev_hash = None
        continue

    problems = []
    if int(block["number"], 16) != n:
        problems.append(f"file holds block {int(block['number'], 16)}")

    # The chain-of-parent-hashes check. Only meaningful between two blocks we actually have, so a
    # gap resets it rather than reporting a spurious break on the far side.
    if prev_hash is not None and block["parentHash"] != prev_hash:
        problems.append(f"parent {block['parentHash']} != previous block's hash {prev_hash}")

    # The independent check: every execution-payload field Xatu carries, taken from the finalized
    # beacon chain rather than from the RPC we are checking.
    entry = manifest.get(n)
    if entry:
        for ours, theirs in (
            ("hash", "hash"),
            ("parentHash", "parent_hash"),
            ("stateRoot", "state_root"),
            ("miner", "fee_recipient"),
        ):
            if block[ours].lower() != entry[theirs].lower():
                problems.append(f"{ours} {block[ours]} != Xatu {entry[theirs]}")

        expected = {
            "gasUsed": int(entry["gas_used"]),
            "gasLimit": int(entry["gas_limit"]),
            "blobGasUsed": int(entry["blob_gas_used"]),
            "excessBlobGas": int(entry["excess_blob_gas"]),
            # Xatu stores this as a little-endian UInt128 blob, hex-encoded by the query above.
            "baseFeePerGas": int.from_bytes(bytes.fromhex(entry["base_fee_per_gas_le"]), "little"),
        }
        for ours, want in expected.items():
            # Absent on both sides for pre-Cancun blocks; absent on one side is itself a problem.
            if ours not in block:
                if want:
                    problems.append(f"no {ours} in the block, Xatu says {want}")
                continue
            if int(block[ours], 16) != want:
                problems.append(f"{ours} {int(block[ours], 16)} != Xatu {want}")

        # Catches a truncated transaction list at fetch time rather than leaving it to the block
        # hash check, which would only say "this is not the block it claims to be".
        txs = len(block.get("transactions", []))
        if txs != int(entry["transactions_count"]):
            problems.append(f"{txs} transactions != Xatu {entry['transactions_count']}")

    if problems:
        bad.append((n, "; ".join(problems)))
        os.remove(path)
        prev_hash = None
    else:
        prev_hash = block["hash"]

want = last - first + 1
print(f"{want - len(missing) - len(bad)} / {want} blocks verified in {outdir}")
if manifest:
    print(f"  checked against Xatu for {len(manifest)} of them")
else:
    print("  no Xatu manifest — canonicity was NOT checked")

for label, rows in (("missing", missing), ("rejected (deleted, re-run to refetch)", bad)):
    if rows:
        print(f"  {label}: {len(rows)}")
        for n, why in rows[:10]:
            print(f"    {n}: {why}")
        if len(rows) > 10:
            print(f"    ... and {len(rows) - 10} more")

sys.exit(1 if missing or bad else 0)
PY
verify_ok=$?

((manifest_ok == 0 && verify_ok == 0))
