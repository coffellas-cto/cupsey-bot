use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Notify};
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
use futures_util::SinkExt; // Add this import for send method
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// Global state for latest blockhash and timestamp
lazy_static! {
    static ref LATEST_BLOCKHASH: Arc<RwLock<Option<Hash>>> = Arc::new(RwLock::new(None));
    static ref BLOCKHASH_LAST_UPDATED: Arc<RwLock<Option<Instant>>> = Arc::new(RwLock::new(None));
    static ref CONNECTION_STATUS: Arc<RwLock<ConnectionStatus>> = Arc::new(RwLock::new(ConnectionStatus::Disconnected));
    static ref BLOCKHASH_UPDATED_NOTIFY: Arc<Notify> = Arc::new(Notify::new());
}

// Connection status tracking
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    ConnectedGrpc,
    ConnectedWebSocket,
    Error(String),
}

// Connection method enum
#[derive(Debug, Clone)]
pub enum ConnectionMethod {
    Grpc { url: String, token: String },
    WebSocket { url: String },
    Hybrid { grpc_url: String, grpc_token: String, ws_url: String },
}

const BLOCKHASH_STALENESS_THRESHOLD: Duration = Duration::from_secs(3); // Reduced to 3s for better freshness
const UPDATE_INTERVAL: Duration = Duration::from_millis(50); // Increased frequency to 50ms
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

static RECONNECT_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static IS_REAL_TIME_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct BlockhashProcessor {
    rpc_client: Arc<RpcClient>,
    logger: Logger,
    connection_method: Option<ConnectionMethod>,
}

impl BlockhashProcessor {
    pub async fn new(rpc_client: Arc<RpcClient>) -> Result<Self> {
        let logger = Logger::new("[BLOCKHASH-PROCESSOR] => ".cyan().to_string());
        
        Ok(Self {
            rpc_client,
            logger,
            connection_method: None,
        })
    }

    /// Set the preferred connection method for real-time updates
    pub fn set_connection_method(&mut self, method: ConnectionMethod) {
        self.connection_method = Some(method);
    }

    /// Start the blockhash processor with the configured connection method
    pub async fn start(&self) -> Result<()> {
        self.logger.log("🚀 Starting enhanced blockhash processor...".green().bold().to_string());

        // Start fallback RPC polling (as backup)
        self.start_fallback_polling().await?;

        // Start real-time connection if configured
        if let Some(ref method) = self.connection_method {
            match method {
                ConnectionMethod::Grpc { url, token } => {
                    self.start_realtime_with_grpc(url.clone(), token.clone()).await?;
                }
                ConnectionMethod::WebSocket { url } => {
                    self.start_realtime_with_websocket(url.clone()).await?;
                }
                ConnectionMethod::Hybrid { grpc_url, grpc_token, ws_url } => {
                    // Start both connections for maximum reliability
                    self.start_realtime_with_grpc(grpc_url.clone(), grpc_token.clone()).await?;
                    tokio::time::sleep(Duration::from_millis(100)).await; // Small delay
                    self.start_realtime_with_websocket(ws_url.clone()).await?;
                }
            }
        } else {
            self.logger.log("⚠️  No real-time connection method configured, using RPC polling only".yellow().to_string());
        }

        Ok(())
    }

