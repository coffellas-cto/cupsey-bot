use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use colored::Colorize;
use futures_util::stream::StreamExt;
use futures_util::{SinkExt, Sink};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
    SubscribeRequestFilterTransactions,  SubscribeUpdate,
};
use tokio::time;

use crate::common::{
    config::AppState,
    logger::Logger,
};
use crate::engine::transaction_parser;

/// Configuration for the second GRPC stream used for wallet monitoring
pub struct WalletMonitorConfig {
    pub second_yellowstone_grpc_http: String,
    pub second_yellowstone_grpc_token: String,
    pub wallet_pubkey: String,
}

impl WalletMonitorConfig {
    pub fn from_env() -> Result<Self, String> {
        let second_yellowstone_grpc_http = std::env::var("SECOND_YELLOWSTONE_GRPC_HTTP")
            .map_err(|_| "SECOND_YELLOWSTONE_GRPC_HTTP environment variable not set".to_string())?;
            
        let second_yellowstone_grpc_token = std::env::var("SECOND_YELLOWSTONE_GRPC_TOKEN")
            .map_err(|_| "SECOND_YELLOWSTONE_GRPC_TOKEN environment variable not set".to_string())?;
            
        // Get wallet pubkey from app state wallet
        let wallet_pubkey = std::env::var("WALLET_PUBKEY")
            .map_err(|_| "WALLET_PUBKEY environment variable not set for wallet monitoring".to_string())?;
            
        Ok(Self {
            second_yellowstone_grpc_http,
            second_yellowstone_grpc_token,
            wallet_pubkey,
        })
    }
}

pub struct WalletMonitor {
    config: WalletMonitorConfig,
    app_state: Arc<AppState>,
    logger: Logger,
}

impl WalletMonitor {
    pub fn new(config: WalletMonitorConfig, app_state: Arc<AppState>) -> Self {
        Self {
            config,
            app_state,
            logger: Logger::new("[WALLET-MONITOR] => ".purple().bold().to_string()),
        }
    }
    
    /// Start monitoring the bot's wallet for immediate transaction confirmation
    pub async fn start_monitoring(&self) -> Result<(), String> {
        self.logger.log("🔍 Starting wallet monitoring with second GRPC stream...".green().to_string());
        self.logger.log(format!("Monitoring wallet: {}", self.config.wallet_pubkey));
        
        // Connect to the second Yellowstone gRPC
        self.logger.log("Connecting to second Yellowstone gRPC endpoint...".green().to_string());
        let mut client = GeyserGrpcClient::build_from_shared(self.config.second_yellowstone_grpc_http.clone())
            .map_err(|e| format!("Failed to build second client: {}", e))?
            .x_token::<String>(Some(self.config.second_yellowstone_grpc_token.clone()))
            .map_err(|e| format!("Failed to set x_token: {}", e))?
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|e| format!("Failed to set tls config: {}", e))?
            .connect()
            .await
            .map_err(|e| format!("Failed to connect to second gRPC: {}", e))?;

        // Set up subscribe with retry logic
        let mut retry_count = 0;
        const MAX_RETRIES: u32 = 3;
        let (subscribe_tx, mut stream) = loop {
            match client.subscribe().await {
                Ok(pair) => break pair,
                Err(e) => {
                    retry_count += 1;
                    if retry_count >= MAX_RETRIES {
                        return Err(format!("Failed to subscribe to second gRPC after {} attempts: {}", MAX_RETRIES, e));
                    }
                    self.logger.log(format!(
                        "[WALLET-MONITOR-CONNECTION ERROR] => Failed to subscribe (attempt {}/{}): {}. Retrying in 5 seconds...",
                        retry_count, MAX_RETRIES, e
                    ).red().to_string());
                    time::sleep(Duration::from_secs(5)).await;
                }
            }
        };

