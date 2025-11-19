#!/usr/bin/env python3
import argparse
import csv
import gzip
import json
import sys
import time
from pathlib import Path
from typing import Any, Dict, Iterator, List, Optional, Tuple

import requests


def parse_hex_quantity(q: Any) -> Optional[int]:
    if q is None:
        return None
    if isinstance(q, int):
        return q
    if isinstance(q, str):
        s = q.strip().lower()
        if s.startswith("0x"):
            return int(s, 16)
        return int(s, 10)
    raise ValueError(f"Unsupported quantity type: {type(q)}")


def as_hex_quantity(v: Optional[int]) -> Optional[str]:
    if v is None:
        return None
    if not isinstance(v, int):
        raise ValueError(f"Expected int for hex quantity, got {type(v)}")
    return hex(v)


def iter_jsonl_gz(path: Path) -> Iterator[Dict[str, Any]]:
    with gzip.open(path, "rt", encoding="utf-8") as f:
        for lineno, line in enumerate(f, 1):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError as e:
                raise SystemExit(f"[{path}:{lineno}] invalid JSON: {e}") from e


def build_simulate_block_params(obj: Dict[str, Any]) -> Dict[str, Any]:
    """Map one NDJSON record to EthSimulateBlock request params with correct types."""
    payload = obj.get("payload", {}) or {}
    message = payload.get("message", {}) or {}
    exec_payload = payload.get("execution_payload", {}) or {}
    adj = payload.get("adjustment_data", {}) or {}

    # txs: list[str] hex-encoded signed transactions
    txs = exec_payload.get("transactions", []) or []

    # blockNumber: u64 (int)
    block_number_i = parse_hex_quantity(exec_payload.get("block_number"))
    if block_number_i is None:
        raise SystemExit("missing payload.execution_payload.block_number")

    # stateBlockNumber: BlockNumberOrTag -> 0x-hex of parent
    parent_block_i = max(block_number_i - 1, 0)
    state_block_number_val = hex(parent_block_i)

    # coinbase: hex address string
    coinbase = exec_payload.get("fee_recipient")
    if not isinstance(coinbase, str):
        raise SystemExit("missing payload.execution_payload.fee_recipient")

    proposer_address = message.get("proposer_fee_recipient")
    if not isinstance(proposer_address, str):
        raise SystemExit("missing payload.message.proposer_fee_recipient")

    # baseFee: Option<u128>
    base_fee_i = parse_hex_quantity(exec_payload.get("base_fee_per_gas"))

    # builderAddresses: Vec<Address>
    builder_addr = adj.get("builder_address")
    builder_addresses: List[str] = [builder_addr] if isinstance(builder_addr, str) else []

    # timestamp: u64 (int) — optional
    timestamp_i = parse_hex_quantity(exec_payload.get("timestamp"))

    params: Dict[str, Any] = {
        "txs": txs,
        "blockNumber": block_number_i,
        "stateBlockNumber": state_block_number_val,
        "coinbase": coinbase,
        "proposerAddress": proposer_address,
        "builderAddresses": builder_addresses,
    }
    if base_fee_i is not None:
        params["baseFee"] = base_fee_i
    if timestamp_i is not None:
        params["timestamp"] = timestamp_i

    return params


def rpc_call(url: str, method: str, params: List[Any], timeout_s: float, rid: int) -> Dict[str, Any]:
    payload = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
    headers = {"Content-Type": "application/json"}
    t0 = time.monotonic_ns()
    resp = requests.post(url, headers=headers, data=json.dumps(payload), timeout=timeout_s)
    t1 = time.monotonic_ns()
    latency_ms = (t1 - t0) / 1e6
    try:
        body = resp.json()
    except Exception:
        body = {"json_parse_error": resp.text}
    return {"latency_ms": latency_ms, "http_status": resp.status_code, "body": body, "id": rid}


def first_present(d: Dict[str, Any], *keys: str) -> Any:
    """Return first present key from d (not None), else None."""
    for k in keys:
        if isinstance(d, dict) and k in d and d[k] is not None:
            return d[k]
    return None


