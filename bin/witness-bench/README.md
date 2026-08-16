# witness-bench

Measures the **marginal cost a block builder pays to attach an execution witness** to a block it is
about to submit.

The question this answers: if builders were required to ship a stateless-execution witness alongside
each block, how much latency does that add on top of what they already spend building? The
interesting comparison is not witness-vs-nothing but **witness vs. the state root the builder
computes for the header regardless** — that is work it cannot avoid, and it walks much of the same
trie.

Status as of 2026-08-14: the harness works end to end, but the datadir it was run against is
unusable past block 24,785,397. See [Where this stopped](#where-this-stopped).

## Design

Blocks are replayed in **lockstep**: at loop entry the database tip is exactly the parent of the
block being replayed, so every state read hits `LatestStateProviderRef` and no `revert_state()`
overlay is ever materialized. That is the provider a builder sees when it builds on the current
head, and it is the only regime in which these timings mean anything.

The alternative — sync forward, then generate witnesses historically — is not viable. Witness
generation on a `HistoricalStateProvider` materializes every account and storage revert between the
target and the tip; at an average distance of ~5,000 blocks over 10k iterations that is unbounded,
and reth itself warns about OOM past `EPOCH_SLOTS`.

Each block is decomposed into independently timed phases:

| phase | who pays it | notes |
|---|---|---|
| `fetch` | harness | reading the block from `--blocks-dir` or static files, including sender recovery |
| `exec` | builder anyway | EVM execution, minus the witness closure |
| `witness` | **marginal** | `ExecutionWitnessRecord::into_execution_witness` |
| `encode` | **marginal** | JSON-serializing the witness (`--encode-witness`) |
| `state_root` | builder anyway | needed for the header regardless |
| `save` | harness | zstd + write for `--witness-dir` |
| `persist` | harness | advancing the tip; a builder never does this |

The number to report is `witness + encode`, and the ratio `witness / state_root`.

`witness` is one phase rather than the record / trie-walk / ancestor-header split an earlier version
timed. In v2.5 `ExecutionWitnessRecord` borrows the finished `State` and does all three inside a
single call, and that call is exactly what `debug_executionWitness` invokes — so measuring it whole
tracks the shipped path instead of a hand-assembled approximation.

## Usage

```bash
# Is this datadir usable at all? Read-only.
witness-bench --datadir <dir> --check-tip-root

# Measure one block without touching the database.
witness-bench --datadir <dir> --blocks-dir <blocks> --from N --repeat 10 --encode-witness \
              --witness-dir <out>/witnesses --out <out>/N.jsonl

# Replay a range, advancing the tip. IRREVERSIBLE.
witness-bench --datadir <dir> --blocks-dir <blocks> --from N --to M --commit --out <out>/run.jsonl
```

Key flags:

- `--repeat N` — re-runs **only the witness pass** against the same executed state. The block is
  executed once and persisted once, so this composes with `--commit`. Iteration 1 is cold; later
  ones run with the trie nodes already in the page cache. A determinism check across repeats bails
  if the witness shape ever differs.
- `--commit` — persists and advances the tip. Required for ranges longer than one block, because
  block N+1 can only be measured once N's post-state is the latest state. Refuses to persist a block
  whose state root does not match its header, and rolls back its transaction on any bail.
- `--allow-inconsistent` — opens without the static-file/database consistency check
  (`AccessRights::RoInconsistent`). Diagnostics only; without it a bad datadir either errors
  read-only or **silently runs a healing unwind** read-write.
- `--witness-dir` — saves each witness to `<dir>/<number>.json.zst`, once per block regardless of
  `--repeat`, timed separately so compression never lands in the marginal-cost number.

Output is JSONL, **one line per witness run** (long format). `us_witness` is the only per-iteration
field; every other column is a property of the block, measured once and repeated on each row so
lines stand alone. Group by `number` and take the first when aggregating those columns.

### Preflight

Before replaying, the harness checks that `Execution`, `AccountHashing`, `StorageHashing`,
`MerkleExecute` and `Finish` all sit at the same height. A snapshot captured mid-pipeline leaves the
plain state ahead of the hashed state and the tries, which produces wrong state roots with no error.
It also requires `--from == tip + 1`.

**`--check-tip-root` is not sufficient to validate a datadir.** It reads only the root node (~1 ms)
and cannot see corruption below it. It passed on a datadir that later produced a wrong state root.

## Block data

`fetch-blocks.sh` pulls blocks from JSON-RPC `eth_getBlockByNumber(n, true)`, saved verbatim as
`<n>.json`, and cross-checks them against Xatu's `canonical_beacon_block` dataset.

Two sources are needed because neither suffices alone. A public RPC endpoint is a load balancer over
nodes that may disagree, and a block that decodes cleanly and hashes to the hash the RPC reported is
still worthless if it is a reorged sibling. Xatu supplies the canonical execution-payload identity
— hash, parent hash, state root, fee recipient, gas, base fee, blob fields, transaction count —
derived from the finalized beacon chain, plus the chain-of-parent-hashes check a lockstep replay
depends on. One off-chain block anywhere in the range kills the run when it is reached, hours in.

The loader independently verifies each block twice: the header hash proves the JSON round trip was
faithful, and `validate_body_against_header` ties the transaction list back to the header's
transactions root. Neither check covers the other.

## Results

Block 24,785,397 — 261 txs, 21.65M gas. Witness: 9,433 nodes / 3.31 MB, 254 codes / 2.34 MB,
2,038 keys, 37 ancestor headers (`BLOCKHASH` reached 36 blocks back). JSON 11.51 MB, zstd 4.42 MB
(2.6×).

`--repeat 10`, all in ms:

| | cold (iter 1) | warm min | warm median | warm max |
|---|---|---|---|---|
| witness | 322.72 | 194.09 | 204.26 | 224.40 |

Once-only phases: exec 76.49, state_root 149.84, encode 26.03, save 131.71, persist 4,240.3.

**Ratios: 2.15× cold, 1.30× warm** against the state root. Encoding is 0.2% of the marginal cost —
it is all trie walk.

### Page cache dominates everything

The same block, same binary, three cache states:

| | exec | witness | state_root |
|---|---|---|---|
| first ever touch (cold disk) | 4,771 ms | 20,034 ms | 7,997 ms |
| process restart, warm page cache | 77 ms | 323 ms | 143 ms |
| same process, second pass | — | 194 ms | — |

A ~100× swing driven entirely by cache residency against a 3.2 TB MDBX file. **Any headline number
must state the cache state alongside it.** This is also an argument for running the experiment on a
full node rather than an archive: a builder's working set is a fraction of an archive's, so its
timings are far closer to the warm rows, and the archive's cold numbers say more about the disk than
about Ethereum.

There is a second, subtler bias: the witness pass runs before the state root and warms the trie for
it, so `witness_over_state_root` flatters the state root. `witness_warm_over_state_root` is the
fairer cache-equalized comparison.

## Where this stopped

The datadir at `/home/ubuntu/data` is an early-April 2026 snapshot (tip was 24,785,396) that was
captured **mid-pipeline** — plain state at ...404, hashing and merkle at ...396 — and whose
`MerkleUnwind` back to 396 failed partway.

- Block 24,785,397 replayed and committed cleanly. Tip is now **24,785,397**, and its root matches
  its header.
- Block 24,785,398 **cannot** be replayed. Its execution is correct —
  `validate_block_post_execution` passes, covering gas used, receipts root, logs bloom and requests
  hash, which requires reading the right plain state for all 326 transactions — but the state root
  computed from the trie tables diverges:

  ```
  computed  0x50b4c0cd5d753635f5deb529aca6542a81a415ad48219773894f1afe303177ba
  expected  0xcf8d3047a6967d4f3a56c06afc267df0efceaac796bdf5f718bfc2c0a579d38e
  ```

The split is clean: **plain state and execution are sound; the trie is not.** The most likely cause
is subtries left as a mix of 396- and 404-era nodes by the failed unwind. Block 397 happened to
touch only sound ones. Because witnesses are read from the same trie tables, **witnesses from this
datadir are not trustworthy**, including the saved one for 397. The timings remain meaningful — the
walk does the same work whether or not the nodes are right.

Side effect: `update_pipeline_stages` moves *every* stage checkpoint, including Headers and Bodies
which the snapshot had at ...404, so static files now look ahead of the database and plain read-only
opens fail the consistency check. Use `--allow-inconsistent`.

## Next steps

1. **Replace the datadir.** `reth download --minimal` at a recent snapshot (a new one is published
   daily): **164.8 GiB download → 262.7 GiB extracted**. The `--full` selection differs *only* in
   transaction history (3.7 GiB vs 621.7 GiB) and needs **880.8 GiB extracted**, which does not fit
   in the ~720 GiB free. Since blocks come from `--blocks-dir`, minimal costs nothing relevant. A
   snapshot synced normally has its stages aligned by construction, which is the fault that bit this
   one.

   Note two things about `reth download --list`. Every row's profile reads `archive` because that is
   the only thing published — one daily snapshot holding all components, unpruned — and its `SIZE`
   is the all-components total. The snapshots are *modular*, so `--minimal` / `--full` / `--archive`
   are client-side selections over the same manifest; pointing `--minimal` at an `archive` row is
   the normal usage. Also, `--list` prints **GiB while labelling it GB**, and `--print-plan-json`
   reports raw bytes; `df -h` is GiB, so compare in GiB.
2. **Re-fetch block data.** A tip of 25,747,156 makes the existing `/home/ubuntu/blocks` range
   (24,785,395–24,787,000, 1,607 blocks) useless; refetch from tip + 1.
3. **Verify storage v2 before trusting a long run.** Every result here came from a v1 datadir; a
   2.5.0 snapshot will be v2, and the persist path (`write_hashed_state` → `write_trie_updates` →
   `update_pipeline_stages`) has never run against it. Replay a few blocks read-only, then commit
   15–20, before starting a full sweep.
4. **Build witness verification — not yet implemented.** Nothing currently proves a witness is
   *complete*; a witness missing nodes looks fine until replayed statelessly. Note that
   `reth-stateless` **no longer exists in v2.5**, so `stateless_validation` is not available. The
   available primitive is `DecodedMultiProofV2::from_witness` in `reth-trie-common`
   (`crates/trie/common/src/proofs.rs:475`). A cheaper decisive check for corruption: key witness
   nodes by `keccak256`, then verify every node except the root is referenced by hash from some
   other node — an orphan node is a smoking gun.

## Environment

Requires `m4` (GMP, via `revm-precompile`) and, unless built `--no-default-features` without `jit`,
`llvm-22-dev` + `libpolly-22-dev` (revmc → inkwell → `llvm-sys 221`, i.e. LLVM 22.1). Ubuntu 26.04
ships llvm-22 in *universe*.

## Artifacts from the 2026-08-14 run

```
/home/ubuntu/witness-bench-out/
├── 24785397.jsonl              10 rows, read-only, --repeat 10
├── 24785397-24785398.jsonl     10 rows, committing run (397 only; 398 bailed)
├── 24785398-diag.jsonl          1 row, the state-root divergence
└── witnesses/24785397.json.zst  4.4 MB — see the trust caveat above
```

`bin/witness-bench/` and `debug-witness-experiment.md` are **uncommitted** on the `usenix` branch,
along with the `Cargo.toml` / `Cargo.lock` workspace-member entry. `cargo +nightly fmt --all --check`
passes; clippy has not completed a clean run.

`debug-witness-experiment.md` in the repo root holds the original design rationale — provider
semantics, the three tips, Route A (Engine API) vs Route B (this harness). Its file:line references
point into the v1.6 tree and are stale, but the reasoning is intact.
