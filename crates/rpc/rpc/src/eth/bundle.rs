//! `Eth` bundle implementation and helpers.

use alloy_consensus::{EnvKzgSettings, Transaction as _};
use alloy_eips::eip7840::BlobParams;
use alloy_primitives::{uint, Keccak256, U256};
use alloy_rpc_types_mev::{EthCallBundle, EthCallBundleResponse, EthCallBundleTransactionResult};
use jsonrpsee::core::RpcResult;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::{ConfigureEvm, Evm};
use reth_primitives_traits::SignedTransaction;
use reth_revm::{database::StateProviderDatabase, db::CacheDB};
use reth_rpc_eth_api::{
    helpers::{Call, EthTransactions, LoadPendingBlock},
    EthCallBundleApiServer, FromEthApiError, FromEvmError,
    bundle::{EthSimulateBlock, EthSimulateBlockResponse},
};
use alloy_primitives::{Address,address};
use reth_provider::{
    HeaderProvider,
};
use reth_rpc_eth_types::{utils::recover_raw_transaction, EthApiError, RpcInvalidTransactionError};
use reth_tasks::pool::BlockingTaskGuard;
use reth_transaction_pool::{
    EthBlobTransactionSidecar, EthPoolTransaction, PoolPooledTx, PoolTransaction, TransactionPool,
};
use revm::{context_interface::result::ResultAndState, DatabaseCommit, DatabaseRef};
//use revm_primitives::EnvWithHandlerCfg;
use std::sync::Arc;
use reth_rpc_eth_types::EthResult;
use revm_primitives::hardfork::{SpecId};
use alloy_consensus::BlockHeader;
/// `Eth` bundle implementation.
pub struct EthBundle<Eth> {
    /// All nested fields bundled together.
    inner: Arc<EthBundleInner<Eth>>,
}

impl<Eth> EthBundle<Eth> {
    /// Create a new `EthBundle` instance.
    pub fn new(eth_api: Eth, blocking_task_guard: BlockingTaskGuard) -> Self {
        Self { inner: Arc::new(EthBundleInner { eth_api, blocking_task_guard }) }
    }

    /// Access the underlying `Eth` API.
    pub fn eth_api(&self) -> &Eth {
        &self.inner.eth_api
    }
}

