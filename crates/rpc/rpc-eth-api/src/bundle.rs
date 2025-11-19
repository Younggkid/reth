//! Additional `eth_` RPC API for bundles.
//!
//! See also <https://docs.flashbots.net/flashbots-auction/advanced/rpc-endpoint>

use alloy_primitives::{Bytes, B256};
use alloy_rpc_types_mev::{
    EthBundleHash, EthCallBundle, EthCallBundleResponse, EthCancelBundle,
    EthCancelPrivateTransaction, EthSendBundle, EthSendPrivateTransaction,
};
use jsonrpsee::proc_macros::rpc;
use serde::{
    Deserialize, Serialize,
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, U256};
use alloy_rpc_types_mev::{EthCallBundleTransactionResult};

mod u256_numeric_string {
    use alloy_primitives::U256;
    use serde::{de, Deserialize, Serializer};
    use std::str::FromStr;

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(deserializer)?;
        match val {
            serde_json::Value::String(s) => {
                if let Ok(val) = s.parse::<u128>() {
                    return Ok(U256::from(val));
                }
                U256::from_str(&s).map_err(de::Error::custom)
            }
            serde_json::Value::Number(num) => {
                num.as_u64().map(U256::from).ok_or_else(|| de::Error::custom("invalid u256"))
            }
            _ => Err(de::Error::custom("invalid u256")),
        }
    }

    pub(crate) fn serialize<S>(val: &U256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let val: u128 = (*val).try_into().map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&val.to_string())
    }

}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EthSimulateBlock{
    /// A list of hex-encoded signed transactions
    pub txs: Vec<Bytes>,
    /// hex encoded block number for which this bundle is valid on
    pub block_number: u64,
    /// Either a hex encoded number or a block tag for which state to base this simulation on
    pub state_block_number: BlockNumberOrTag,
    /// coinbase used for simulation
    pub coinbase: Address,
    /// proposer address used for simulation
    pub proposer_address: Address,
    /// base fee used for simulation
    pub base_fee: Option<u128>,
    /// builder address list
    pub builder_addresses: Vec<Address>,
    /// the timestamp to use for this bundle simulation, in seconds since the unix epoch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

/// Response for `eth_simulateBlock`
#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EthSimulateBlockResponse {
    /// The balance of the coinbase before the block
    #[serde(with = "u256_numeric_string")]
    pub coinbase_before: U256,
    /// The balance of the coinbase after the block
    #[serde(with = "u256_numeric_string")]
    pub coinbase_after: U256,
    /// The balance of the builders before the block
    pub builder_balances_before: Vec<U256>,
    /// The balance of the builders after the block
    pub builder_balances_after: Vec<U256>,
    /// The total gas fees paid for all transactions in the bundle
    #[serde(with = "u256_numeric_string")]
    pub gas_fees: U256,
    /// Results of individual transactions within the bundle
    pub results: Vec<EthCallBundleTransactionResult>,
    /// The block number used as a base for this simulation
     #[serde(with = "alloy_serde::quantity")]
    pub state_block_number: u64,
    /// The total gas used by all transactions in the bundle
    #[serde(with = "alloy_serde::quantity")]
    pub total_gas_used: u64,

    pub option: u64,
    
    pub from_builder: Address,

    pub to_proposer: Option<Address>,

    pub actual_value: U256,
}


/// A subset of the [EthBundleApi] API interface that only supports `eth_callBundle`.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "eth"))]
pub trait EthCallBundleApi {
    /// `eth_callBundle` can be used to simulate a bundle against a specific block number,
    /// including simulating a bundle at the top of the next block.
    #[method(name = "callBundle")]
    async fn call_bundle(
        &self,
        request: EthCallBundle,
    ) -> jsonrpsee::core::RpcResult<EthCallBundleResponse>;

        /// `eth_simulateBlock` can be used to simulate a block at a specific block number.
    #[method(name = "simulateBlock")]
    async fn simulate_block(
        &self,
        request: EthSimulateBlock,
    ) -> jsonrpsee::core::RpcResult<EthSimulateBlockResponse>;
}

/// The __full__ Eth bundle rpc interface.
///
/// See also <https://docs.flashbots.net/flashbots-auction/advanced/rpc-endpoint>
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "eth"))]
pub trait EthBundleApi {
    /// `eth_sendBundle` can be used to send your bundles to the builder.
    #[method(name = "sendBundle")]
    async fn send_bundle(&self, bundle: EthSendBundle)
        -> jsonrpsee::core::RpcResult<EthBundleHash>;

    /// `eth_callBundle` can be used to simulate a bundle against a specific block number,
    /// including simulating a bundle at the top of the next block.
    #[method(name = "callBundle")]
    async fn call_bundle(
        &self,
        request: EthCallBundle,
    ) -> jsonrpsee::core::RpcResult<EthCallBundleResponse>;

    #[method(name = "simulateBlock")]
    async fn simulate_block(
        &self,
        request: EthSimulateBlock,
    ) -> jsonrpsee::core::RpcResult<EthSimulateBlockResponse>;  

    /// `eth_cancelBundle` is used to prevent a submitted bundle from being included on-chain. See [bundle cancellations](https://docs.flashbots.net/flashbots-auction/advanced/bundle-cancellations) for more information.
    #[method(name = "cancelBundle")]
    async fn cancel_bundle(&self, request: EthCancelBundle) -> jsonrpsee::core::RpcResult<()>;

    /// `eth_sendPrivateTransaction` is used to send a single transaction to Flashbots. Flashbots will attempt to build a block including the transaction for the next 25 blocks. See [Private Transactions](https://docs.flashbots.net/flashbots-protect/additional-documentation/eth-sendPrivateTransaction) for more info.
    #[method(name = "sendPrivateTransaction")]
    async fn send_private_transaction(
        &self,
        request: EthSendPrivateTransaction,
    ) -> jsonrpsee::core::RpcResult<B256>;

    /// The `eth_sendPrivateRawTransaction` method can be used to send private transactions to
    /// the RPC endpoint. Private transactions are protected from frontrunning and kept
    /// private until included in a block. A request to this endpoint needs to follow
    /// the standard `eth_sendRawTransaction`
    #[method(name = "sendPrivateRawTransaction")]
    async fn send_private_raw_transaction(&self, bytes: Bytes) -> jsonrpsee::core::RpcResult<B256>;

    /// The `eth_cancelPrivateTransaction` method stops private transactions from being
    /// submitted for future blocks.
    ///
    /// A transaction can only be cancelled if the request is signed by the same key as the
    /// `eth_sendPrivateTransaction` call submitting the transaction in first place.
    #[method(name = "cancelPrivateTransaction")]
    async fn cancel_private_transaction(
        &self,
        request: EthCancelPrivateTransaction,
    ) -> jsonrpsee::core::RpcResult<bool>;
}