    /// Start fallback RPC polling as backup
    async fn start_fallback_polling(&self) -> Result<()> {
        let rpc_client = self.rpc_client.clone();
        let logger = self.logger.clone();

        tokio::spawn(async move {
            let mut last_poll = Instant::now();
            
            loop {
                // Only poll if real-time is not active or it's been too long since last update
                let should_poll = if IS_REAL_TIME_ACTIVE.load(Ordering::Relaxed) {
                    // Check if real-time data is stale
                    let last_updated = BLOCKHASH_LAST_UPDATED.read().await;
                    if let Some(instant) = *last_updated {
                        instant.elapsed() > Duration::from_secs(10) // Real-time seems inactive
                    } else {
                        true
                    }
                } else {
                    true
                };

                if should_poll || last_poll.elapsed() > Duration::from_secs(5) {
                    match Self::update_blockhash_from_rpc(&rpc_client).await {
                        Ok(blockhash) => {
                            Self::update_blockhash(blockhash).await;
                            if !IS_REAL_TIME_ACTIVE.load(Ordering::Relaxed) {
                                logger.log(format!("📡 RPC fallback updated blockhash: {}", blockhash).dimmed().to_string());
                            }
                        }
                        Err(e) => {
                            logger.log(format!("❌ RPC fallback error: {}", e).red().to_string());
                        }
                    }
                    last_poll = Instant::now();
                }

                tokio::time::sleep(UPDATE_INTERVAL).await;
            }
        });

        Ok(())
    }

