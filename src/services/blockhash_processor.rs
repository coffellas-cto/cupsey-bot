use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use solana_sdk::hash::Hash;
use solana_client::rpc_client::RpcClient;
use anyhow::{Result, anyhow};
use colored::Colorize;
use lazy_static::lazy_static;
use crate::common::logger::Logger;
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, 
    SubscribeRequestFilterBlocks, SubscribeUpdate,
};
use futures_util::stream::StreamExt;

// Global state for latest blockhash and timestamp
lazy_static! {
    static ref LATEST_BLOCKHASH: Arc<RwLock<Option<Hash>>> = Arc::new(RwLock::new(None));
    static ref BLOCKHASH_LAST_UPDATED: Arc<RwLock<Option<Instant>>> = Arc::new(RwLock::new(None));
    static ref BLOCK_HEIGHT: Arc<RwLock<Option<u64>>> = Arc::new(RwLock::new(None));
}

const BLOCKHASH_STALENESS_THRESHOLD: Duration = Duration::from_secs(5); // Reduced from 10s to 5s for better freshness
const UPDATE_INTERVAL: Duration = Duration::from_millis(100); // Increased frequency from 300ms to 200ms

pub struct BlockhashProcessor {
    rpc_client: Arc<RpcClient>,
    logger: Logger,
}

impl BlockhashProcessor {
    pub async fn new(rpc_client: Arc<RpcClient>) -> Result<Self> {
        let logger = Logger::new("[BLOCKHASH-PROCESSOR] => ".cyan().to_string());
        
        Ok(Self {
            rpc_client,
            logger,
        })
    }

    pub async fn start(&self) -> Result<()> {
        self.logger.log("Starting blockhash processor...".green().to_string());

        // Clone necessary components for the background task
        let rpc_client = self.rpc_client.clone();
        let logger = self.logger.clone();

        tokio::spawn(async move {
            loop {
                match Self::update_blockhash_from_rpc(&rpc_client).await {
                    Ok(blockhash) => {
                        // Update global blockhash
                        let mut latest = LATEST_BLOCKHASH.write().await;
                        *latest = Some(blockhash);
                        
                        // Update timestamp
                        let mut last_updated = BLOCKHASH_LAST_UPDATED.write().await;
                        *last_updated = Some(Instant::now());
                        
                        // logger.log(format!("Updated latest blockhash: {}", blockhash));
                    }
                    Err(e) => {
                        logger.log(format!("Error getting latest blockhash: {}", e).red().to_string());
                    }
                }

                tokio::time::sleep(UPDATE_INTERVAL).await;
            }
        });

        Ok(())
    }

