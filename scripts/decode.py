#!/usr/bin/env python3
import gzip, json, time
from pathlib import Path
from hexbytes import HexBytes
from eth_account.typed_transactions import TypedTransaction
from eth_account._utils.legacy_transactions import Transaction, vrs_from
from eth_account._utils.signing import hash_of_signed_transaction
from eth_account.account import Account
import json
from pathlib import Path
# --- toggle: include sender recovery or not ---
RECOVER_SENDER = True

def decode_raw_tx(raw_tx: str) -> dict:
    """Decode a raw EIP-2718 (or legacy) signed transaction hex string.
       Returns a minimal dict with 'from' (optional) and 'to'."""
    txn_bytes = HexBytes(raw_tx)

    # EIP-2718 typed tx if first byte <= 0x7F; else legacy
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

    to_addr = "0x" + tx_dict["to"].hex() if tx_dict.get("to") else None
    out = {}
    if sender is not None:
        out["from"] = sender
    out["to"] = to_addr
    return out

def main():
    in_path = Path("eval/new/builder_01.ndjson.gz")
    if not in_path.is_file():
        raise FileNotFoundError(f"Input not found: {in_path}")

    # 1) read only the first row
    with gzip.open(in_path, "rt") as f:
        first_line = f.readline()
        if not first_line:
            raise RuntimeError("File is empty (no first line).")

    rec = json.loads(first_line)

    # 2) extract payload.execution_payload.transactions
    payload = rec.get("payload") or {}
    exec_payload = payload.get("execution_payload") or {}
    txs = exec_payload.get("transactions") or []

    # 3) print the transactions content verbatim
    print("=== transactions (raw) ===")
    print(txs)  # this is usually a list of hex strings
    print(f"count={len(txs)}")

    if not txs:
        print("No transactions to decode; exiting.")
        return

    # 4) decode all txs and measure total time
    t0 = time.perf_counter()
    decoded = [decode_raw_tx(tx) for tx in txs if tx]
    elapsed_ms = (time.perf_counter() - t0) * 1000.0

    print("=== decoded sample (first 3) ===")
    for item in decoded[:3]:
        print(item)


    # assuming parsed_payload is already built
    transactions = rec.get("payload", {}).get("execution_payload", {}).get("transactions", [])

    # Write to a txt file as a Rust-style vector of strings
    out_path = Path("transactions.txt")
    with open(out_path, "w") as f:
        f.write("[\n")
        for tx in transactions:
            # each tx is already a 0x-prefixed hex string
            f.write(f'    "{tx}",\n')
        f.write("]\n")

    print(f"Wrote {len(transactions)} transactions to {out_path}")
    print(f"decoded_count={len(decoded)} total_time_ms={elapsed_ms:.3f} avg_time_us={elapsed_ms*1000/len(decoded):.2f}")

if __name__ == "__main__":
    main()