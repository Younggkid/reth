use alloc::vec::Vec;
use alloy_primitives::{keccak256, Bytes, B256};
use reth_trie::{ExecutionWitnessMode, HashedPostState, HashedStorage};
use revm::database::State;

/// Wall time and page-fault deltas for one step of witness generation.
///
/// Major page faults are the signal that matters: reth's database is memory-mapped, so a read that
/// misses the page cache surfaces as a fault rather than as a syscall. Counting them per step
/// separates work that touches disk from work that is pure CPU, which wall time alone cannot do —
/// a cold trie walk and a warm one differ by orders of magnitude with identical instruction counts.
///
/// Collection is off unless `RETH_WITNESS_TIMING` is set, because reading `/proc` costs a few
/// microseconds per sample and the cheapest steps here run in tens of microseconds.
#[cfg(feature = "std")]
#[derive(Debug, Default, Clone, Copy)]
pub struct StepMeter {
    /// Wall time for the step.
    pub elapsed: core::time::Duration,
    /// Page faults that required disk I/O.
    pub major_faults: u64,
    /// Page faults served without disk I/O.
    pub minor_faults: u64,
}

/// The per-step measurements collected across one witness generation.
#[cfg(feature = "std")]
#[derive(Debug, Default, Clone, Copy)]
pub struct WitnessTiming {
    /// Collecting bytecode preimages. Pure memory.
    pub codes: StepMeter,
    /// Hashing touched addresses and slots into the witness target. Pure CPU.
    pub hash: StepMeter,
    /// The trie walk. This is the step that reads the database.
    pub trie: StepMeter,
    /// Ancestor headers for `BLOCKHASH`, plus RLP encoding.
    pub headers: StepMeter,
}

#[cfg(feature = "std")]
struct StepGuard {
    start: std::time::Instant,
    faults: (u64, u64),
}

#[cfg(feature = "std")]
impl StepGuard {
    fn start() -> Self {
        Self { start: std::time::Instant::now(), faults: thread_faults() }
    }

    fn stop(self) -> StepMeter {
        let elapsed = self.start.elapsed();
        let (minor, major) = thread_faults();
        StepMeter {
            elapsed,
            major_faults: major.saturating_sub(self.faults.1),
            minor_faults: minor.saturating_sub(self.faults.0),
        }
    }
}

/// Returns `(minor, major)` fault counts for the calling thread, or zeros when disabled.
///
/// Reads `/proc/thread-self/stat`, whose `comm` field can itself contain spaces and parentheses;
/// splitting on the last `)` is the only reliable way to find the numeric tail. In that tail
/// `minflt` is index 7 and `majflt` index 9.
#[cfg(feature = "std")]
fn thread_faults() -> (u64, u64) {
    if !timing_enabled() {
        return (0, 0);
    }
    let Ok(stat) = std::fs::read_to_string("/proc/thread-self/stat") else { return (0, 0) };
    let Some((_, tail)) = stat.rsplit_once(')') else { return (0, 0) };
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let field = |index: usize| fields.get(index).and_then(|v| v.parse().ok()).unwrap_or(0);
    (field(7), field(9))
}

#[cfg(feature = "std")]
fn timing_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RETH_WITNESS_TIMING").is_some())
}

/// Tracks state changes during execution.
#[derive(Debug, Clone, Default)]
pub struct ExecutionWitnessRecord {
    /// Records all state changes
    pub hashed_state: HashedPostState,
    /// Map of all contract codes (created / accessed) to their preimages that were required during
    /// the execution of the block, including during state root recomputation.
    ///
    /// `keccak(bytecodes) => bytecodes`
    pub codes: Vec<Bytes>,
    /// Map of all hashed account and storage keys (addresses and slots) to their preimages
    /// (unhashed account addresses and storage slots, respectively) that were required during
    /// the execution of the block.
    ///
    /// `keccak(address|slot) => address|slot`
    pub keys: Vec<Bytes>,
    /// The lowest block number referenced by any BLOCKHASH opcode call during transaction
    /// execution.
    ///
    /// This helps determine which ancestor block headers must be included in the
    /// `ExecutionWitness`.
    ///
    /// `None` - when the BLOCKHASH opcode was not called during execution
    pub lowest_block_number: Option<u64>,
    /// Per-step timings and page-fault counts, populated as the record is built.
    #[cfg(feature = "std")]
    pub timing: WitnessTiming,
}