        // Convert to Arc for sharing across tasks
        let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));

        // Set up subscription specifically for our wallet
        self.logger.log("Setting up wallet-specific subscription...".green().to_string());
        let subscription_request = SubscribeRequest {
            transactions: maplit::hashmap! {
                "WalletMonitor".to_owned() => SubscribeRequestFilterTransactions {
                    vote: Some(false), // Exclude vote transactions
                    failed: Some(false), // Exclude failed transactions
                    signature: None,
                    account_include: vec![self.config.wallet_pubkey.clone()], // Only monitor our wallet
                    account_exclude: vec![], // Don't exclude anything for wallet monitoring
                    account_required: Vec::<String>::new(),
                }
            },
            commitment: Some(CommitmentLevel::Processed as i32), // Use processed for faster confirmation
            ..Default::default()
        };
        
        subscribe_tx
            .lock()
            .await
            .send(subscription_request)
            .await
            .map_err(|e| format!("Failed to send wallet monitor subscribe request: {}", e))?;

        // Spawn heartbeat task for wallet monitor
        let subscribe_tx_clone = subscribe_tx.clone();
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(30));
            
            loop {
                interval.tick().await;
                if let Err(_e) = Self::send_heartbeat_ping(&subscribe_tx_clone).await {
                    break;
                }
            }
        });

        // Main wallet monitoring loop
        self.logger.log("Starting wallet transaction monitoring loop...".green().to_string());
        let monitor_result = loop {
            match stream.next().await {
                Some(msg_result) => {
                    match msg_result {
                        Ok(msg) => {
                            if let Err(e) = self.process_wallet_message(&msg).await {
                                self.logger.log(format!("Error processing wallet message: {}", e).red().to_string());
                            }
                        },
                        Err(e) => {
                            self.logger.log(format!("Wallet monitor stream error: {:?}", e).red().to_string());
                            break "stream_error";
                        },
                    }
                },
                None => {
                    self.logger.log("Wallet monitor stream ended".yellow().to_string());
                    break "stream_ended";
                }
            }
        };
        
        // Properly close the gRPC connection
        self.logger.log(format!("Closing wallet monitor gRPC connection (reason: {})", monitor_result).yellow().to_string());
        
        // Drop the subscription sender to close the subscription
        drop(subscribe_tx);
        
        // Drop the stream to close it
        drop(stream);
        
        // The client will be automatically dropped when the function ends, 
        // but we explicitly drop it here to ensure immediate cleanup
        drop(client);
        
        self.logger.log("✅ Successfully closed wallet monitor gRPC connection".green().to_string());
        
        // Return appropriate result
        match monitor_result {
            "stream_error" => {
                self.logger.log("Wallet monitor stream ended due to error - connection properly closed".yellow().to_string());
                Err("Wallet monitor stream error".to_string())
            },
            _ => {
                self.logger.log("Wallet monitor stream ended normally - connection properly closed".yellow().to_string());
                Ok(())
            }
        }
    }
    
    /// Process wallet-specific transaction messages
    async fn process_wallet_message(&self, msg: &SubscribeUpdate) -> Result<(), String> {
        // Handle ping messages
        if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
            return Ok(());
        }

        // Handle transaction messages for our wallet
        if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
            if let Some(transaction) = &txn.transaction {
                if let Some(signature_bytes) = &transaction.signature {
                    let signature = bs58::encode(signature_bytes).into_string();
                    
                    self.logger.log(format!("🔍 WALLET TRANSACTION DETECTED: {}", signature).cyan().to_string());
                    
                    // Check if this is a buy or sell transaction based on logs
                    if let Some(meta) = &transaction.meta {
                        let is_buy = meta.log_messages.iter().any(|log| {
                            log.contains("Program log: Instruction: Buy") || log.contains("MintTo")
                        });
                        
                        let is_sell = meta.log_messages.iter().any(|log| {
                            log.contains("Program log: Instruction: Sell")
                        });
                        
                        if is_buy {
                            self.logger.log(format!("✅ WALLET BUY CONFIRMED: {}", signature).green().to_string());
                            
                            // Extract token information from transaction
                            if let Some(token_info) = self.extract_token_info_from_wallet_transaction(txn).await {
                                self.logger.log(format!("🪙 Token: {} - Amount: {}", token_info.mint, token_info.token_amount_f64));
                                
                            }
                        }
                        
                        if is_sell {
                            self.logger.log(format!("💰 WALLET SELL CONFIRMED: {}", signature).yellow().to_string());
                            
                            // Extract token information from transaction
                            if let Some(token_info) = self.extract_token_info_from_wallet_transaction(txn).await {
                                self.logger.log(format!("🪙 Token: {} - Amount: {}", token_info.mint, token_info.token_amount_f64));
                                
                                // Remove from bought token list if this was a full sell
                                if let Some(_) = crate::engine::copy_trading::BOUGHT_TOKEN_LIST.remove(&token_info.mint) {
                                    self.logger.log(format!("Removed {} from bought token list after sell confirmation", token_info.mint));
                                }
                            }
                        }
                        
                        // Log all wallet transactions for debugging
                        self.logger.log(format!("📊 Wallet transaction details - SOL change: {}, Token changes: {}", 
                                               meta.pre_balances.iter().zip(meta.post_balances.iter())
                                                   .map(|(pre, post)| (*post as i64) - (*pre as i64))
                                                   .sum::<i64>() as f64 / 1_000_000_000.0,
                                               meta.pre_token_balances.len() + meta.post_token_balances.len()));
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Extract token information from wallet transaction
    async fn extract_token_info_from_wallet_transaction(
        &self,
        txn: &yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction,
    ) -> Option<transaction_parser::TradeInfoFromToken> {
        // Try to parse transaction data from inner instructions
        if let Some(tx_inner) = &txn.transaction {
            if let Some(meta) = &tx_inner.meta {
                // Look for inner instructions with data that can be parsed
                for inner in &meta.inner_instructions {
                    for ix in &inner.instructions {
                        if ix.data.len() >= 146 { // Minimum size for parseable transaction data
                            if let Some(trade_info) = transaction_parser::parse_transaction_data(txn, &ix.data) {
                                return Some(trade_info);
                            }
                        }
                    }
                }
                
                // If no parseable data found, create basic trade info from token balances
                if !meta.post_token_balances.is_empty() {
                    let first_token = &meta.post_token_balances[0];
                    return Some(transaction_parser::TradeInfoFromToken {
                        dex_type: transaction_parser::DexType::Unknown,
                        slot: 0,
                        signature: "wallet_monitor".to_string(),
                        target: self.config.wallet_pubkey.clone(),
                        mint: first_token.mint.clone(),
                        user: self.config.wallet_pubkey.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        is_buy: meta.log_messages.iter().any(|log| log.contains("MintTo")),
                        price: 0, // Will be calculated later
                        is_reverse_when_pump_swap: false,
                        // Set other fields to None/default
                        base_amount_in: None,
                        min_quote_amount_out: None,
                        user_base_token_reserves: None,
                        user_quote_token_reserves: None,
                        pool_base_token_reserves: None,
                        pool_quote_token_reserves: None,
                        quote_amount_out: None,
                        lp_fee_basis_points: None,
                        lp_fee: None,
                        protocol_fee_basis_points: None,
                        protocol_fee: None,
                        quote_amount_out_without_lp_fee: None,
                        user_quote_amount_out: None,
                        pool: None,
                        user_base_token_account: None,
                        user_quote_token_account: None,
                        protocol_fee_recipient: None,
                        protocol_fee_recipient_token_account: None,
                        coin_creator: None,
                        coin_creator_fee_basis_points: None,
                        coin_creator_fee: None,
                        sol_amount: None,
                        token_amount: None,
                        virtual_sol_reserves: None,
                        virtual_token_reserves: None,
                        real_sol_reserves: None,
                        real_token_reserves: None,
                        bonding_curve: String::new(),
                        volume_change: 0,
                        bonding_curve_info: None,
                        pool_info: None,
                        token_amount_f64: first_token.ui_token_amount.ui_amount.unwrap_or(0.0),
                        amount: None,
                        max_sol_cost: None,
                        min_sol_output: None,
                        base_amount_out: None,
                        max_quote_amount_in: None,
                    });
                }
            }
        }
        
        None
    }
    
    /// Send heartbeat ping to maintain connection
    async fn send_heartbeat_ping(
        subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
    ) -> Result<(), String> {
        let ping_request = SubscribeRequest {
            ping: Some(SubscribeRequestPing { id: 1 }),
            ..Default::default()
        };
        
        let mut tx = subscribe_tx.lock().await;
        match tx.send(ping_request).await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Failed to send wallet monitor ping: {:?}", e)),
        }
    }
}

/// Start wallet monitoring in the background
pub async fn start_wallet_monitoring(app_state: Arc<AppState>) -> Result<(), String> {
    let config = WalletMonitorConfig::from_env()?;
    let monitor = WalletMonitor::new(config, app_state);
    
    // Start monitoring in a background task
    tokio::spawn(async move {
        loop {
            if let Err(e) = monitor.start_monitoring().await {
                eprintln!("Wallet monitor error: {}. Retrying in 10 seconds...", e);
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    });
    
    Ok(())
} 