impl<Eth> EthBundle<Eth>
where
    Eth: EthTransactions + LoadPendingBlock + Call + 'static,
{
    /// Simulates a bundle of transactions at the top of a given block number with the state of
    /// another (or the same) block. This can be used to simulate future blocks with the current
    /// state, or it can be used to simulate a past block. The sender is responsible for signing the
    /// transactions and using the correct nonce and ensuring validity
    pub async fn call_bundle(
        &self,
        bundle: EthCallBundle,
    ) -> Result<EthCallBundleResponse, Eth::Error> {
        let EthCallBundle {
            txs,
            block_number,
            coinbase,
            state_block_number,
            timeout: _,
            timestamp,
            gas_limit,
            difficulty,
            base_fee,
            ..
        } = bundle;
        if txs.is_empty() {
            return Err(EthApiError::InvalidParams(
                EthBundleError::EmptyBundleTransactions.to_string(),
            )
            .into())
        }
        if block_number == 0 {
            return Err(EthApiError::InvalidParams(
                EthBundleError::BundleMissingBlockNumber.to_string(),
            )
            .into())
        }

        let transactions = txs
            .into_iter()
            .map(|tx| recover_raw_transaction::<PoolPooledTx<Eth::Pool>>(&tx))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .collect::<Vec<_>>();

        let block_id: alloy_rpc_types_eth::BlockId = state_block_number.into();
        // Note: the block number is considered the `parent` block: <https://github.com/flashbots/mev-geth/blob/fddf97beec5877483f879a77b7dea2e58a58d653/internal/ethapi/api.go#L2104>
        let (mut evm_env, at) = self.eth_api().evm_env_at(block_id).await?;

        if let Some(coinbase) = coinbase {
            evm_env.block_env.beneficiary = coinbase;
        }

        // need to adjust the timestamp for the next block
        if let Some(timestamp) = timestamp {
            evm_env.block_env.timestamp = U256::from(timestamp);
        } else {
            evm_env.block_env.timestamp += uint!(12_U256);
        }

        if let Some(difficulty) = difficulty {
            evm_env.block_env.difficulty = U256::from(difficulty);
        }

        // Validate that the bundle does not contain more than MAX_BLOB_NUMBER_PER_BLOCK blob
        // transactions.
        let blob_gas_used = transactions.iter().filter_map(|tx| tx.blob_gas_used()).sum::<u64>();
        if blob_gas_used > 0 {
            let blob_params = self
                .eth_api()
                .provider()
                .chain_spec()
                .blob_params_at_timestamp(evm_env.block_env.timestamp.saturating_to())
                .unwrap_or_else(BlobParams::cancun);
            if transactions.iter().filter_map(|tx| tx.blob_gas_used()).sum::<u64>() >
                blob_params.max_blob_gas_per_block()
            {
                return Err(EthApiError::InvalidParams(
                    EthBundleError::Eip4844BlobGasExceeded(blob_params.max_blob_gas_per_block())
                        .to_string(),
                )
                .into())
            }
        }

        // default to call gas limit unless user requests a smaller limit
        evm_env.block_env.gas_limit = self.inner.eth_api.call_gas_limit();
        if let Some(gas_limit) = gas_limit {
            if gas_limit > evm_env.block_env.gas_limit {
                return Err(
                    EthApiError::InvalidTransaction(RpcInvalidTransactionError::GasTooHigh).into()
                )
            }
            evm_env.block_env.gas_limit = gas_limit;
        }

        if let Some(base_fee) = base_fee {
            evm_env.block_env.basefee = base_fee.try_into().unwrap_or(u64::MAX);
        }

        let state_block_number = evm_env.block_env.number;
        // use the block number of the request
        evm_env.block_env.number = U256::from(block_number);

        let eth_api = self.eth_api().clone();

        self.eth_api()
            .spawn_with_state_at_block(at, move |state| {
                let coinbase = evm_env.block_env.beneficiary;
                let basefee = evm_env.block_env.basefee;
                let db = CacheDB::new(StateProviderDatabase::new(state));

                let initial_coinbase = db
                    .basic_ref(coinbase)
                    .map_err(Eth::Error::from_eth_err)?
                    .map(|acc| acc.balance)
                    .unwrap_or_default();
                let mut coinbase_balance_before_tx = initial_coinbase;
                let mut coinbase_balance_after_tx = initial_coinbase;
                let mut total_gas_used = 0u64;
                let mut total_gas_fees = U256::ZERO;
                let mut hasher = Keccak256::new();

                let mut evm = eth_api.evm_config().evm_with_env(db, evm_env);

                let mut results = Vec::with_capacity(transactions.len());
                let mut transactions = transactions.into_iter().peekable();

                while let Some(tx) = transactions.next() {
                    let signer = tx.signer();
                    let tx = {
                        let mut tx = <Eth::Pool as TransactionPool>::Transaction::from_pooled(tx);

                        if let EthBlobTransactionSidecar::Present(sidecar) = tx.take_blob() {
                            tx.validate_blob(&sidecar, EnvKzgSettings::Default.get()).map_err(
                                |e| {
                                    Eth::Error::from_eth_err(EthApiError::InvalidParams(
                                        e.to_string(),
                                    ))
                                },
                            )?;
                        }

                        tx.into_consensus()
                    };

                    hasher.update(*tx.tx_hash());
                    let ResultAndState { result, state } = evm
                        .transact(eth_api.evm_config().tx_env(&tx))
                        .map_err(Eth::Error::from_evm_err)?;

                    let gas_price = tx
                        .effective_tip_per_gas(basefee)
                        .expect("fee is always valid; execution succeeded");
                    let gas_used = result.gas_used();
                    total_gas_used += gas_used;

                    let gas_fees = U256::from(gas_used) * U256::from(gas_price);
                    total_gas_fees += gas_fees;

                    // coinbase is always present in the result state
                    coinbase_balance_after_tx =
                        state.get(&coinbase).map(|acc| acc.info.balance).unwrap_or_default();
                    let coinbase_diff =
                        coinbase_balance_after_tx.saturating_sub(coinbase_balance_before_tx);
                    let eth_sent_to_coinbase = coinbase_diff.saturating_sub(gas_fees);

                    // update the coinbase balance
                    coinbase_balance_before_tx = coinbase_balance_after_tx;

                    // set the return data for the response
                    let (value, revert) = if result.is_success() {
                        let value = result.into_output().unwrap_or_default();
                        (Some(value), None)
                    } else {
                        let revert = result.into_output().unwrap_or_default();
                        (None, Some(revert))
                    };

                    let tx_res = EthCallBundleTransactionResult {
                        coinbase_diff,
                        eth_sent_to_coinbase,
                        from_address: signer,
                        gas_fees,
                        gas_price: U256::from(gas_price),
                        gas_used,
                        to_address: tx.to(),
                        tx_hash: *tx.tx_hash(),
                        value,
                        revert,
                    };
                    results.push(tx_res);

                    // need to apply the state changes of this call before executing the
                    // next call
                    if transactions.peek().is_some() {
                        // need to apply the state changes of this call before executing
                        // the next call
                        evm.db_mut().commit(state)
                    }
                }

                // populate the response

                let coinbase_diff = coinbase_balance_after_tx.saturating_sub(initial_coinbase);
                let eth_sent_to_coinbase = coinbase_diff.saturating_sub(total_gas_fees);
                let bundle_gas_price =
                    coinbase_diff.checked_div(U256::from(total_gas_used)).unwrap_or_default();
                let res = EthCallBundleResponse {
                    bundle_gas_price,
                    bundle_hash: hasher.finalize(),
                    coinbase_diff,
                    eth_sent_to_coinbase,
                    gas_fees: total_gas_fees,
                    results,
                    state_block_number: state_block_number.to(),
                    total_gas_used,
                };

                Ok(res)
            })
            .await
    }
    pub async fn simulate_block(&self, block: EthSimulateBlock) -> Result<EthSimulateBlockResponse, Eth::Error> {
        let EthSimulateBlock { txs, block_number, state_block_number, coinbase, proposer_address, base_fee, builder_addresses, timestamp } = block;
        if txs.is_empty() {
            return Err(EthApiError::InvalidParams(
                EthBundleError::EmptyBundleTransactions.to_string(),
            )
            .into())
        }
        if block_number == 0 {
            return Err(EthApiError::InvalidParams(
                EthBundleError::BundleMissingBlockNumber.to_string(),
            )
            .into())
        }
        let transactions = txs
            .into_iter()
            .map(|tx| recover_raw_transaction::<PoolPooledTx<Eth::Pool>>(&tx))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .collect::<Vec<_>>();
        let mut from_builder = address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045");
        let mut to_proposer = None;
        let mut actual_value = U256::ZERO;
        if let Some(last_tx) = transactions.last() {
            // Sender (recovered signer)
            from_builder = last_tx.signer();

            // Receiver (may be None for contract creation)
            to_proposer = last_tx.inner().to();

            // ETH value in wei
            actual_value = last_tx.inner().value();
        }
        let mut option1 = false;
        let mut option2 = false;
        let mut option = 0;
        if let Some(to_addr) = to_proposer {
            if to_addr == proposer_address {
                option2 = true;
                option += 2;
            }
            if coinbase == proposer_address {
                option1 = true;
                option += 1;
            }

        }
       // keep alloy’s BlockId
        let block_id: alloy_rpc_types_eth::BlockId = state_block_number.into();

        // 1) Grab EVM env (2-tuple)
        let (mut evm_env, at) = self.inner.eth_api.evm_env_at(block_id).await?;

        // ----- mutate block env via evm_env.block_env -----
        if let Some(timestamp) = timestamp {
            evm_env.block_env.timestamp = U256::from(timestamp);
        } else {
            evm_env.block_env.timestamp += U256::from(12);
        }

        if let Some(base_fee) = base_fee {
            evm_env.block_env.basefee = base_fee as u64;
        } else  {
            // 2) Use provider() instead of LoadPendingBlock::<trait>::provider(...)
            let provider = self.inner.eth_api.provider();
            let parent_block = evm_env.block_env.number.saturating_to::<u64>();

            let Some(parent) = provider.header_by_number(parent_block)? 
            else {
                return Err(EthApiError::InvalidParams(
                EthBundleError::BundleMissingBlockNumber.to_string(),
            ).into())
            };

            if let Some(next_basefee) = parent.next_block_base_fee(
                provider.chain_spec().base_fee_params_at_block(parent_block),
            ) {
                evm_env.block_env.basefee = next_basefee;
            }
        }

        // Save the state block number before we overwrite it
        let state_block_number = evm_env.block_env.number;

        // overwrite number/coinbase with the request’s
        evm_env.block_env.number = U256::from(block_number);
        evm_env.block_env.beneficiary = coinbase;

        let eth_api = self.inner.eth_api.clone();

        // 3) Build the EVM Env up-front; move it into the closure
        //let cfg_env   = evm_env.cfg_env;    // move out
        //let block_env = evm_env.block_env;  // move out
        //let env = EnvWithHandlerCfg::new_with_cfg_env(cfg_env, block_env, TxEnv::default());
        let eth_api = self.eth_api().clone();

        self.eth_api().spawn_with_state_at_block(at, move |state| {
            // NOTE: block_env was moved; access via the `env` we created:
            let coinbase = evm_env.block_env.beneficiary;
            let basefee = evm_env.block_env.basefee;

            let db = CacheDB::new(StateProviderDatabase::new(state));


            let initial_coinbase = db
                .basic_ref(coinbase)
                .map_err(Eth::Error::from_eth_err)?
                .map(|acc| acc.balance)
                .unwrap_or_default();

            let builder_balances_before_tx = builder_addresses
                .clone()
                .into_iter()
                .map(|addr| {
                    DatabaseRef::basic_ref(&db, addr)
                        .map(|acc| acc.map_or(U256::ZERO, |i| i.balance))
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>();

            let mut coinbase_balance_before_tx = initial_coinbase;
            let mut coinbase_balance_after_tx  = initial_coinbase;
            let mut total_gas_used  = 0u64;
            let mut total_gas_fees  = U256::ZERO;
            let mut hasher = Keccak256::new();

            // 4) Use the helper function instead of the Call trait
            let mut evm = eth_api.evm_config().evm_with_env(db, evm_env);

            let mut results      = Vec::with_capacity(transactions.len());
            let mut transactions = transactions.into_iter().peekable();

            while let Some(tx) = transactions.next() {
                let signer = tx.signer();
                let tx = {
                    let mut tx = <Eth::Pool as TransactionPool>::Transaction::from_pooled(tx);

                    if let EthBlobTransactionSidecar::Present(sidecar) = tx.take_blob() {
                        tx.validate_blob(&sidecar, EnvKzgSettings::Default.get()).map_err(
                            |e| {
                                Eth::Error::from_eth_err(EthApiError::InvalidParams(
                                    e.to_string(),
                                ))
                            },
                        )?;
                    }

                    tx.into_consensus()
                };

                hasher.update(*tx.tx_hash());
                let ResultAndState { result, state } = evm
                    .transact(eth_api.evm_config().tx_env(&tx))
                    .map_err(Eth::Error::from_evm_err)?;

                let gas_price = tx
                    .effective_tip_per_gas(basefee)
                    .expect("fee is always valid; execution succeeded");
                let gas_used = result.gas_used();
                total_gas_used += gas_used;

                let gas_fees = U256::from(gas_used) * U256::from(gas_price);
                total_gas_fees += gas_fees;

                // coinbase is always present in the result state
                coinbase_balance_after_tx =
                    state.get(&coinbase).map(|acc| acc.info.balance).unwrap_or_default();
                let coinbase_diff =
                    coinbase_balance_after_tx.saturating_sub(coinbase_balance_before_tx);
                let eth_sent_to_coinbase = coinbase_diff.saturating_sub(gas_fees);

                // update the coinbase balance
                coinbase_balance_before_tx = coinbase_balance_after_tx;

                // set the return data for the response
                let (value, revert) = if result.is_success() {
                    let value = result.into_output().unwrap_or_default();
                    (Some(value), None)
                } else {
                    let revert = result.into_output().unwrap_or_default();
                    (None, Some(revert))
                };

                let tx_res = EthCallBundleTransactionResult {
                    coinbase_diff,
                    eth_sent_to_coinbase,
                    from_address: signer,
                    gas_fees,
                    gas_price: U256::from(gas_price),
                    gas_used,
                    to_address: tx.to(),
                    tx_hash: *tx.tx_hash(),
                    value,
                    revert,
                };
                results.push(tx_res);

                // need to apply the state changes of this call before executing the
                // next call
                if transactions.peek().is_some() {
                    // need to apply the state changes of this call before executing
                    // the next call
                    evm.db_mut().commit(state)
                }
            }

            let builder_balances_after_tx = builder_addresses
                .clone()
                .into_iter()
                .map(|addr| {
                    DatabaseRef::basic_ref(&evm.db(), addr)
                        .map(|acc| acc.map_or(U256::ZERO, |i| i.balance))
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>();

            let res = EthSimulateBlockResponse {
                coinbase_before:        initial_coinbase,
                coinbase_after:         coinbase_balance_after_tx,
                builder_balances_before: builder_balances_before_tx,
                builder_balances_after:  builder_balances_after_tx,
                gas_fees:               total_gas_fees,
                results,
                state_block_number:     state_block_number.to(),
                total_gas_used,
                option,
                from_builder,
                to_proposer: to_proposer,
                actual_value,
            };

            Ok(res)
        })
        .await
    }
}

#[async_trait::async_trait]
impl<Eth> EthCallBundleApiServer for EthBundle<Eth>
where
    Eth: EthTransactions + LoadPendingBlock + Call + 'static,
{
    async fn call_bundle(&self, request: EthCallBundle) -> RpcResult<EthCallBundleResponse> {
        Self::call_bundle(self, request).await.map_err(Into::into)
    }
    async fn simulate_block(&self, request: EthSimulateBlock) -> RpcResult<EthSimulateBlockResponse> {
        Self::simulate_block(self, request).await.map_err(Into::into)
    }
}

/// Container type for `EthBundle` internals
#[derive(Debug)]
struct EthBundleInner<Eth> {
    /// Access to commonly used code of the `eth` namespace
    eth_api: Eth,
    // restrict the number of concurrent tracing calls.
    #[expect(dead_code)]
    blocking_task_guard: BlockingTaskGuard,
}

impl<Eth> std::fmt::Debug for EthBundle<Eth> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EthBundle").finish_non_exhaustive()
    }
}

impl<Eth> Clone for EthBundle<Eth> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

/// [`EthBundle`] specific errors.
#[derive(Debug, thiserror::Error)]
pub enum EthBundleError {
    /// Thrown if the bundle does not contain any transactions.
    #[error("bundle missing txs")]
    EmptyBundleTransactions,
    /// Thrown if the bundle does not contain a block number, or block number is 0.
    #[error("bundle missing blockNumber")]
    BundleMissingBlockNumber,
    /// Thrown when the blob gas usage of the blob transactions in a bundle exceed the maximum.
    #[error("blob gas usage exceeds the limit of {0} gas per block.")]
    Eip4844BlobGasExceeded(u64),
}
