use alloc::vec::Vec;
use alloy_primitives::{keccak256, Bytes, B256};
use reth_trie::{ExecutionWitnessMode, HashedPostState, HashedStorage};
use revm::database::State;

/// Wall time and page-fault deltas for the steps inside witness generation.
///
/// Major page faults are the useful signal here: reth's database is memory-mapped, so a read that
/// misses the page cache surfaces as a major fault rather than as a syscall. Counting them per step
/// separates the work that touches disk from the work that is pure CPU, which wall time alone
/// cannot do — a cold trie walk and a warm one differ by two orders of magnitude with identical
/// instruction counts.
///
/// Collection is off unless `RETH_WITNESS_TIMING` is set, because reading `/proc` costs a few
/// microseconds per sample and the cheapest steps here run in tens of microseconds.
#[cfg(all(feature = "witness", feature = "std"))]
#[derive(Debug, Default, Clone, Copy)]
struct StepMeter {
    elapsed: core::time::Duration,
    major_faults: u64,
    minor_faults: u64,
}

#[cfg(all(feature = "witness", feature = "std"))]
struct StepGuard {
    start: std::time::Instant,
    faults: (u64, u64),
}

#[cfg(all(feature = "witness", feature = "std"))]
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
#[cfg(all(feature = "witness", feature = "std"))]
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

#[cfg(all(feature = "witness", feature = "std"))]
fn timing_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RETH_WITNESS_TIMING").is_some())
}

/// Borrows finalized execution state for witness generation.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionWitnessRecord<'a, DB> {
    /// State after execution.
    state: &'a State<DB>,
}