    /// Start real-time blockhash updates using WebSocket
    pub async fn start_realtime_with_websocket(&self, ws_url: String) -> Result<()> {
        self.logger.log(format!("🌐 Starting WebSocket real-time blockhash updates: {}", ws_url).green().bold().to_string());
        
        let logger = self.logger.clone();
        
        tokio::spawn(async move {
            let mut reconnect_count = 0u32;
            
            loop {
                match Self::websocket_blockhash_stream(ws_url.clone(), logger.clone()).await {
                    Ok(_) => {
                        logger.log("🔄 WebSocket stream ended, reconnecting...".yellow().to_string());
                        reconnect_count = 0; // Reset on successful connection
                    }
                    Err(e) => {
                        reconnect_count += 1;
                        if reconnect_count >= MAX_RECONNECT_ATTEMPTS {
                            logger.log(format!("❌ Max WebSocket reconnection attempts reached ({}), giving up", MAX_RECONNECT_ATTEMPTS).red().bold().to_string());
                            break;
                        }
                        
                        let delay = RECONNECT_DELAY.mul_f32(reconnect_count as f32);
                        logger.log(format!("❌ WebSocket error (attempt {}): {}, reconnecting in {:?}...", reconnect_count, e, delay).red().to_string());
                        
                        Self::update_connection_status(ConnectionStatus::Error(e.to_string())).await;
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        });
        
        Ok(())
    }

    /// WebSocket stream implementation for Solana blockhash updates
    async fn websocket_blockhash_stream(ws_url: String, logger: Logger) -> Result<()> {
        logger.log("🔌 Connecting to Solana WebSocket...".green().to_string());
        
        Self::update_connection_status(ConnectionStatus::Connecting).await;
        
        let (ws_stream, _) = connect_async(&ws_url).await
            .map_err(|e| anyhow!("Failed to connect to WebSocket: {}", e))?;

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        
        // Subscribe to block notifications
        let subscribe_msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "blockSubscribe",
            "params": [
                "finalized", // Use finalized for stability, or "confirmed" for faster updates
                {
                    "commitment": "finalized",
                    "encoding": "json",
                    "transactionDetails": "none",
                    "rewards": false,
                    "maxSupportedTransactionVersion": 0
                }
            ]
        });

        let subscribe_text = serde_json::to_string(&subscribe_msg)?;
        ws_sender.send(Message::Text(subscribe_text)).await
            .map_err(|e| anyhow!("Failed to send subscription: {}", e))?;

        Self::update_connection_status(ConnectionStatus::ConnectedWebSocket).await;
        IS_REAL_TIME_ACTIVE.store(true, Ordering::Relaxed);
        
        logger.log("🚀 WebSocket real-time blockhash stream active!".green().bold().to_string());

        // Process incoming messages
        while let Some(message_result) = ws_receiver.next().await {
            match message_result {
                Ok(Message::Text(text)) => {
                    if let Ok(data) = serde_json::from_str::<Value>(&text) {
                        if let Some(params) = data.get("params") {
                            if let Some(result) = params.get("result") {
                                if let Some(value) = result.get("value") {
                                    if let Some(block) = value.get("block") {
                                        if let Some(blockhash_str) = block.get("blockhash").and_then(|v| v.as_str()) {
                                            if let Ok(blockhash_bytes) = bs58::decode(blockhash_str).into_vec() {
                                                if blockhash_bytes.len() == 32 {
                                                    let mut hash_array = [0u8; 32];
                                                    hash_array.copy_from_slice(&blockhash_bytes);
                                                    let new_blockhash = Hash::new(&hash_array);
                                                    
                                                    Self::update_blockhash(new_blockhash).await;
                                                    
                                                    logger.log(format!(
                                                        "🌐 WebSocket blockhash update: {}", 
                                                        new_blockhash
                                                    ).cyan().to_string());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    logger.log("🔌 WebSocket connection closed".yellow().to_string());
                    break;
                }
                Err(e) => {
                    logger.log(format!("❌ WebSocket message error: {:?}", e).red().to_string());
                    return Err(anyhow!("WebSocket message error: {:?}", e));
                }
                _ => {}
            }
        }
        
        IS_REAL_TIME_ACTIVE.store(false, Ordering::Relaxed);
        Self::update_connection_status(ConnectionStatus::Disconnected).await;
        
        Ok(())
    }

    /// Enhanced gRPC real-time blockhash updates
    pub async fn start_realtime_with_grpc(
        &self, 
        grpc_url: String, 
        grpc_token: String
    ) -> Result<()> {
        self.logger.log("⚡ Starting enhanced gRPC real-time blockhash updates...".green().bold().to_string());
        
        let logger = self.logger.clone();
        
        tokio::spawn(async move {
            let mut reconnect_count = 0u32;
            
            loop {
                match Self::grpc_blockhash_stream(grpc_url.clone(), grpc_token.clone(), logger.clone()).await {
                    Ok(_) => {
                        logger.log("🔄 gRPC stream ended, reconnecting...".yellow().to_string());
                        reconnect_count = 0;
                    }
                    Err(e) => {
                        reconnect_count += 1;
                        if reconnect_count >= MAX_RECONNECT_ATTEMPTS {
                            logger.log(format!("❌ Max gRPC reconnection attempts reached ({}), giving up", MAX_RECONNECT_ATTEMPTS).red().bold().to_string());
                            break;
                        }
                        
                        let delay = RECONNECT_DELAY.mul_f32((reconnect_count as f32).min(5.0));
                        logger.log(format!("❌ gRPC error (attempt {}): {}, reconnecting in {:?}...", reconnect_count, e, delay).red().to_string());
                        
                        Self::update_connection_status(ConnectionStatus::Error(e.to_string())).await;
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        });
        
        Ok(())
    }

    /// Enhanced gRPC blockhash stream with better error handling
    async fn grpc_blockhash_stream(
        grpc_url: String,
        grpc_token: String, 
        logger: Logger
    ) -> Result<()> {
        logger.log("🔌 Connecting to Yellowstone gRPC...".green().to_string());
        
        Self::update_connection_status(ConnectionStatus::Connecting).await;
        
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

        // Enhanced subscription with minimal data transfer
        let subscription_request = SubscribeRequest {
            blocks: maplit::hashmap! {
                "blocks".to_owned() => SubscribeRequestFilterBlocks {
                    account_include: vec![], 
                    include_transactions: Some(false), // Only block metadata
                    include_accounts: Some(false),
                    include_entries: Some(false),
                }
            },
            commitment: Some(CommitmentLevel::Processed as i32), // Fastest updates
            ..Default::default()
        };

        subscribe_tx.send(subscription_request).await
            .map_err(|e| anyhow!("Failed to send subscription: {}", e))?;

        Self::update_connection_status(ConnectionStatus::ConnectedGrpc).await;
        IS_REAL_TIME_ACTIVE.store(true, Ordering::Relaxed);
        
        logger.log("🚀 Enhanced gRPC real-time blockhash stream active!".green().bold().to_string());

        let mut block_count = 0u64;
        let start_time = Instant::now();

        // Process incoming blocks
        while let Some(msg_result) = stream.next().await {
            match msg_result {
                Ok(msg) => {
                    if let Some(UpdateOneof::Block(block_update)) = msg.update_oneof {
                        // Extract blockhash directly from block_update
                        let blockhash_str = block_update.blockhash;
                        
                        if let Ok(blockhash_bytes) = bs58::decode(&blockhash_str).into_vec() {
                            if blockhash_bytes.len() == 32 {
                                let mut hash_array = [0u8; 32];
                                hash_array.copy_from_slice(&blockhash_bytes);
                                let new_blockhash = Hash::new(&hash_array);
                                
                                Self::update_blockhash(new_blockhash).await;
                                
                                // Block height tracking removed as it's not essential
                                
                                block_count += 1;
                                
                                // Log with performance metrics
                                if block_count % 100 == 0 {
                                    let avg_time = start_time.elapsed().as_secs_f64() / block_count as f64;
                                    logger.log(format!(
                                        "⚡ gRPC: {} blocks processed, avg {:.2}s/block, latest: {}", 
                                        block_count,
                                        avg_time,
                                        new_blockhash
                                    ).cyan().to_string());
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    logger.log(format!("❌ gRPC stream error: {:?}", e).red().to_string());
                    IS_REAL_TIME_ACTIVE.store(false, Ordering::Relaxed);
                    return Err(anyhow!("gRPC stream error: {:?}", e));
                }
            }
        }
        
        IS_REAL_TIME_ACTIVE.store(false, Ordering::Relaxed);
        Self::update_connection_status(ConnectionStatus::Disconnected).await;
        
        Ok(())
    }

    async fn update_blockhash_from_rpc(rpc_client: &RpcClient) -> Result<Hash> {
        rpc_client.get_latest_blockhash()
            .map_err(|e| anyhow!("Failed to get blockhash from RPC: {}", e))
    }

    /// Update the latest blockhash and notify waiters
    async fn update_blockhash(hash: Hash) {
        let mut latest = LATEST_BLOCKHASH.write().await;
        *latest = Some(hash);
        drop(latest);
        
        let mut last_updated = BLOCKHASH_LAST_UPDATED.write().await;
        *last_updated = Some(Instant::now());
        drop(last_updated);
        
        // Notify any waiters
        BLOCKHASH_UPDATED_NOTIFY.notify_waiters();
    }

    /// Update connection status
    async fn update_connection_status(status: ConnectionStatus) {
        let mut current_status = CONNECTION_STATUS.write().await;
        *current_status = status;
    }

    /// Get current connection status
    pub async fn get_connection_status() -> ConnectionStatus {
        let status = CONNECTION_STATUS.read().await;
        status.clone()
    }

    /// Wait for a fresh blockhash update (with timeout)
    pub async fn wait_for_fresh_blockhash(timeout: Duration) -> Option<Hash> {
        let start = Instant::now();
        
        loop {
            if let Some(hash) = Self::get_latest_blockhash().await {
                return Some(hash);
            }
            
            if start.elapsed() >= timeout {
                return None;
            }
            
            // Wait for notification or timeout
            tokio::select! {
                _ = BLOCKHASH_UPDATED_NOTIFY.notified() => {
                    // Continue loop to check for fresh blockhash
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    // Timeout check
                }
            }
        }
    }

    /// Get the latest cached blockhash with enhanced freshness check
    pub async fn get_latest_blockhash() -> Option<Hash> {
        let last_updated = BLOCKHASH_LAST_UPDATED.read().await;
        if let Some(instant) = *last_updated {
            if instant.elapsed() > BLOCKHASH_STALENESS_THRESHOLD {
                return None;
            }
        } else {
            return None;
        }
        drop(last_updated);
        
        let latest = LATEST_BLOCKHASH.read().await;
        *latest
    }

    /// Get a fresh blockhash with multiple fallback strategies
    pub async fn get_fresh_blockhash(&self) -> Result<Hash> {
        // Strategy 1: Try cached if very fresh
        if let Some(hash) = Self::get_latest_blockhash().await {
            return Ok(hash);
        }
        
        // Strategy 2: Wait briefly for real-time update if active
        if IS_REAL_TIME_ACTIVE.load(Ordering::Relaxed) {
            if let Some(hash) = Self::wait_for_fresh_blockhash(Duration::from_millis(200)).await {
                self.logger.log("✨ Got fresh blockhash from real-time stream".green().to_string());
                return Ok(hash);
            }
        }
        
        // Strategy 3: Fallback to immediate RPC
        self.logger.log("⚠️  No fresh cached blockhash, fetching from RPC...".yellow().to_string());
        self.get_immediate_blockhash().await
    }

    /// Get blockhash with ZERO latency - directly from RPC
    pub async fn get_immediate_blockhash(&self) -> Result<Hash> {
        self.logger.log("🔥 Getting IMMEDIATE blockhash from RPC...".red().bold().to_string());
        let start = Instant::now();
        
        let hash = self.rpc_client.get_latest_blockhash()
            .map_err(|e| anyhow!("Failed to get immediate blockhash: {}", e))?;
            
        self.logger.log(format!("⚡ Immediate blockhash fetched in {:?}: {}", start.elapsed(), hash).green().to_string());
        
        // Update cache
        Self::update_blockhash(hash).await;
        
        Ok(hash)
    }

    /// Get ultra-fresh blockhash with the best available method
    pub async fn get_ultra_fresh_blockhash(&self) -> Result<Hash> {
        // Try ultra-fresh cache first (< 500ms)
        let is_ultra_fresh = {
            let last_updated = BLOCKHASH_LAST_UPDATED.read().await;
            if let Some(instant) = *last_updated {
                instant.elapsed() < Duration::from_millis(500)
            } else {
                false
            }
        };
        
        if is_ultra_fresh {
            if let Some(hash) = Self::get_latest_blockhash().await {
                self.logger.log("🚀 Using ultra-fresh cached blockhash (< 500ms old)".green().to_string());
                return Ok(hash);
            }
        }
        
        // Try waiting for real-time update
        if IS_REAL_TIME_ACTIVE.load(Ordering::Relaxed) {
            if let Some(hash) = Self::wait_for_fresh_blockhash(Duration::from_millis(100)).await {
                self.logger.log("⚡ Got ultra-fresh blockhash from real-time stream".cyan().to_string());
                return Ok(hash);
            }
        }
        
        // Fallback to immediate RPC
        self.get_immediate_blockhash().await
    }



    /// Get performance statistics
    pub async fn get_stats() -> BlockhashStats {
        let connection_status = Self::get_connection_status().await;
        let is_real_time = IS_REAL_TIME_ACTIVE.load(Ordering::Relaxed);
        let reconnect_attempts = RECONNECT_ATTEMPTS.load(Ordering::Relaxed);
        
        let (latest_hash, last_updated) = {
            let hash = LATEST_BLOCKHASH.read().await;
            let updated = BLOCKHASH_LAST_UPDATED.read().await;
            (*hash, *updated)
        };
        
        BlockhashStats {
            connection_status,
            is_real_time_active: is_real_time,
            latest_blockhash: latest_hash,
            last_updated,
            total_reconnect_attempts: reconnect_attempts,
        }
    }
}

#[derive(Debug)]
pub struct BlockhashStats {
    pub connection_status: ConnectionStatus,
    pub is_real_time_active: bool,
    pub latest_blockhash: Option<Hash>,
    pub last_updated: Option<Instant>,
    pub total_reconnect_attempts: u64,
} 