impl ExecutionWitnessRecord {
    /// Records the state after execution using the given witness generation mode.
    pub fn record_executed_state<DB>(&mut self, statedb: &State<DB>, mode: ExecutionWitnessMode) {
        #[cfg(feature = "std")]
        let step = StepGuard::start();
        self.codes = match mode {
            ExecutionWitnessMode::Legacy => statedb
                .cache
                .contracts
                .values()
                .map(|code| code.original_bytes())
                .chain(
                    // cache state does not have all the contracts, especially when
                    // a contract is created within the block
                    // the contract only exists in bundle state, therefore we need
                    // to include them as well
                    statedb.bundle_state.contracts.values().map(|code| code.original_bytes()),
                )
                .collect(),
            ExecutionWitnessMode::Canonical => {
                let mut codes: Vec<_> = statedb
                    .cache
                    .contracts
                    .values()
                    .map(|c| c.original_bytes())
                    .filter(|code| !code.is_empty())
                    .collect();
                codes.sort_unstable();
                codes
            }
        };
        #[cfg(feature = "std")]
        {
            self.timing.codes = step.stop();
        }

        #[cfg(feature = "std")]
        let step = StepGuard::start();
        for (address, account) in &statedb.cache.accounts {
            let hashed_address = keccak256(address);
            self.hashed_state
                .accounts
                .insert(hashed_address, account.account.as_ref().map(|a| (&a.info).into()));

            let storage = self
                .hashed_state
                .storages
                .entry(hashed_address)
                .or_insert_with(|| HashedStorage::new(account.status.was_destroyed()));

            if let Some(account) = &account.account {
                self.keys.push(address.to_vec().into());

                for (slot, value) in &account.storage {
                    let slot = B256::from(*slot);
                    let hashed_slot = keccak256(slot);
                    storage.storage.insert(hashed_slot, *value);

                    self.keys.push(slot.into());
                }
            }
        }
        self.lowest_block_number =
            statedb.block_hashes.lowest().map(|(block_number, _)| block_number);
        #[cfg(feature = "std")]
        {
            self.timing.hash = step.stop();
        }
    }

    /// Creates the record from the state after execution.
    pub fn from_executed_state<DB>(state: &State<DB>, mode: ExecutionWitnessMode) -> Self {
        let mut record = Self::default();
        record.record_executed_state(state, mode);
        record
    }

    /// Converts this record into a complete [`alloy_rpc_types_debug::ExecutionWitness`] by
    /// generating state proofs and fetching ancestor block headers.
    ///
    /// The `block_number` is the number of the block being witnessed. Ancestor headers are
    /// included based on the lowest block number referenced by BLOCKHASH opcodes during
    /// execution, or just the parent header if BLOCKHASH was not called.
    #[cfg(feature = "witness")]
    pub fn into_execution_witness<SP, HP>(
        self,
        state_provider: &SP,
        headers_provider: &HP,
        block_number: u64,
        mode: ExecutionWitnessMode,
    ) -> reth_storage_errors::provider::ProviderResult<alloy_rpc_types_debug::ExecutionWitness>
    where
        SP: reth_storage_api::StateProofProvider + ?Sized,
        HP: reth_storage_api::HeaderProvider + ?Sized,
        HP::Header: alloy_rlp::Encodable,
    {
        #[cfg(feature = "std")]
        let Self { hashed_state, codes, keys, lowest_block_number, mut timing } = self;
        #[cfg(not(feature = "std"))]
        let Self { hashed_state, codes, keys, lowest_block_number } = self;

        #[cfg(feature = "std")]
        let step = StepGuard::start();
        let state = state_provider.witness(Default::default(), hashed_state, mode)?;
        #[cfg(feature = "std")]
        {
            timing.trie = step.stop();
        }
        let mut exec_witness =
            alloy_rpc_types_debug::ExecutionWitness { state, codes, keys, ..Default::default() };

        #[cfg(feature = "std")]
        let step = StepGuard::start();
        let smallest = lowest_block_number.unwrap_or_else(|| block_number.saturating_sub(1));
        let range = smallest..block_number;

        exec_witness.headers = headers_provider
            .headers_range(range)?
            .into_iter()
            .map(|header| {
                let mut buf = Vec::new();
                alloy_rlp::Encodable::encode(&header, &mut buf);
                buf.into()
            })
            .collect();
        #[cfg(feature = "std")]
        {
            timing.headers = step.stop();

            // One event per witness, at debug level so a normal node never pays for it. `trie` is
            // the only step that walks the database; the rest are memory or CPU, so a large
            // `*_major` on any other step means something unexpected is faulting.
            tracing::debug!(
                target: "reth::witness::timing",
                block_number,
                nodes = exec_witness.state.len(),
                codes = exec_witness.codes.len(),
                keys = exec_witness.keys.len(),
                headers = exec_witness.headers.len(),
                us_codes = timing.codes.elapsed.as_micros() as u64,
                us_hash = timing.hash.elapsed.as_micros() as u64,
                us_trie = timing.trie.elapsed.as_micros() as u64,
                us_headers = timing.headers.elapsed.as_micros() as u64,
                major_codes = timing.codes.major_faults,
                major_hash = timing.hash.major_faults,
                major_trie = timing.trie.major_faults,
                major_headers = timing.headers.major_faults,
                minor_trie = timing.trie.minor_faults,
                "execution witness generated"
            );
        }

        Ok(exec_witness)
    }
}