def extract_coinbase_balances(body: Dict[str, Any]) -> Tuple[Optional[int], Optional[int]]:
    """
    Reads coinbase_before/after from JSON-RPC result.
    Supports both snake_case and camelCase.
    Values may be hex-quantity strings or integers.
    """
    result = body.get("result", {})
    cb_before_raw = first_present(result, "coinbase_before", "coinbaseBefore")
    cb_after_raw  = first_present(result, "coinbase_after", "coinbaseAfter")
    return parse_hex_quantity(cb_before_raw), parse_hex_quantity(cb_after_raw)

def extract_option(body: Dict[str, Any]) -> Optional[int]:
    result = body.get("result", {})
    option = first_present(result, "option", "Option")
    return parse_hex_quantity(option)


def extract_actual_value(body: Dict[str, Any]) -> Optional[int]:
    """
    Extracts 'actual value' (in wei) from the RPC response body.
    Tries multiple aliases and parses hex or decimal quantities.
    """
    result = body.get("result", {})
    raw = first_present(
        result,
        "actual_value", "actualValue",
        "proposer_value", "proposerValue",
        "proposerValueWei",
        "value",  # last-resort fallback if your impl uses 'value'
    )
    return parse_hex_quantity(raw)


def parse_gwei_str_to_wei(s: Optional[str]) -> Optional[int]:
    """
    Parse a decimal string representing Gwei into an int Wei.
    Returns None if s is None or empty.
    """
    if s is None:
        return None
    ss = s.strip()
    if ss == "":
        return None
    gwei = int(ss, 10)
    return gwei


def parse_bool_like(v: Any) -> Optional[bool]:
    """
    Parse common boolean representations. Returns None if unknown.
    Accepts: True/False, "true"/"false", "yes"/"no", 1/0.
    """
    if v is None:
        return None
    if isinstance(v, bool):
        return v
    if isinstance(v, (int, float)):
        return bool(v)
    if isinstance(v, str):
        s = v.strip().lower()
        if s in ("true", "yes", "y", "1"):
            return True
        if s in ("false", "no", "n", "0"):
            return False
    return None


