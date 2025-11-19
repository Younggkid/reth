#!/usr/bin/env python3
import gzip, json, requests, sys, time, csv, os, re
from pathlib import Path
from eth_account.typed_transactions import TypedTransaction
from eth_account._utils.legacy_transactions import Transaction, vrs_from
from eth_account._utils.signing import hash_of_signed_transaction
from eth_account.account import Account
from hexbytes import HexBytes
from requests.adapters import HTTPAdapter
from urllib3.util.retry import Retry

URL = "http://localhost:12345/eth_simulateV1"

# --- Tunables ---
PRINT_EVERY = 25          # reduce console spam
FLUSH_EVERY = 25          # csv flush frequency
RECOVER_SENDER = True    # set False if server can infer `from`
REQUEST_TIMEOUT = (3, 60) # (connect, read) seconds
# -----------------

def decode_raw_tx(raw_tx: str) -> dict:
    txn_bytes = HexBytes(raw_tx)
    # EIP-2718: typed tx if first byte <= 0x7F
    if len(txn_bytes) > 0 and txn_bytes[0] <= 0x7F:
        tx = TypedTransaction.from_bytes(txn_bytes)
        msg_hash = tx.hash() if RECOVER_SENDER else None
        vrs = tx.vrs() if RECOVER_SENDER else None
        tx_dict = tx.as_dict()
    else:
        tx = Transaction.from_bytes(txn_bytes)
        msg_hash = hash_of_signed_transaction(tx) if RECOVER_SENDER else None
        vrs = vrs_from(tx) if RECOVER_SENDER else None
        tx_dict = tx.as_dict()

    sender = None
    if RECOVER_SENDER:
        sender = Account._recover_hash(msg_hash, vrs=vrs)
        if not sender.startswith("0x"):
            sender = "0x" + sender

    to_addr = "0x" + tx_dict["to"].hex() if tx_dict["to"] else None
    out = {}
    if sender is not None:
        out["from"] = sender
    out["to"] = to_addr
    return out

def make_session() -> requests.Session:
    s = requests.Session()
    # Persistent HTTP/1.1 connections + basic retry on transient errors
    retry = Retry(total=3, backoff_factor=0.2, status_forcelist=(502, 503, 504))
    adapter = HTTPAdapter(max_retries=retry, pool_connections=10, pool_maxsize=10)
    s.mount("http://", adapter)
    s.mount("https://", adapter)
    return s

def send_payloads(ndjson_gz_path: Path, output_csv: Path):
    output_csv.parent.mkdir(parents=True, exist_ok=True)
    fieldnames = ["slot_number", "gas_used", "num_of_txns", "time_ms"]

    session = make_session()

    with open(output_csv, "w", newline="") as csvfile:
        writer = csv.DictWriter(csvfile, fieldnames=fieldnames)
        writer.writeheader()

        last_flush = 0
        last_print = 0

        with gzip.open(ndjson_gz_path, "rt") as f:
            for line_num, line in enumerate(f, start=1):
                try:
                    record = json.loads(line)

                    slot_str = record.get("payload", {}).get("message", {}).get("slot")
                    if slot_str is None:
                        if line_num - last_print >= PRINT_EVERY:
                            print(f"[Line {line_num}] No slot; skip.")
                            last_print = line_num
                        continue

                    execution_payload = record.get("payload", {}).get("execution_payload", {})
                    if not execution_payload:
                        if line_num - last_print >= PRINT_EVERY:
                            print(f"[Line {line_num}] No execution_payload; skip.")
                            last_print = line_num
                        continue

                    raw_txs = execution_payload.get("transactions", []) or []
                    ep_gas_used = execution_payload.get("gas_used") or execution_payload.get("gasUsed")
                    block_num = execution_payload.get("block_number") or execution_payload.get("blockNumber")
                    if isinstance(block_num, str):
                        block_num = int(block_num, 16) if block_num.startswith("0x") else int(block_num)
                    if block_num is None:
                        if line_num - last_print >= PRINT_EVERY:
                            print(f"[Line {line_num}] No block_number; skip.")
                            last_print = line_num
                        continue
                    block_num_hex = hex(block_num)

                    if not raw_txs:
                        if line_num - last_print >= PRINT_EVERY:
                            print(f"[Line {line_num}] Skipping: no txs.")
                            last_print = line_num
                        continue

                    # Build calls: this is the CPU-heavy section if RECOVER_SENDER=True
                    calls = [decode_raw_tx(tx) for tx in raw_txs if tx]

                    simulate_payload = {
                        "blockStateCalls": [
                            {"blockOverrides": {}, "stateOverrides": {}, "calls": calls}
                        ],
                        "traceTransfers": False,
                        "validation": False,
                        "returnFullTransactions": False
                    }

                    rpc_request = {
                        "jsonrpc": "2.0",
                        "method": "eth_simulateV1",
                        "params": [simulate_payload, block_num_hex],
                        "id": f"{line_num}"
                    }

                    t0 = time.perf_counter()
                    resp = session.post(URL, json=rpc_request, timeout=REQUEST_TIMEOUT)
                    # Optional: check errors
                    # if resp.status_code != 200: print("HTTP", resp.status_code, resp.text)
                    elapsed_ms = (time.perf_counter() - t0) * 1000

                    row = {
                        "slot_number": slot_str,
                        "gas_used": ep_gas_used if ep_gas_used is not None else "",
                        "num_of_txns": len(calls),
                        "time_ms": round(elapsed_ms, 3),
                    }
                    writer.writerow(row)

                    # Periodic flush (no fsync)
                    if line_num - last_flush >= FLUSH_EVERY:
                        csvfile.flush()
                        last_flush = line_num

                    # Periodic print
                    if line_num - last_print >= PRINT_EVERY:
                        print(f"[Line {line_num}] OK: {len(calls)} txs, gas_used={ep_gas_used}, time={elapsed_ms:.2f}ms")
                        last_print = line_num

                except Exception as e:
                    print(f"[Line {line_num}] Error: {e}")

    print(f"✅ Wrote rows to {output_csv}")

def usage() -> None:
    print("Usage: python send_payloads_xx.py XX")
    print("  XX must be an integer in [01..50] (zero-padded).")
    print("  Input : eval/files/builder_XX.ndjson.gz")
    print("  Output: eval/output/output_XX.csv")


def main():
    if len(sys.argv) != 2:
        usage()
        sys.exit(1)

    arg = sys.argv[1].strip()

    # Accept "01".."50" or "1".."50"; normalize to zero-padded XX
    if not re.fullmatch(r"\d{1,2}", arg):
        print(f"Error: invalid number '{arg}'. Expect 01..50.")
        usage()
        sys.exit(1)

    n = int(arg)
    if not (1 <= n <= 50):
        print(f"Error: number out of range: {n}. Expect 01..50.")
        usage()
        sys.exit(1)

    XX = f"{n:02d}"

    in_path = Path(f"eval/new/builder_{XX}.ndjson.gz")
    out_path = Path(f"eval/new/output_{XX}.csv")

    if not in_path.is_file():
        print(f"Error: input file not found: {in_path}")
        sys.exit(1)

    print(f"[info] Input : {in_path}")
    print(f"[info] Output: {out_path}")
    send_payloads(in_path, out_path)


if __name__ == "__main__":
    main()