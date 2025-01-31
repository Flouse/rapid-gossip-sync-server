use std::collections::HashMap;
use std::io::ErrorKind;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;

use bitcoin::blockdata::constants::ChainHash;
use bitcoin::{BlockHash, TxOut};
use bitcoin::blockdata::block::Block;
use bitcoin::hashes::Hash;
use lightning::log_error;
use lightning::routing::gossip::{NetworkGraph, P2PGossipSync};
use lightning::routing::utxo::{UtxoFuture, UtxoLookup, UtxoResult, UtxoLookupError};
use lightning::util::logger::Logger;
use lightning_block_sync::{BlockData, BlockSource};
use lightning_block_sync::http::BinaryResponse;
use lightning_block_sync::rest::RestClient;

use crate::config;
use crate::types::GossipPeerManager;

pub(crate) struct ChainVerifier<L: Deref + Clone + Send + Sync + 'static> where L::Target: Logger {
	rest_client: Arc<RestClient>,
	graph: Arc<NetworkGraph<L>>,
	outbound_gossiper: Arc<P2PGossipSync<Arc<NetworkGraph<L>>, Arc<Self>, L>>,
	peer_handler: Mutex<Option<GossipPeerManager<L>>>,
	/// A cache on the funding amounts for each channel that we've looked up, mapping from SCID to
	/// funding satoshis.
	channel_funding_amounts: Arc<Mutex<HashMap<u64, u64>>>,
	logger: L
}

struct RestBinaryResponse(Vec<u8>);

impl<L: Deref + Clone + Send + Sync + 'static> ChainVerifier<L> where L::Target: Logger {
	pub(crate) fn new(graph: Arc<NetworkGraph<L>>, outbound_gossiper: Arc<P2PGossipSync<Arc<NetworkGraph<L>>, Arc<Self>, L>>, logger: L) -> Self {
		ChainVerifier {
			rest_client: Arc::new(RestClient::new(config::bitcoin_rest_endpoint())),
			outbound_gossiper,
			graph,
			peer_handler: Mutex::new(None),
			channel_funding_amounts: Arc::new(Mutex::new(HashMap::new())),
			logger,
		}
	}
	pub(crate) fn set_ph(&self, peer_handler: GossipPeerManager<L>) {
		*self.peer_handler.lock().unwrap() = Some(peer_handler);
	}

	pub(crate) fn get_cached_funding_value(&self, scid: u64) -> Option<u64> {
		self.channel_funding_amounts.lock().unwrap().get(&scid).map(|v| *v)
	}

	pub(crate) async fn retrieve_funding_value(&self, scid: u64) -> Result<u64, UtxoLookupError> {
		Self::retrieve_cache_txo(Arc::clone(&self.rest_client), Some(Arc::clone(&self.channel_funding_amounts)), scid, self.logger.clone())
			.await.map(|txo| txo.value.to_sat())
	}

	pub(crate) async fn retrieve_txo(client: Arc<RestClient>, short_channel_id: u64, logger: L) -> Result<TxOut, UtxoLookupError> {
		Self::retrieve_cache_txo(client, None, short_channel_id, logger).await
	}

	async fn retrieve_cache_txo(client: Arc<RestClient>, channel_funding_amounts: Option<Arc<Mutex<HashMap<u64, u64>>>>, short_channel_id: u64, logger: L) -> Result<TxOut, UtxoLookupError> {
		let block_height = (short_channel_id >> 5 * 8) as u32; // block height is most significant three bytes
		let transaction_index = ((short_channel_id >> 2 * 8) & 0xffffff) as u32;
		let output_index = (short_channel_id & 0xffff) as u16;

		let mut block = Self::retrieve_block(client, block_height, logger.clone()).await?;
		if transaction_index as usize >= block.txdata.len() {
			log_error!(logger, "Could't find transaction {} in block {}", transaction_index, block_height);
			return Err(UtxoLookupError::UnknownTx);
		}
		let mut transaction = block.txdata.swap_remove(transaction_index as usize);
		if output_index as usize >= transaction.output.len() {
			log_error!(logger, "Could't find output {} in transaction {}", output_index, transaction.compute_txid());
			return Err(UtxoLookupError::UnknownTx);
		}
		let txo = transaction.output.swap_remove(output_index as usize);
		if let Some(channel_funding_amounts) = channel_funding_amounts {
			channel_funding_amounts.lock().unwrap().insert(short_channel_id, txo.value.to_sat());
		}
		Ok(txo)
	}

	async fn retrieve_block(client: Arc<RestClient>, block_height: u32, logger: L) -> Result<Block, UtxoLookupError> {
		let uri = format!("blockhashbyheight/{}.bin", block_height);
		let block_hash_result =
			client.request_resource::<BinaryResponse, RestBinaryResponse>(&uri).await;
		let block_hash: Vec<u8> = block_hash_result.map_err(|error| {
			match error.kind() {
				ErrorKind::InvalidData => {
					// the response length was likely 0
					log_error!(logger, "Could't find block hash at height {}: Invalid response! Please make sure the `-rest=1` flag is set.", block_height);
				}
				_ => {
					log_error!(logger, "Could't find block hash at height {}: {}", block_height, error.to_string());
				}
			}
			UtxoLookupError::UnknownChain
		})?.0;
		let block_hash = BlockHash::from_slice(&block_hash).unwrap();

		let block_result = client.get_block(&block_hash).await;
		match block_result {
			Ok(BlockData::FullBlock(block)) => {
				Ok(block)
			},
			Ok(_) => unreachable!(),
			Err(error) => {
				log_error!(logger, "Couldn't retrieve block {}: {:?} ({})", block_height, error, block_hash);
				Err(UtxoLookupError::UnknownChain)
			}
		}
	}
}

impl<L: Deref + Clone + Send + Sync + 'static> UtxoLookup for ChainVerifier<L> where L::Target: Logger {
	fn get_utxo(&self, _genesis_hash: &ChainHash, short_channel_id: u64) -> UtxoResult {
		let res = UtxoFuture::new();
		let fut = res.clone();
		let graph_ref = Arc::clone(&self.graph);
		let client_ref = Arc::clone(&self.rest_client);
		let gossip_ref = Arc::clone(&self.outbound_gossiper);
		let channel_funding_amounts_cache_ref = Arc::clone(&self.channel_funding_amounts);
		let pm_ref = self.peer_handler.lock().unwrap().clone();
		let logger_ref = self.logger.clone();
		tokio::spawn(async move {
			let res = Self::retrieve_cache_txo(client_ref, Some(channel_funding_amounts_cache_ref), short_channel_id, logger_ref).await;
			fut.resolve(&*graph_ref, &*gossip_ref, res);
			if let Some(pm) = pm_ref { pm.process_events(); }
		});
		UtxoResult::Async(res)
	}
}

impl TryInto<RestBinaryResponse> for BinaryResponse {
	type Error = std::io::Error;

	fn try_into(self) -> Result<RestBinaryResponse, Self::Error> {
		Ok(RestBinaryResponse(self.0))
	}
}