def main() -> None:
    ap = argparse.ArgumentParser(
        description="Read one builder_XX.ndjson.gz, simulate block, compare real proposer value (deltaWei) to claimed message.value (gwei)."
    )
    ap.add_argument("--endpoint", required=True, help="RPC endpoint, e.g. http://127.0.0.1:8545")
    ap.add_argument("--builder", required=True, help="Builder index (e.g. 01, 02, ..., 10)")
    ap.add_argument("--dir", default="eval/new", help="Directory containing builder_XX.ndjson.gz (default: eval/new)")
    ap.add_argument("--timeout", type=float, default=60.0, help="HTTP timeout per request (seconds).")
    ap.add_argument("--method", default="eth_simulateBlock", help="RPC method name (default: eth_simulateBlock).")
    ap.add_argument("--out-csv", default=None, help="(optional) Override output CSV path. Defaults to simulate_XX.csv based on --builder.")
    args = ap.parse_args()

    input_file = Path(args.dir) / f"builder_{args.builder}.ndjson.gz"
    if not input_file.exists():
        raise SystemExit(f"File not found: {input_file}")

    results: List[Dict[str, Any]] = []
    printed_first = False
    first_output_path = Path("first_output.txt")

    print(f"Reading {input_file} ...", flush=True)

    for rid, record in enumerate(iter_jsonl_gz(input_file), 1):
        # --- progress read ---
        payload = record.get("payload", {}) or {}
        exec_payload = payload.get("execution_payload", {}) or {}
        block_num = exec_payload.get("block_number", "unknown")

        # builder-claimed value (string in gwei) -> wei
        claimed_gwei_str = (payload.get("message", {}) or {}).get("value")
        claimed_wei = parse_gwei_str_to_wei(claimed_gwei_str)

        # safe_to_propose (top-level per your note). Fallback to payload if needed.
        safe_raw = record.get("safe_to_propose", None)
        if safe_raw is None:
            safe_raw = payload.get("safe_to_propose", None)
            #print(f"[{rid}] Warning: safe_to_propose missing at top-level, falling back to payload.")
        safe_bool = parse_bool_like(safe_raw)
        safe_str = "No" if safe_bool is False else "Yes"  # treat missing/false as NO per request

        # simulate
        bundle = build_simulate_block_params(record)
        res = rpc_call(args.endpoint, args.method, [bundle], args.timeout, rid)

        # extract coinbase before/after
        cb_before, cb_after = extract_coinbase_balances(res["body"])
        # extract actual proposer value from RPC response
        actual_value = extract_actual_value(res["body"])

        option = extract_option(res["body"])
        if cb_before is None or cb_after is None:
            delta_wei = None
            match = None
        else:
            delta_wei = cb_after - cb_before
            match = (claimed_wei is not None) and (delta_wei >= claimed_wei)

        # store per-call info
        res["block_num"] = block_num
        res["claimed_gwei_str"] = claimed_gwei_str
        res["claimed_wei"] = claimed_wei
        res["coinbase_before"] = cb_before
        res["coinbase_after"] = cb_after
        res["delta_wei"] = delta_wei
        res["meets_claim"] = match
        res["safe_to_propose"] = safe_str
        res["actual_value"] = actual_value
        res["option"] = option
        results.append(res)

        # write first response to file once
        if not printed_first:
            with first_output_path.open("w", encoding="utf-8") as outfp:
                outfp.write("=== First response (raw JSON) ===\n")
                outfp.write(json.dumps(res["body"], indent=2, sort_keys=True, ensure_ascii=False))
                outfp.write("\n=== End first response ===\n")
            print(f"First response written to {first_output_path.resolve()}")
            printed_first = True

        # print verdict per block (+ safe_to_propose)
        verdict = "UNKNOWN"
        if match is True:
            verdict = "YES"
        elif match is False:
            verdict = "NO"
        print(
            f"[{rid}] block={block_num}  MATCHES? {verdict}  safe_to_propose={safe_str} actual_value={actual_value} option={option}",
            flush=True,
        )

    # CSV output path: simulate_XX.csv by default unless overridden
    out_csv = Path(args.out_csv) if args.out_csv else Path(f"simulate_{args.builder}.csv")
    with out_csv.open("w", newline="", encoding="utf-8") as fp:
        writer = csv.writer(fp)
        writer.writerow([
            "id",
            "block_num",
            "http_status",
            "latency_ms",
            "deltaWei",
            "claimedGweiStr",
            "claimedWei",
            "meetsClaim",        # YES/NO/UNKNOWN
            "safe_to_propose",   # YES/NO
            "actualValueWei",    # NEW: from RPC response
            "option",            # NEW: from RPC response
        ])
        for r in results:
            meets_str = (
                "YES" if r.get("meets_claim") is True else
                "NO" if r.get("meets_claim") is False else
                "UNKNOWN"
            )
            writer.writerow([
                r["id"],
                r.get("block_num"),
                r["http_status"],
                f"{r['latency_ms']:.3f}",
                "" if r.get("delta_wei")       is None else str(r["delta_wei"]),
                r.get("claimed_gwei_str") if r.get("claimed_gwei_str") is not None else "",
                "" if r.get("claimed_wei")     is None else str(r["claimed_wei"]),
                meets_str,
                r.get("safe_to_propose", "NO"),
                "" if r.get("actual_value")    is None else str(r["actual_value"]),
                "" if r.get("option")          is None else str(r["option"]),
            ])


    print(f"\nWrote results to: {out_csv.resolve()}")
    print("\n=== Per-call latency (ms) ===")
    for r in results:
        print(f"id={r['id']:>6}  http={r['http_status']}  latency_ms={r['latency_ms']:.3f}")

    lats = [r["latency_ms"] for r in results]
    if lats:
        print("\n=== Summary ===")
        print(f"count={len(lats)}  min={min(lats):.3f} ms  avg={sum(lats)/len(lats):.3f} ms  max={max(lats):.3f} ms")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)