impl<'a, DB> ExecutionWitnessRecord<'a, DB> {
    /// Creates a new record from the state after execution.
    pub const fn new(state: &'a State<DB>) -> Self {
        Self { state }
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
        SP: reth_storage_api::HashedPostStateProvider
            + reth_storage_api::StateProofProvider
            + ?Sized,
        HP: reth_storage_api::HeaderProvider + ?Sized,
        HP::Header: alloy_rlp::Encodable,
    {
        let step = StepGuard::start();
        let codes = match mode {
            ExecutionWitnessMode::Legacy => self
                .state
                .cache
                .contracts
                .values()
                .map(|code| code.original_bytes())
                .chain(
                    // cache state does not have all the contracts, especially when
                    // a contract is created within the block
                    // the contract only exists in bundle state, therefore we need
                    // to include them as well
                    self.state.bundle_state.contracts.values().map(|code| code.original_bytes()),
                )
                .collect(),
            ExecutionWitnessMode::Canonical => {
                let mut codes: Vec<_> = self
                    .state
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

        let m_codes = step.stop();

        let mut m_hash = StepMeter::default();
        let mut m_expand = StepMeter::default();
        let (hashed_state, keys) =
            self.hashed_post_state_timed(state_provider, &mut m_hash, &mut m_expand)?;

        let step = StepGuard::start();
        let state = state_provider.witness(Default::default(), hashed_state, mode)?;
        let m_trie = step.stop();
        let mut exec_witness =
            alloy_rpc_types_debug::ExecutionWitness { state, codes, keys, ..Default::default() };

        let step = StepGuard::start();
        let lowest_block_number =
            self.state.block_hashes.lowest().map(|(block_number, _)| block_number);
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
        let m_headers = step.stop();

        // One event per witness, at debug level so a normal node never pays for it. `trie` is the
        // only step that walks the database; the rest are memory or CPU, so a large `*_major` on
        // any other step means something unexpected is faulting.
        tracing::debug!(
            target: "reth::witness::timing",
            block_number,
            nodes = exec_witness.state.len(),
            codes = exec_witness.codes.len(),
            keys = exec_witness.keys.len(),
            headers = exec_witness.headers.len(),
            us_codes = m_codes.elapsed.as_micros() as u64,
            us_hash = m_hash.elapsed.as_micros() as u64,
            us_expand = m_expand.elapsed.as_micros() as u64,
            us_trie = m_trie.elapsed.as_micros() as u64,
            us_headers = m_headers.elapsed.as_micros() as u64,
            major_codes = m_codes.major_faults,
            major_hash = m_hash.major_faults,
            major_expand = m_expand.major_faults,
            major_trie = m_trie.major_faults,
            major_headers = m_headers.major_faults,
            minor_trie = m_trie.minor_faults,
            "execution witness generated"
        );

        Ok(exec_witness)
    }

    /// Builds the witness target, reporting how long the local hashing and the provider's
    /// expansion took separately. The first is pure keccak over touched keys; the second can read
    /// the database, because destroyed accounts need their untouched slots expanded from the parent
    /// state.
    #[cfg(feature = "witness")]
    fn hashed_post_state_timed<SP>(
        &self,
        state_provider: &SP,
        hash_meter: &mut StepMeter,
        expand_meter: &mut StepMeter,
    ) -> reth_storage_errors::provider::ProviderResult<(HashedPostState, Vec<Bytes>)>
    where
        SP: reth_storage_api::HashedPostStateProvider + ?Sized,
    {
        let step = StepGuard::start();
        let mut hashed_state = HashedPostState::default();
        let mut keys = Vec::new();
        for (address, account) in &self.state.cache.accounts {
            let hashed_address = keccak256(address);
            hashed_state
                .accounts
                .insert(hashed_address, account.account.as_ref().map(|a| (&a.info).into()));

            let storage = hashed_state
                .storages
                .entry(hashed_address)
                .or_insert_with(|| HashedStorage::new(false));

            if let Some(account) = &account.account {
                keys.push(address.to_vec().into());

                for (slot, value) in &account.storage {
                    let slot = B256::from(*slot);
                    let hashed_slot = keccak256(slot);
                    storage.storage.insert(hashed_slot, *value);

                    keys.push(slot.into());
                }
            }
        }

        // The execution cache does not contain untouched slots of a destroyed account. The
        // provider expands them into explicit zero writes from the parent state; extending it last
        // also ensures the bundle's final values override those collected from the cache.
        *hash_meter = step.stop();

        let step = StepGuard::start();
        hashed_state.extend(state_provider.hashed_post_state(&self.state.bundle_state)?);
        *expand_meter = step.stop();

        Ok((hashed_state, keys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use reth_storage_api::HashedPostStateProvider;
    use reth_storage_errors::provider::ProviderResult;
    use revm::{
        database::{states::CacheAccount, AccountStatus, BundleAccount, EmptyDB},
        state::AccountInfo,
    };

    #[derive(Debug)]
    struct ExpandedStateProvider(HashedPostState);

    impl HashedPostStateProvider for ExpandedStateProvider {
        fn hashed_post_state(
            &self,
            bundle_state: &revm::database::BundleState,
        ) -> ProviderResult<HashedPostState> {
            assert!(bundle_state.state.values().any(BundleAccount::was_destroyed));
            Ok(self.0.clone())
        }
    }

    #[test]
    fn destroyed_account_storage_is_zero_expanded_without_wipe() {
        let address = Address::with_last_byte(1);
        let hashed_address = keccak256(address);
        let hashed_slot = B256::with_last_byte(2);

        let mut state = State::builder().with_database(EmptyDB::default()).build();
        state.cache.accounts.insert(address, CacheAccount::new_destroyed());
        state.bundle_state.state.insert(
            address,
            BundleAccount::new(
                Some(AccountInfo::default()),
                None,
                Default::default(),
                AccountStatus::Destroyed,
            ),
        );

        let provider = ExpandedStateProvider(
            HashedPostState::default().with_accounts([(hashed_address, None)]).with_storages([(
                hashed_address,
                HashedStorage::from_iter([(hashed_slot, U256::ZERO)]),
            )]),
        );

        let (hashed_state, _) = ExecutionWitnessRecord::new(&state)
            .hashed_post_state_timed(&provider, &mut Default::default(), &mut Default::default())
            .unwrap();
        let storage = hashed_state.storages.get(&hashed_address).unwrap();
        assert!(!storage.wiped);
        assert_eq!(storage.storage.get(&hashed_slot), Some(&U256::ZERO));
    }
}