    /// Start real-time blockhash updates using Yellowstone gRPC WebSocket
    pub async fn start_realtime_with_grpc(
        &self, 
        grpc_url: String, 
        grpc_token: String
    ) -> Result<()> {
        self.logger.log("Starting REAL-TIME blockhash processor with Yellowstone gRPC...".green().bold().to_string());
        
        let logger = self.logger.clone();
        
        tokio::spawn(async move {
            loop {
                match Self::grpc_blockhash_stream(grpc_url.clone(), grpc_token.clone(), logger.clone()).await {
                    Ok(_) => {
                        logger.log("gRPC blockhash stream ended, reconnecting...".yellow().to_string());
                    }
                    Err(e) => {
                        logger.log(format!("gRPC blockhash stream error: {}, reconnecting in 5s...", e).red().to_string());
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
        
        Ok(())
    }

    /// Real-time blockhash updates via Yellowstone gRPC block subscription
    async fn grpc_blockhash_stream(
        grpc_url: String,
        grpc_token: String, 
        logger: Logger
    ) -> Result<()> {
        logger.log("Connecting to Yellowstone gRPC for real-time blockhash...".green().to_string());
        
        let mut client = GeyserGrpcClient::build_from_shared(grpc_url)
            .map_err(|e| anyhow!("Failed to build gRPC client: {}", e))?
            .x_token::<String>(Some(grpc_token))
            .map_err(|e| anyhow!("Failed to set x_token: {}", e))?
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|e| anyhow!("Failed to set tls config: {}", e))?
            .connect()
            .await
            .map_err(|e| anyhow!("Failed to connect to gRPC: {}", e))?;

        let (mut subscribe_tx, mut stream) = client.subscribe().await
            .map_err(|e| anyhow!("Failed to subscribe: {}", e))?;

        // Subscribe to block updates for real-time blockhash
        let subscription_request = SubscribeRequest {
            blocks: maplit::hashmap! {
                "blocks".to_owned() => SubscribeRequestFilterBlocks {
                    account_include: vec![], 
                    include_transactions: Some(false), // We only need block metadata
                    include_accounts: Some(false),
                    include_entries: Some(false),
                }
            },
            commitment: Some(CommitmentLevel::Processed as i32), // Use processed for fastest updates
            ..Default::default()
        };

        subscribe_tx.send(subscription_request).await
            .map_err(|e| anyhow!("Failed to send subscription: {}", e))?;

        logger.log("🚀 Real-time blockhash stream started - will update on every new block!".green().bold().to_string());

        // Process incoming blocks for immediate blockhash updates
        while let Some(msg_result) = stream.next().await {
            match msg_result {
                Ok(msg) => {
                    if let Some(UpdateOneof::Block(block_update)) = msg.update_oneof {
                        if let Some(block) = block_update.block {
                            // Extract blockhash from block
                            let blockhash_str = block.blockhash;
                            if let Ok(blockhash_bytes) = bs58::decode(&blockhash_str).into_vec() {
                                if blockhash_bytes.len() == 32 {
                                    let mut hash_array = [0u8; 32];
                                    hash_array.copy_from_slice(&blockhash_bytes);
                                    let new_blockhash = Hash::new(&hash_array);
                                    
                                    // Update global state immediately
                                    Self::update_blockhash(new_blockhash).await;
                                    
                                    // Update block height
                                    let mut height = BLOCK_HEIGHT.write().await;
                                    *height = Some(block.block_height);
                                    
                                    logger.log(format!(
                                        "⚡ REAL-TIME blockhash update: {} (block: {})", 
                                        new_blockhash, 
                                        block.block_height
                                    ).cyan().to_string());
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    logger.log(format!("gRPC stream error: {:?}", e).red().to_string());
                    return Err(anyhow!("gRPC stream error: {:?}", e));
                }
            }
        }
        
        Ok(())
    }

    async fn update_blockhash_from_rpc(rpc_client: &RpcClient) -> Result<Hash> {
        rpc_client.get_latest_blockhash()
            .map_err(|e| anyhow!("Failed to get blockhash from RPC: {}", e))
    }

    /// Update the latest blockhash and its timestamp
    async fn update_blockhash(hash: Hash) {
        let mut latest = LATEST_BLOCKHASH.write().await;
        *latest = Some(hash);
        
        let mut last_updated = BLOCKHASH_LAST_UPDATED.write().await;
        *last_updated = Some(Instant::now());
    }

    /// Get the latest cached blockhash with freshness check
    pub async fn get_latest_blockhash() -> Option<Hash> {
        // Check if blockhash is stale
        let last_updated = BLOCKHASH_LAST_UPDATED.read().await;
        if let Some(instant) = *last_updated {
            if instant.elapsed() > BLOCKHASH_STALENESS_THRESHOLD {
                return None;
            }
        }
        
        let latest = LATEST_BLOCKHASH.read().await;
        *latest
    }

    /// Get a fresh blockhash, falling back to RPC if necessary
    pub async fn get_fresh_blockhash(&self) -> Result<Hash> {
        if let Some(hash) = Self::get_latest_blockhash().await {
            return Ok(hash);
        }
        
        // Fallback to RPC if cached blockhash is stale or missing
        self.logger.log("Cached blockhash is stale or missing, falling back to RPC...".yellow().to_string());
        let new_hash = self.rpc_client.get_latest_blockhash()
            .map_err(|e| anyhow!("Failed to get blockhash from RPC: {}", e))?;
        
        Self::update_blockhash(new_hash).await;
        Ok(new_hash)
    }

    /// Get blockhash with ZERO latency - directly from RPC without caching
    /// Use this for critical transactions like emergency sells
    pub async fn get_immediate_blockhash(&self) -> Result<Hash> {
        self.logger.log("🔥 Getting IMMEDIATE blockhash directly from RPC...".red().bold().to_string());
        let start = Instant::now();
        
        let hash = self.rpc_client.get_latest_blockhash()
            .map_err(|e| anyhow!("Failed to get immediate blockhash: {}", e))?;
            
        self.logger.log(format!("⚡ Immediate blockhash fetched in {:?}: {}", start.elapsed(), hash).green().to_string());
        
        // Update cache as well
        Self::update_blockhash(hash).await;
        
        Ok(hash)
    }

    /// Get blockhash with the highest freshness guarantee
    /// Tries: 1) Real-time cache 2) Immediate RPC 3) Fallback RPC
    pub async fn get_ultra_fresh_blockhash(&self) -> Result<Hash> {
        // Try cached first (if very fresh - under 1 second)
        let last_updated = BLOCKHASH_LAST_UPDATED.read().await;
        if let Some(instant) = *last_updated {
            if instant.elapsed() < Duration::from_secs(1) {
                drop(last_updated);
                if let Some(hash) = Self::get_latest_blockhash().await {
                    self.logger.log("🚀 Using ultra-fresh cached blockhash (< 1s old)".green().to_string());
                    return Ok(hash);
                }
            }
        }
        drop(last_updated);
        
        // Otherwise get immediate from RPC
        self.get_immediate_blockhash().await
    }

    /// Get current block height (if available from real-time stream)
    pub async fn get_current_block_height() -> Option<u64> {
        let height = BLOCK_HEIGHT.read().await;
        *height
    }
} 