use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use crate::common::config::import_env_var;
use crate::engine::selling_strategy::{SellingEngine, SellingConfig};
use anyhow::Result;
use anchor_client::solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_sdk::signature::Signer;
use spl_associated_token_account::get_associated_token_address;
use colored::Colorize;
use tokio::time;
use tokio::time::sleep;
use futures_util::stream::StreamExt;
use futures_util::{SinkExt, Sink};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
    SubscribeRequestFilterTransactions,  SubscribeUpdate,
};
use solana_transaction_status::TransactionConfirmationStatus;
use crate::engine::transaction_parser;
use crate::common::{
    config::{AppState, SwapConfig, JUPITER_PROGRAM, OKX_DEX_PROGRAM},
    logger::Logger,
    cache::WALLET_TOKEN_ACCOUNTS,
};
use crate::engine::swap::{SwapDirection, SwapProtocol};

use tokio_util::sync::CancellationToken;
use dashmap::DashMap;

// Enum for different selling actions
#[derive(Debug, Clone)]
pub enum SellingAction {
    Hold,
    SellAll(String), // Reason for selling all
}

// Data structure for tracking bought tokens with comprehensive selling logic
#[derive(Clone, Debug)]
pub struct BoughtTokenInfo {
    pub token_mint: String,
    pub entry_price: u64,
    pub current_price: u64,
    pub highest_price: u64,
    pub lowest_price_after_highest: u64,
    pub initial_amount: f64, // Amount of SOL initially spent
    pub current_amount: f64, // Current token amount held
    pub buy_timestamp: Instant,
    pub protocol: SwapProtocol,
    pub trade_info: transaction_parser::TradeInfoFromToken,
    pub pnl_percentage: f64,
    pub highest_pnl_percentage: f64,
    pub trailing_stop_percentage: f64,
    pub selling_time_seconds: u64, // SELLING_TIME in seconds
    pub last_price_update: Instant,
    pub first_20_percent_reached_time: Option<Instant>, // When 20% PnL was first reached
}

impl BoughtTokenInfo {
    pub fn new(
        token_mint: String,
        entry_price: u64,
        initial_amount: f64,
        current_amount: f64,
        protocol: SwapProtocol,
        trade_info: transaction_parser::TradeInfoFromToken,
        selling_time_seconds: u64,
    ) -> Self {
        Self {
            token_mint,
            entry_price,
            current_price: entry_price,
            highest_price: entry_price,
            lowest_price_after_highest: entry_price,
            initial_amount,
            current_amount,
            buy_timestamp: Instant::now(),
            protocol,
            trade_info,
            pnl_percentage: 0.0,
            highest_pnl_percentage: 0.0,
            trailing_stop_percentage: 1.0, // Start with 1% trailing stop
            selling_time_seconds,
            last_price_update: Instant::now(),
            first_20_percent_reached_time: None,
        }
    }

    pub fn update_price(&mut self, new_price: u64) {
        self.current_price = new_price;
        self.last_price_update = Instant::now();
        
        // Update highest price
        if new_price > self.highest_price {
            self.highest_price = new_price;
            self.lowest_price_after_highest = new_price; // Reset lowest after new high
        } else if new_price < self.lowest_price_after_highest {
            self.lowest_price_after_highest = new_price;
        }
        
        // Calculate PnL percentage - prevent division by zero
        self.pnl_percentage = if self.entry_price > 0 {
            ((new_price as f64 - self.entry_price as f64) / self.entry_price as f64) * 100.0
        } else {
            0.0 // No PnL calculation if entry_price is not set
        };
        
        // Debug logging for price calculations
        if self.pnl_percentage.abs() > 1000.0 { // Log if PnL is unusually high
            println!("DEBUG PRICE: Token {} - Entry: {}, Current: {}, PnL: {:.2}%", 
                self.token_mint, self.entry_price, new_price, self.pnl_percentage);
        }
        
        // Update highest PnL
        if self.pnl_percentage > self.highest_pnl_percentage {
            self.highest_pnl_percentage = self.pnl_percentage;
        }
        
        // Check if 20% PnL is reached for the first time and within 1.5 seconds
        if self.pnl_percentage >= 20.0 && self.first_20_percent_reached_time.is_none() {
            let time_since_buy = self.buy_timestamp.elapsed().as_millis();
            if time_since_buy <= 1500 { // 1.5 seconds
                self.first_20_percent_reached_time = Some(Instant::now());
            }
        }
        
        // Update trailing stop based on PnL
        self.trailing_stop_percentage = self.calculate_dynamic_trailing_stop();
    }
    
    fn calculate_dynamic_trailing_stop(&self) -> f64 {
        match self.highest_pnl_percentage {
            pnl if pnl <= 5.0 => 1.0,     // 1% trailing stop
            pnl if pnl <= 20.0 => 5.0,    // 5% trailing stop
            pnl if pnl <= 50.0 => 10.0,   // 10% trailing stop
            pnl if pnl <= 100.0 => 30.0,  // 30% trailing stop
            pnl if pnl <= 500.0 => 100.0, // 100% trailing stop
            _ => 100.0,                    // 100% trailing stop for extreme gains
        }
    }
    
    /// Legacy method - use get_selling_action() instead
    /// DEPRECATED: This method contained bad logic and should not be used
    pub fn should_sell_all_due_to_time(&self) -> bool {
        // This method is deprecated and always returns false
        // Use get_selling_action() instead which has proper logic
        false
    }
    
    pub fn should_sell_due_to_trailing_stop(&self) -> bool {
        // Don't trigger trailing stop if entry_price is not set (buy not processed yet)
        if self.entry_price == 0 || self.highest_price == 0 {
            return false;
        }
        
        let drop_from_highest = ((self.highest_price as f64 - self.current_price as f64) / self.highest_price as f64) * 100.0;
        drop_from_highest >= self.trailing_stop_percentage
    }
    
    /// Determine selling action based on comprehensive rules
    pub fn get_selling_action(&self) -> SellingAction {
        // CRITICAL: Don't sell if entry_price is 0 (buy transaction not yet processed)
        if self.entry_price == 0 {
            return SellingAction::Hold;
        }
        
        let time_since_buy = self.buy_timestamp.elapsed().as_secs();
        
        // Rule 1: Stop Loss - sell if loss exceeds 10%
        if self.pnl_percentage <= -10.0 {
            return SellingAction::SellAll(format!("Stop loss triggered: {:.2}% loss", self.pnl_percentage));
        }
        
        // Rule 2: Take Profit - sell if profit exceeds 100%
        if self.pnl_percentage >= 100.0 {
            return SellingAction::SellAll(format!("Take profit triggered: {:.2}% profit", self.pnl_percentage));
        }
        
        // Rule 3: Maximum hold time - sell after 24 hours (86400 seconds)
        if time_since_buy >= 86400 {
            return SellingAction::SellAll(format!("Max hold time reached: {} hours", time_since_buy / 3600));
        }
        
        // Rule 4: Trailing Stop Logic
        if self.should_sell_due_to_trailing_stop() {
            return SellingAction::SellAll(format!(
                "Trailing stop triggered ({}% drop from high)",
                self.trailing_stop_percentage
            ));
        }
        
        SellingAction::Hold
    }
    

    
    /// Check trailing stop with specific percentage
    fn should_sell_due_to_trailing_stop_with_percentage(&self, trailing_stop_percentage: f64) -> bool {
        // Don't trigger trailing stop if entry_price is not set (buy not processed yet)
        if self.entry_price == 0 || self.highest_price == 0 {
            return false;
        }
        
        let drop_from_highest = ((self.highest_price as f64 - self.current_price as f64) / self.highest_price as f64) * 100.0;
        drop_from_highest >= trailing_stop_percentage
    }
    

}

// Global state for copy trading
lazy_static::lazy_static! {
    static ref COUNTER: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref SOLD_TOKENS: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref BOUGHT_TOKENS: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref LAST_BUY_TIME: Arc<DashMap<(), Option<Instant>>> = Arc::new(DashMap::new());
    static ref BUYING_ENABLED: Arc<DashMap<(), bool>> = Arc::new(DashMap::new());
    static ref TOKEN_TRACKING: Arc<DashMap<String, TokenTrackingInfo>> = Arc::new(DashMap::new());
    // Global registry for monitoring task cancellation tokens
    static ref MONITORING_TASKS: Arc<DashMap<String, CancellationToken>> = Arc::new(DashMap::new());
    // New: Bought token list for comprehensive tracking
    static ref BOUGHT_TOKEN_LIST: Arc<DashMap<String, BoughtTokenInfo>> = Arc::new(DashMap::new());
}

// Initialize the global counters with default values
fn init_global_state() {
    COUNTER.insert((), 0);
    SOLD_TOKENS.insert((), 0);
    BOUGHT_TOKENS.insert((), 0);
    LAST_BUY_TIME.insert((), None);
    BUYING_ENABLED.insert((), true);
}

// Track token performance for selling strategies
#[derive(Clone, Debug)]
pub struct TokenTrackingInfo {
    pub top_pnl: f64,
    pub last_sell_time: Instant,
    pub completed_intervals: HashSet<String>,
}

/// Configuration for copy trading
pub struct CopyTradingConfig {
    pub yellowstone_grpc_http: String,
    pub yellowstone_grpc_token: String,
    pub app_state: AppState,
    pub swap_config: SwapConfig,
    pub counter_limit: u64,
    pub target_addresses: Vec<String>,
    pub excluded_addresses: Vec<String>,
    pub protocol_preference: SwapProtocol,
}

/// Helper to send heartbeat pings to maintain connection
async fn send_heartbeat_ping(
    subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
) -> Result<(), String> {
    let ping_request = SubscribeRequest {
        ping: Some(SubscribeRequestPing { id: 0 }),
        ..Default::default()
    };
    
    let mut tx = subscribe_tx.lock().await;
    match tx.send(ping_request).await {
        Ok(_) => {
            Ok(())
        },
        Err(e) => Err(format!("Failed to send ping: {:?}", e)),
    }
}

/// Cancel monitoring task for a sold token and clean up tracking
async fn cancel_token_monitoring(token_mint: &str, logger: &Logger) -> Result<(), String> {
    logger.log(format!("Cancelling monitoring for sold token: {}", token_mint));
    
    // Cancel the monitoring task
    if let Some(entry) = MONITORING_TASKS.remove(token_mint) {
        entry.1.cancel();
        logger.log(format!("Cancelled monitoring task for token: {}", token_mint));
    } else {
        logger.log(format!("No monitoring task found for token: {}", token_mint).yellow().to_string());
    }
    
    // Remove from token tracking
    if TOKEN_TRACKING.remove(token_mint).is_some() {
        logger.log(format!("Removed token from tracking: {}", token_mint));
    }
    
    Ok(())
}

/// Main function to start copy trading
pub async fn start_copy_trading(config: CopyTradingConfig) -> Result<(), String> {
    let logger = Logger::new("[COPY-TRADING] => ".green().bold().to_string());
    
    // Initialize global state
    init_global_state();
    // Initialize
    logger.log("Initializing copy trading bot...".green().to_string());
    logger.log(format!("Target addresses: {:?}", config.target_addresses));
    logger.log(format!("Protocol preference: {:?}", config.protocol_preference));
    
    // Start wallet monitoring with second GRPC stream (if configured)
    if let (Ok(second_grpc_http), Ok(_second_grpc_token)) = (
        std::env::var("SECOND_YELLOWSTONE_GRPC_HTTP"),
        std::env::var("SECOND_YELLOWSTONE_GRPC_TOKEN")
    ) {
        logger.log("🔍 Starting wallet monitoring with second GRPC stream...".purple().to_string());
        let app_state_clone = config.app_state.clone();
        tokio::spawn(async move {
            // Create wallet monitor configuration
            let wallet_pubkey = app_state_clone.wallet.pubkey().to_string();
            
            // Set wallet pubkey environment variable for wallet monitor
            std::env::set_var("WALLET_PUBKEY", wallet_pubkey);
            
            // Start wallet monitoring loop with reconnection
            loop {
                if let Err(e) = start_wallet_monitoring_internal(Arc::new(app_state_clone.clone())).await {
                    eprintln!("Wallet monitor error: {}. Retrying in 10 seconds...", e);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        });
         } else {
         logger.log("⚠️ Second GRPC not configured, wallet monitoring disabled".yellow().to_string());
     }
     
     // Start enhanced selling monitor
     logger.log("🚀 Starting enhanced selling monitor...".cyan().to_string());
     let app_state_clone = Arc::new(config.app_state.clone());
     let swap_config_clone = Arc::new(config.swap_config.clone());
     tokio::spawn(async move {
         start_enhanced_selling_monitor(app_state_clone, swap_config_clone).await;
     });
    
    // Connect to Yellowstone gRPC
    logger.log("Connecting to Yellowstone gRPC...".green().to_string());
    let mut client = GeyserGrpcClient::build_from_shared(config.yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed to build client: {}", e))?
        .x_token::<String>(Some(config.yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed to set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed to set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;

    // Set up subscribe
    let mut retry_count = 0;
    const MAX_RETRIES: u32 = 3;
    let (subscribe_tx, mut stream) = loop {
        match client.subscribe().await {
            Ok(pair) => break pair,
            Err(e) => {
                retry_count += 1;
                if retry_count >= MAX_RETRIES {
                    return Err(format!("Failed to subscribe after {} attempts: {}", MAX_RETRIES, e));
                }
                logger.log(format!(
                    "[CONNECTION ERROR] => Failed to subscribe (attempt {}/{}): {}. Retrying in 5 seconds...",
                    retry_count, MAX_RETRIES, e
                ).red().to_string());
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    // Convert to Arc to allow cloning across tasks
    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));
    // Enable buying
    BUYING_ENABLED.insert((), true);

    // Create config for subscription
    let target_addresses = config.target_addresses.clone();
    // Add excluded addresses : this can be : 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8,CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C
    let mut excluded_addresses = vec![JUPITER_PROGRAM.to_string(), OKX_DEX_PROGRAM.to_string()];
    excluded_addresses.extend(config.excluded_addresses.clone());
    // Set up subscription
    logger.log("Setting up subscription...".green().to_string());
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "All".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false), // Exclude vote transactions
                failed: Some(false), // Exclude failed transactions
                signature: None,
                account_include: target_addresses.clone(), // Only include transactions involving our targets
                account_exclude: excluded_addresses, // Exclude some common programs
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    
    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed to send subscribe request: {}", e))?;
    
    // Create Arc config for tasks
    let config = Arc::new(config);

    // Spawn heartbeat task
    let subscribe_tx_clone = subscribe_tx.clone();
    
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            if let Err(_e) = send_heartbeat_ping(&subscribe_tx_clone).await {
                break;
            }
        }
    });
    
    // Main stream processing loop
    logger.log("Starting main processing loop...".green().to_string());
    let stream_result = loop {
        match stream.next().await {
            Some(msg_result) => {
                match msg_result {
                    Ok(msg) => {
                        if let Err(e) = process_message(&msg, &subscribe_tx, config.clone(), &logger).await {
                            logger.log(format!("Error processing message: {}", e).red().to_string());
                        }
                    },
                    Err(e) => {
                        logger.log(format!("Stream error: {:?}", e).red().to_string());
                        break "stream_error";
                    },
                }
            },
            None => {
                logger.log("Main stream ended".yellow().to_string());
                break "stream_ended";
            }
        }
    };
    
    // Properly close the main gRPC connection
    logger.log(format!("Closing main copy trading gRPC connection (reason: {})", stream_result).yellow().to_string());
    
    // Drop the subscription sender to close the subscription
    drop(subscribe_tx);
    
    // Drop the stream to close it
    drop(stream);
    
    // The client will be automatically dropped when the function ends, 
    // but we explicitly drop it here to ensure immediate cleanup
    drop(client);
    
    logger.log("✅ Successfully closed main copy trading gRPC connection".green().to_string());
    
    // Return appropriate result
    match stream_result {
        "stream_error" => {
            logger.log("Main stream ended due to error - connection properly closed, will attempt reconnect if needed".yellow().to_string());
            Err("Main stream error".to_string())
        },
        _ => {
            logger.log("Main stream ended normally - connection properly closed".yellow().to_string());
            Ok(())
        }
    }
}

/// Verify that a transaction was successful
async fn verify_transaction(
    signature_str: &str,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<bool, String> {
    // Parse signature
    let signature = match Signature::from_str(signature_str) {
        Ok(sig) => sig,
        Err(e) => return Err(format!("Invalid signature: {}", e)),
    };
    
    // Verify transaction success with retries
    let max_retries = 5;
    for retry in 0..max_retries {
        // Check transaction status
        match app_state.rpc_nonblocking_client.get_signature_statuses(&[signature]).await {
            Ok(result) => {
                if let Some(status_opt) = result.value.get(0) {
                    if let Some(status) = status_opt {
                        if status.err.is_some() {
                            // Transaction failed
                            return Err(format!("Transaction failed: {:?}", status.err));
                        } else if let Some(conf_status) = &status.confirmation_status {
                            if matches!(conf_status, TransactionConfirmationStatus::Finalized | 
                                                      TransactionConfirmationStatus::Confirmed) {
                                return Ok(true);
                            } else {
                                logger.log(format!("Transaction not yet confirmed (status: {:?}), retrying...", 
                                         conf_status).yellow().to_string());
                            }
                        } else {
                        }
                    } else {
                    }
                }
            },
            Err(e) => {
                logger.log(format!("Failed to get transaction status: {}, retrying...", e).red().to_string());
            }
        }
        
        if retry < max_retries - 1 {
            // Wait before retrying
            sleep(Duration::from_millis(500)).await;
        } else {
            return Err("Transaction verification timed out".to_string());
        }
    }
    
    // If we get here, verification failed
    Err("Transaction verification failed after retries".to_string())
}

/// Execute buy operation based on detected transaction
pub async fn execute_buy(
    trade_info: transaction_parser::TradeInfoFromToken,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    protocol: SwapProtocol,
) -> Result<(), String> {
    let logger = Logger::new("[EXECUTE-BUY] => ".green().to_string());
    let start_time = Instant::now();
    
    // Create a modified swap config based on the trade_info
    let mut buy_config = (*swap_config).clone();
    buy_config.swap_direction = SwapDirection::Buy;
    
    // Store the amount_in before potential moves
    let amount_in = buy_config.amount_in;
    
    // Get token amount and SOL cost from trade_info
    let (_amount_in, _token_amount) = match trade_info.dex_type {
        transaction_parser::DexType::PumpSwap => {
            let sol_amount = trade_info.sol_change.abs();
            let token_amount = trade_info.token_change.abs();
            (sol_amount, token_amount)
        },
        transaction_parser::DexType::PumpFun => {
            let sol_amount = trade_info.sol_change.abs();
            let token_amount = trade_info.token_change.abs();
            (sol_amount, token_amount)
        },
        transaction_parser::DexType::RaydiumLaunchpad => {
            let sol_amount = trade_info.sol_change.abs();
            let token_amount = trade_info.token_change.abs();
            (sol_amount, token_amount)
        },
        _ => {
            return Err("Unsupported transaction type".to_string());
        }
    };
    
    // Protocol string for notifications
    let _protocol_str = match protocol {
        SwapProtocol::PumpSwap => "PumpSwap",
        SwapProtocol::PumpFun => "PumpFun",
        SwapProtocol::RaydiumLaunchpad => "RaydiumLaunchpad",
        _ => "Unknown",
    };
    
    // Send notification that we're attempting to copy the trade
    
    // Execute based on protocol
    let result = match protocol {
        SwapProtocol::PumpFun => {
            logger.log("Using PumpFun protocol for buy".to_string());
            
            // Create the PumpFun instance
            let pump = crate::dex::pump_fun::Pump::new(
                app_state.rpc_nonblocking_client.clone(),
                app_state.rpc_client.clone(),
                app_state.wallet.clone(),
            );
            // Build swap instructions from parsed data
            match pump.build_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    logger.log(format!("Generated PumpFun buy instruction at price: {}", price));
                    logger.log(format!("copy transaction {}", trade_info.signature));
                    let start_time = Instant::now();
                    // Get real-time blockhash from processor
                    let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                        Some(hash) => hash,
                        None => {
                            logger.log("Failed to get real-time blockhash, skipping transaction".red().to_string());
                            return Err("Failed to get real-time blockhash".to_string());
                        }
                    };
                    println!("time taken for get_latest_blockhash: {:?}", start_time.elapsed());
                    println!("using zeroslot for buy transaction >>>>>>>>");
                    // Execute the transaction using zeroslot for buying
                    match crate::core::tx::new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            logger.log(format!("Buy transaction sent: {}", signature));
                            
                            
                            // Verify transaction
                            match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                Ok(verified) => {
                                    if verified {
                                        logger.log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account to our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.insert(token_ata);
                                            logger.log(format!("Added token account {} to global list", token_ata));
                                            
                                            // Add to enhanced tracking system for PumpFun
                                            let bought_token_info = BoughtTokenInfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                protocol.clone(),
                                                trade_info.clone(),
                                                3, // 3 seconds selling time
                                            );
                                            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
                                            logger.log(format!("Added {} to enhanced tracking system (PumpFun)", trade_info.mint));
                                        }
                                        
                                        
                                        Ok(())
                                    } else {
                                        
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed to build PumpFun buy instruction: {}", e))
                }
            }
        },
        SwapProtocol::PumpSwap => {
            logger.log("Using PumpSwap protocol for buy".to_string());
            
            // Create the PumpSwap instance
            let pump_swap = crate::dex::pump_swap::PumpSwap::new(
                app_state.wallet.clone(),
                Some(app_state.rpc_client.clone()),
                Some(app_state.rpc_nonblocking_client.clone()),
            );
            
            // Build swap instructions from parsed data for buy
            match pump_swap.build_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    logger.log(format!("Generated PumpSwap buy instruction at price: {}", price));
                    logger.log(format!("copy transaction {}", trade_info.signature));
                    
                    // Get real-time blockhash from processor
                    let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                        Some(hash) => hash,
                        None => {
                            logger.log("Failed to get real-time blockhash, skipping transaction".red().to_string());
                            return Err("Failed to get real-time blockhash".to_string());
                        }
                    };

                    println!("using zeroslot for buy transaction >>>>>>>>");
                    // Execute the transaction using zeroslot for buying
                    match crate::core::tx::new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            logger.log(format!("Buy transaction sent: {}", signature));
                            
                            // Verify transaction
                            match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                Ok(verified) => {
                                    if verified {
                                        logger.log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account to our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.insert(token_ata);
                                            logger.log(format!("Added token account {} to global list", token_ata));
                                            
                                            // Add to enhanced tracking system for PumpSwap
                                            let bought_token_info = BoughtTokenInfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                protocol.clone(),
                                                trade_info.clone(),
                                                3, // 3 seconds selling time
                                            );
                                            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
                                            logger.log(format!("Added {} to enhanced tracking system (PumpSwap)", trade_info.mint));
                                        }
                                        
                                        Ok(())
                                    } else {
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed to build PumpSwap buy instruction: {}", e))
                },
            }
        },
                    SwapProtocol::RaydiumLaunchpad => {
                logger.log("Using RaydiumLaunchpad protocol for buy".to_string());
                
                // Create the Raydium instance
                let raydium = crate::dex::raydium_launchpad::Raydium::new(
                app_state.wallet.clone(),
                Some(app_state.rpc_client.clone()),
                Some(app_state.rpc_nonblocking_client.clone()),
            );
            
            // Build swap instructions from parsed data for buy
            match raydium.build_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    
                    // Get real-time blockhash from processor
                    let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                        Some(hash) => hash,
                        None => {
                            logger.log("Failed to get real-time blockhash, skipping transaction".red().to_string());
                            return Err("Failed to get real-time blockhash".to_string());
                        }
                    };
                    
                    // Execute the transaction using zeroslot for buying
                    match crate::core::tx::new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            logger.log(format!("Buy transaction sent: {}", signature));
                            
                            // Verify transaction
                            match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                Ok(verified) => {
                                    if verified {
                                        logger.log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account to our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.insert(token_ata);
                                            logger.log(format!("Added token account {} to global list", token_ata));
                                            
                                            // Add to enhanced tracking system for Raydium
                                            let bought_token_info = BoughtTokenInfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                protocol.clone(),
                                                trade_info.clone(),
                                                3, // 3 seconds selling time
                                            );
                                            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
                                            logger.log(format!("Added {} to enhanced tracking system (Raydium)", trade_info.mint));
                                        }
                                        
                                        Ok(())
                                    } else {
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed to build RaydiumLaunchpad buy instruction: {}", e))
                },
            }
        },
        SwapProtocol::Auto | SwapProtocol::Unknown => {
            logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for buy".yellow().to_string());
            
            // Create the PumpFun instance
            let pump = crate::dex::pump_fun::Pump::new(
                app_state.rpc_nonblocking_client.clone(),
                app_state.rpc_client.clone(),
                app_state.wallet.clone(),
            );
            // Build swap instructions from parsed data
            match pump.build_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    logger.log(format!("Generated PumpFun buy instruction at price: {}", price));
                    logger.log(format!("copy transaction {}", trade_info.signature));
                    let start_time = Instant::now();
                    // Get real-time blockhash from processor
                    let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                        Some(hash) => hash,
                        None => {
                            logger.log("Failed to get real-time blockhash, skipping transaction".red().to_string());
                            return Err("Failed to get real-time blockhash".to_string());
                        }
                    };
                    println!("time taken for get_latest_blockhash: {:?}", start_time.elapsed());
                    println!("using zeroslot for buy transaction >>>>>>>>");
                    // Execute the transaction using zeroslot for buying
                    match crate::core::tx::new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            logger.log(format!("Buy transaction sent: {}", signature));
                            
                            // Verify transaction
                            match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                Ok(verified) => {
                                    if verified {
                                        logger.log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account to our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.insert(token_ata);
                                            logger.log(format!("Added token account {} to global list", token_ata));
                                            
                                            // Add to enhanced tracking system for PumpFun
                                            let bought_token_info = BoughtTokenInfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                SwapProtocol::PumpFun, // Use PumpFun as the fallback protocol
                                                trade_info.clone(),
                                                import_env_var("SELLING_TIME").parse::<u64>().unwrap_or(600),
                                            );
                                            
                                            // Insert the token into the bought tokens map for monitoring
                                            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
                                            logger.log(format!("Added {} to bought tokens tracking", trade_info.mint));
                                            
                                            // Start enhanced selling monitor for this token
                                            let app_state_clone = app_state.clone();
                                            let swap_config_clone = swap_config.clone();
                                            let token_mint = trade_info.mint.clone();
                                            tokio::spawn(async move {
                                                start_enhanced_selling_monitor(app_state_clone, swap_config_clone).await;
                                            });
                                        }
                                        
                                        Ok(())
                                    } else {
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed to build PumpFun buy instruction: {}", e))
                },
            }
        },
    };
    
    // Log execution time
    let elapsed = start_time.elapsed();
    logger.log(format!("Buy execution time: {:?}", elapsed));
    
    // Increment bought counter on success
    if result.is_ok() {
        // Update counters and tracking
        let bought_count = {
            let mut entry = BOUGHT_TOKENS.entry(()).or_insert(0);
            *entry += 1;
            *entry
        };
        logger.log(format!("Total bought: {}", bought_count));
        
        // Add token to bought token list for comprehensive tracking
        let bought_token_info = BoughtTokenInfo::new(
            trade_info.mint.clone(),
            trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
            amount_in, // SOL amount spent (using stored value)
            trade_info.token_change.abs(), // Token amount received
            protocol.clone(),
            trade_info.clone(),
            std::env::var("SELLING_TIME").unwrap_or_else(|_| "300".to_string()).parse().unwrap_or(300),
        );
        
        // Debug logging for token tracking
        println!("DEBUG TRACKING: Adding token {} to BOUGHT_TOKEN_LIST with entry_price: {}", 
            trade_info.mint, bought_token_info.entry_price);
        
        // Only add to tracking if entry_price is valid
        if bought_token_info.entry_price > 0 {
            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
        } else {
            println!("WARNING: Refusing to track token {} with entry_price = 0", trade_info.mint);
        }
        
        // Token added to selling system via the selling_engine.update_metrics call above
        
        // Legacy tracking for compatibility
        TOKEN_TRACKING.entry(trade_info.mint.clone()).or_insert(TokenTrackingInfo {
            top_pnl: 0.0,
            last_sell_time: Instant::now(),
            completed_intervals: HashSet::new(),
        });
        
        // Get active tokens list
        let _active_tokens: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
        let _sold_count = SOLD_TOKENS.get(&()).map(|r| *r).unwrap_or(0);
        
    }
    
    result
}

/// Internal wallet monitoring function using second GRPC stream
async fn start_wallet_monitoring_internal(app_state: Arc<AppState>) -> Result<(), String> {
    use futures_util::stream::StreamExt;
    use futures_util::{SinkExt, Sink};
    use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
    use yellowstone_grpc_proto::geyser::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
        SubscribeRequestFilterTransactions, SubscribeUpdate,
    };
    
    let logger = Logger::new("[WALLET-MONITOR-INTERNAL] => ".purple().bold().to_string());
    
    let second_grpc_http = std::env::var("SECOND_YELLOWSTONE_GRPC_HTTP")
        .map_err(|_| "SECOND_YELLOWSTONE_GRPC_HTTP not set".to_string())?;
    let second_grpc_token = std::env::var("SECOND_YELLOWSTONE_GRPC_TOKEN")
        .map_err(|_| "SECOND_YELLOWSTONE_GRPC_TOKEN not set".to_string())?;
    let wallet_pubkey = app_state.wallet.pubkey().to_string();
    
    logger.log(format!("🔍 Connecting to second GRPC for wallet monitoring: {}", wallet_pubkey));
    
    // Connect to second Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(second_grpc_http)
        .map_err(|e| format!("Failed to build second client: {}", e))?
        .x_token::<String>(Some(second_grpc_token))
        .map_err(|e| format!("Failed to set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed to set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed to connect to second gRPC: {}", e))?;

    // Set up subscribe
    let (subscribe_tx, mut stream) = client.subscribe().await
        .map_err(|e| format!("Failed to subscribe to second gRPC: {}", e))?;
    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));

    // Set up wallet-specific subscription
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "WalletMonitor".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: vec![wallet_pubkey.clone()],
                account_exclude: vec![],
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    
    subscribe_tx.lock().await.send(subscription_request).await
        .map_err(|e| format!("Failed to send wallet subscription: {}", e))?;

    // Process wallet transactions
    let monitor_result = loop {
        match stream.next().await {
            Some(msg_result) => {
                match msg_result {
                    Ok(msg) => {
                        if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
                            if let Some(transaction) = &txn.transaction {
                                if let signature_bytes = &transaction.signature {
                                    let signature = bs58::encode(signature_bytes).into_string();
                                    
                                    if let Some(meta) = &transaction.meta {
                                        let is_buy = meta.log_messages.iter().any(|log| {
                                            log.contains("Program log: Instruction: Buy") || log.contains("MintTo")
                                        });
                                        
                                        let is_sell = meta.log_messages.iter().any(|log| {
                                            log.contains("Program log: Instruction: Sell")
                                        });
                                        
                                        if is_buy {
                                            logger.log(format!("✅ WALLET BUY CONFIRMED: {}", signature).green().to_string());
                                        }
                                        
                                        if is_sell {
                                            logger.log(format!("💰 WALLET SELL CONFIRMED: {}", signature).yellow().to_string());
                                            
                                            // Try to extract token mint and remove from bought list
                                            for token_balance in &meta.post_token_balances {
                                                if token_balance.ui_token_amount.as_ref().map(|ui| ui.ui_amount).unwrap_or(0.0) == 0.0 {
                                                    // This token was sold completely
                                                    BOUGHT_TOKEN_LIST.remove(&token_balance.mint);
                                                    // Remove token from the global tracking system
                                                    crate::engine::selling_strategy::TOKEN_METRICS.remove(&token_balance.mint);
                                                    crate::engine::selling_strategy::TOKEN_TRACKING.remove(&token_balance.mint);
                                                    logger.log(format!("Removed {} from tracking after confirmed sell", token_balance.mint));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    Err(e) => {
                        logger.log(format!("Wallet monitor stream error: {:?}", e).red().to_string());
                        break "stream_error";
                    },
                }
            },
            None => {
                logger.log("Wallet monitor stream ended".yellow().to_string());
                break "stream_ended";
            }
        }
    };
    
    // Properly close the gRPC connection
    logger.log(format!("Closing wallet monitor gRPC connection (reason: {})", monitor_result).yellow().to_string());
    
    // Drop the subscription sender to close the subscription
    drop(subscribe_tx);
    
    // Drop the stream to close it
    drop(stream);
    
    // The client will be automatically dropped when the function ends, 
    // but we explicitly drop it here to ensure immediate cleanup
    drop(client);
    
    logger.log("✅ Successfully closed wallet monitor gRPC connection".green().to_string());
    
    // Return error if stream errored, otherwise Ok
    match monitor_result {
        "stream_error" => Err("Wallet monitor stream error".to_string()),
        _ => Ok(())
    }
}


/// Execute whale emergency sell using zeroslot for maximum speed
async fn execute_whale_emergency_sell(
    token_mint: &str,
    token_info: &mut BoughtTokenInfo,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    logger: &Logger,
) -> Result<(), String> {
    logger.log(format!("🐋 Executing whale emergency sell for token: {}", token_mint).red().bold().to_string());
    
    // Calculate amount to sell (sell all remaining tokens)
    let amount_to_sell = token_info.current_amount;
    
    if amount_to_sell <= 0.0 {
        return Err("No tokens to sell".to_string());
    }
    
    // Create emergency sell config with higher slippage tolerance
    let mut emergency_config = (*swap_config).clone();
    emergency_config.swap_direction = SwapDirection::Sell;
    emergency_config.amount_in = amount_to_sell;
    emergency_config.slippage = 1000; // 10% slippage for emergency whale sells
    
    // Create trade info for emergency sell
    let trade_info = create_sell_trade_info_from_original(token_mint, amount_to_sell, &token_info.trade_info);
    
    let result = match token_info.protocol {
        SwapProtocol::PumpFun => {
            execute_pumpfun_emergency_sell_with_zeroslot(&trade_info, emergency_config, app_state.clone(), &logger).await
        },
        SwapProtocol::PumpSwap => {
            execute_pumpswap_emergency_sell_with_zeroslot(&trade_info, emergency_config, app_state.clone(), &logger).await
        },
        SwapProtocol::RaydiumLaunchpad => {
            execute_raydium_emergency_sell_with_zeroslot(&trade_info, emergency_config, app_state.clone(), &logger).await
        },
        SwapProtocol::Auto | SwapProtocol::Unknown => {
            logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for whale emergency sell".yellow().to_string());
            execute_pumpfun_emergency_sell_with_zeroslot(&trade_info, emergency_config, app_state.clone(), &logger).await
        },
    };
    
    if result.is_ok() {
        logger.log(format!(
            "🐋 Successfully executed whale emergency sell for {} tokens of {}",
            amount_to_sell, token_mint
        ).green().to_string());
        
        // Remove from tracking since all tokens were sold
        BOUGHT_TOKEN_LIST.remove(token_mint);
        TOKEN_TRACKING.remove(token_mint);
        logger.log(format!("Removed {} from all tracking systems after whale emergency sell", token_mint));
    }
    
    result
}

/// Execute PumpFun emergency sell with zeroslot
async fn execute_pumpfun_emergency_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("🐋 Generated PumpFun whale emergency sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::core::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 ZEROSLOT whale emergency sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpFun whale emergency sell instruction: {}", e)),
    }
}

/// Execute PumpSwap emergency sell with zeroslot
async fn execute_pumpswap_emergency_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("🐋 Generated PumpSwap whale emergency sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::core::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 ZEROSLOT whale emergency sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpSwap whale emergency sell instruction: {}", e)),
    }
}

/// Execute Raydium emergency sell with zeroslot
async fn execute_raydium_emergency_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::Raydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("🐋 Generated Raydium whale emergency sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::core::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 ZEROSLOT Raydium whale emergency sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build Raydium whale emergency sell instruction: {}", e)),
    }
}

/// Enhanced sell execution with comprehensive selling logic
pub async fn execute_enhanced_sell(
    token_mint: String,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
) -> Result<(), String> {
    let logger = Logger::new("[ENHANCED-SELL] => ".green().to_string());
    
    // Get token info from global tracking
    let mut token_info = match BOUGHT_TOKEN_LIST.get_mut(&token_mint) {
        Some(info) => info,
        None => {
            return Err(format!("Token {} not found in tracking list", token_mint));
        }
    };
    
    // Get selling action based on comprehensive rules
    let selling_action = token_info.get_selling_action();
    
    // Debug logging for tokens with invalid entry price
    if token_info.entry_price == 0 {
        logger.log(format!("WARNING: Token {} has entry_price = 0, buy transaction may not be processed yet", token_mint).yellow().to_string());
    }
    
    match selling_action {
        SellingAction::Hold => {
            logger.log(format!("Holding token {}", token_mint));
            return Ok(());
        },
        SellingAction::SellAll(reason) => {
            logger.log(format!("Selling ALL of token {} - Reason: {}", token_mint, reason));
            execute_sell_all_enhanced(&token_mint, &mut token_info, app_state, swap_config).await
        }
    }
}

/// Execute sell all with zeroslot for maximum speed
async fn execute_sell_all_enhanced(
    token_mint: &str,
    token_info: &mut BoughtTokenInfo,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
) -> Result<(), String> {
    let logger = Logger::new("[SELL-ALL-ENHANCED] => ".red().to_string());
    
    // Get current token balance
    let wallet_pubkey = app_state.wallet.try_pubkey()
        .map_err(|e| format!("Failed to get wallet pubkey: {}", e))?;
    let token_pubkey = Pubkey::from_str(token_mint)
        .map_err(|e| format!("Invalid token mint: {}", e))?;
    let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
    
    let token_amount = match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
        Ok(Some(account)) => {
            let amount_value = account.token_amount.amount.parse::<f64>()
                .map_err(|e| format!("Failed to parse token amount: {}", e))?;
            amount_value / 10f64.powi(account.token_amount.decimals as i32)
        },
        Ok(None) => {
            return Err(format!("No token account found for mint: {}", token_mint));
        },
        Err(e) => {
            return Err(format!("Failed to get token account: {}", e));
        }
    };
    
    if token_amount <= 0.0 {
        logger.log("No tokens to sell".yellow().to_string());
        return Ok(());
    }
    
    // Create sell config
    let mut sell_config = (*swap_config).clone();
    sell_config.swap_direction = SwapDirection::Sell;
    sell_config.amount_in = token_amount;
    sell_config.slippage = 1000; // 10% slippage for emergency sells
    
    // Create trade info for sell using original trade info
    let trade_info = create_sell_trade_info_from_original(token_mint, token_amount, &token_info.trade_info);
    
    let result = match token_info.protocol {
        SwapProtocol::PumpFun => {
            execute_pumpfun_sell_with_zeroslot(&trade_info, sell_config, app_state.clone(), &logger).await
        },
        SwapProtocol::PumpSwap => {
            execute_pumpswap_sell_with_zeroslot(&trade_info, sell_config, app_state.clone(), &logger).await
        },
        SwapProtocol::RaydiumLaunchpad => {
            execute_raydium_sell_with_zeroslot(&trade_info, sell_config, app_state.clone(), &logger).await
        },
        SwapProtocol::Auto | SwapProtocol::Unknown => {
            logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for sell all".yellow().to_string());
            execute_pumpfun_sell_with_zeroslot(&trade_info, (*swap_config).clone(), app_state.clone(), &logger).await
        },
    };
    
    if result.is_ok() {
        // Use comprehensive verification and cleanup
        match verify_sell_transaction_and_cleanup(
            token_mint,
            None, // No specific transaction signature for enhanced sell
            app_state.clone(),
            &logger,
        ).await {
            Ok(cleaned_up) => {
                if cleaned_up {
                    logger.log(format!("✅ Comprehensive cleanup completed for sell all: {}", token_mint));
                } else {
                    logger.log(format!("⚠️  Sell all cleanup verification failed for: {}", token_mint).yellow().to_string());
                }
            },
            Err(e) => {
                logger.log(format!("❌ Error during sell all cleanup verification: {}", e).red().to_string());
                // Fallback to basic removal
                BOUGHT_TOKEN_LIST.remove(token_mint);
                TOKEN_TRACKING.remove(token_mint);
                logger.log(format!("Fallback: Removed {} from basic tracking systems", token_mint));
            }
        }
    }
    
    result
}

/// Execute progressive sell with normal transaction method


/// Clean up tracking systems by removing tokens with zero balance
async fn cleanup_token_tracking(app_state: &Arc<AppState>) {
    let logger = Logger::new("[TRACKING-CLEANUP] => ".blue().to_string());
    
    // Get all tokens from both tracking systems
    let tokens_to_check: Vec<String> = BOUGHT_TOKEN_LIST.iter()
        .map(|entry| entry.key().clone())
        .collect();
    
    if tokens_to_check.is_empty() {
        return;
    }
    
    let mut removed_count = 0;
    
    for token_mint in tokens_to_check {
        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
            if let Ok(token_pubkey) = Pubkey::from_str(&token_mint) {
                let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
                
                match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                    Ok(account_result) => {
                        match account_result {
                            Some(account) => {
                                if let Ok(amount_value) = account.token_amount.amount.parse::<f64>() {
                                    let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                                    if decimal_amount <= 0.000001 {
                                        // Remove from all tracking systems
                                        BOUGHT_TOKEN_LIST.remove(&token_mint);
                                        TOKEN_TRACKING.remove(&token_mint);
                                        removed_count += 1;
                                        logger.log(format!("Cleanup: Removed {} with zero balance", token_mint));
                                    }
                                }
                            },
                            None => {
                                // Token account doesn't exist, remove from tracking
                                BOUGHT_TOKEN_LIST.remove(&token_mint);
                                TOKEN_TRACKING.remove(&token_mint);
                                removed_count += 1;
                                logger.log(format!("Cleanup: Removed {} (account not found)", token_mint));
                            }
                        }
                    },
                    Err(_) => {
                        // Error getting account, keep in tracking for now
                    }
                }
            }
        }
    }
    
    if removed_count > 0 {
        logger.log(format!("Cleanup completed: Removed {} stale tokens from tracking", removed_count));
    }
}

/// Monitor cleanup tasks for token tracking (transaction-driven selling logic handles actual selling)
async fn start_enhanced_selling_monitor(
    app_state: Arc<AppState>,
    _swap_config: Arc<SwapConfig>,
) {
    let logger = Logger::new("[ENHANCED-SELLING-MONITOR] => ".cyan().to_string());
    logger.log("Enhanced selling monitor started (cleanup-only mode - selling triggered by transactions)".to_string());
    
    // Run cleanup every 30 seconds
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let mut cleanup_cycle = 0;
    
    loop {
        interval.tick().await;
        cleanup_cycle += 1;
        
        // Run basic cleanup every cycle (30 seconds)
        cleanup_token_tracking(&app_state).await;
        
        // Run comprehensive cleanup every 4 cycles (2 minutes)
        if cleanup_cycle >= 4 {
            periodic_comprehensive_cleanup(app_state.clone(), &logger).await;
            cleanup_cycle = 0;
        }
        
        // Log current tracking status occasionally
        let tracked_count = BOUGHT_TOKEN_LIST.len();
        if tracked_count > 0 {
            logger.log(format!("Currently tracking {} tokens (selling checks triggered by transactions)", tracked_count).blue().to_string());
        }
    }
}

/// Update token price from current market data
async fn update_token_price(
    token_mint: &str,
    app_state: &Arc<AppState>,
) -> Result<(), String> {
    if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(token_mint) {
        let current_price = match token_info.protocol {
            SwapProtocol::PumpFun => {
                let pump = crate::dex::pump_fun::Pump::new(
                    app_state.rpc_nonblocking_client.clone(),
                    app_state.rpc_client.clone(),
                    app_state.wallet.clone(),
                );
                
                match pump.get_token_price(token_mint).await {
                    Ok(price) => price as u64, // Price is already scaled from get_token_price()
                    Err(_) => return Err("Failed to get PumpFun price".to_string()),
                }
            },
            SwapProtocol::PumpSwap => {
                let pump_swap = crate::dex::pump_swap::PumpSwap::new(
                    app_state.wallet.clone(),
                    Some(app_state.rpc_client.clone()),
                    Some(app_state.rpc_nonblocking_client.clone()),
                );
                
                match pump_swap.get_token_price(token_mint).await {
                    Ok(price) => (price * 1_000_000_000.0) as u64, // PumpSwap still needs scaling
                    Err(_) => return Err("Failed to get PumpSwap price".to_string()),
                }
            },
            SwapProtocol::RaydiumLaunchpad => {
                let raydium = crate::dex::raydium_launchpad::Raydium::new(
                    app_state.wallet.clone(),
                    Some(app_state.rpc_client.clone()),
                    Some(app_state.rpc_nonblocking_client.clone()),
                );
                
                match raydium.get_token_price(token_mint).await {
                    Ok(price) => (price * 1_000_000_000.0) as u64, // Raydium still needs scaling
                    Err(_) => return Err("Failed to get Raydium price".to_string()),
                }
            },
            SwapProtocol::Auto | SwapProtocol::Unknown => {
                // For Auto/Unknown protocols, try PumpFun first
                let pump_fun = crate::dex::pump_fun::Pump::new(
                    app_state.rpc_nonblocking_client.clone(),
                    app_state.rpc_client.clone(),
                    app_state.wallet.clone(),
                );
                
                match pump_fun.get_token_price(token_mint).await {
                    Ok(price) => price as u64, // Price is already scaled from get_token_price()
                    Err(_) => {
                        // If PumpFun fails, fall back to current price from token info
                        token_info.current_price
                    },
                }
            },
        };
        
        // Debug logging for price updates
        println!("DEBUG PRICE UPDATE: Token {} - Protocol: {:?}, Old: {}, New: {}", 
            token_mint, token_info.protocol, token_info.current_price, current_price);
        
        // Update the price using the enhanced method
        token_info.update_price(current_price);
        
        Ok(())
    } else {
        Err(format!("Token {} not found in tracking", token_mint))
    }
}

/// Create trade info for selling using original trade info to preserve important fields
fn create_sell_trade_info_from_original(
    token_mint: &str,
    token_amount: f64,
    original_trade_info: &transaction_parser::TradeInfoFromToken,
) -> transaction_parser::TradeInfoFromToken {
    transaction_parser::TradeInfoFromToken {
        dex_type: original_trade_info.dex_type.clone(),
        slot: original_trade_info.slot,
        signature: "enhanced_sell".to_string(),
        pool_id: original_trade_info.pool_id.clone(),
        mint: token_mint.to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        is_buy: false,
        price: original_trade_info.price,
        is_reverse_when_pump_swap: original_trade_info.is_reverse_when_pump_swap,
        coin_creator: original_trade_info.coin_creator.clone(), // This is the key field that was missing!
        sol_change: 0.0,
        token_change: token_amount,
        liquidity: original_trade_info.liquidity,
        virtual_sol_reserves: original_trade_info.virtual_sol_reserves,
        virtual_token_reserves: original_trade_info.virtual_token_reserves,
    }
}

/// Create trade info for selling (legacy function - kept for compatibility)
fn create_sell_trade_info(
    token_mint: &str,
    token_amount: f64,
    protocol: &SwapProtocol,
) -> transaction_parser::TradeInfoFromToken {
    transaction_parser::TradeInfoFromToken {
        dex_type: match protocol {
            SwapProtocol::PumpSwap => transaction_parser::DexType::PumpSwap,
            SwapProtocol::PumpFun => transaction_parser::DexType::PumpFun,
            SwapProtocol::RaydiumLaunchpad => transaction_parser::DexType::RaydiumLaunchpad,
            _ => transaction_parser::DexType::Unknown,
        },
        slot: 0,
        signature: "enhanced_sell".to_string(),
        pool_id: String::new(),
        mint: token_mint.to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        is_buy: false,
        price: 0,
        is_reverse_when_pump_swap: false,
        coin_creator: None,
        sol_change: 0.0,
        token_change: token_amount,
        liquidity: 0.0,
        virtual_sol_reserves: 0,
        virtual_token_reserves: 0,
    }
}

/// Execute PumpFun sell with zeroslot
async fn execute_pumpfun_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::core::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("ZEROSLOT sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpFun sell instruction: {}", e)),
    }
}

/// Execute PumpSwap sell with zeroslot
async fn execute_pumpswap_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated PumpSwap sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::core::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("ZEROSLOT sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpSwap sell instruction: {}", e)),
    }
}

/// Execute PumpFun sell with normal transaction method
async fn execute_pumpfun_sell_with_normal(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::core::tx::new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("NORMAL sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpFun sell instruction: {}", e)),
    }
}

/// Execute PumpSwap sell with normal transaction method
async fn execute_pumpswap_sell_with_normal(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated PumpSwap sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::core::tx::new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("NORMAL sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpSwap sell instruction: {}", e)),
    }
}

/// Execute Raydium sell with zeroslot
async fn execute_raydium_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::Raydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated Raydium sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::core::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("ZEROSLOT Raydium sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build Raydium sell instruction: {}", e)),
    }
}

/// Execute Raydium sell with normal transaction method
async fn execute_raydium_sell_with_normal(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::Raydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated Raydium sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::core::tx::new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("NORMAL Raydium sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build Raydium sell instruction: {}", e)),
    }
}

/// Execute sell operation for a token
pub async fn execute_sell(
    token_mint: String,
    trade_info: transaction_parser::TradeInfoFromToken,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    protocol: SwapProtocol,
    chunks: Option<usize>,
    interval_ms: Option<u64>,
) -> Result<(), String> {
    let logger = Logger::new("[EXECUTE-SELL] => ".green().to_string());
    let start_time = Instant::now();
    
    logger.log(format!("Selling token: {}", token_mint));
    
    // Protocol string for notifications
    let _protocol_str = match protocol {
        SwapProtocol::PumpSwap => "PumpSwap",
        SwapProtocol::PumpFun => "PumpFun",
        SwapProtocol::RaydiumLaunchpad => "RaydiumLaunchpad",
        _ => "Unknown",
    };
    
    // Create a minimal trade info for notification using the new structure
    let notification_trade_info = transaction_parser::TradeInfoFromToken {
        dex_type: match protocol {
            SwapProtocol::PumpSwap => transaction_parser::DexType::PumpSwap,
            SwapProtocol::PumpFun => transaction_parser::DexType::PumpFun,
            SwapProtocol::RaydiumLaunchpad => transaction_parser::DexType::RaydiumLaunchpad,
            _ => transaction_parser::DexType::Unknown,
        },
        slot: trade_info.slot,
        signature: trade_info.signature.clone(),
        pool_id: trade_info.pool_id.clone(),
        mint: trade_info.mint.clone(),
        timestamp: trade_info.timestamp,
        is_buy: false, // This is a sell notification
        price: trade_info.price,
        is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
        coin_creator: trade_info.coin_creator.clone(),
        sol_change: trade_info.sol_change,
        token_change: trade_info.token_change,
        liquidity: trade_info.liquidity,
        virtual_sol_reserves: trade_info.virtual_sol_reserves,
        virtual_token_reserves: trade_info.virtual_token_reserves,
    };

    // Create a modified swap config for selling
    let mut sell_config = (*swap_config).clone();
    sell_config.swap_direction = SwapDirection::Sell;

    // Get wallet pubkey - handle the error properly instead of using ?
    let wallet_pubkey = match app_state.wallet.try_pubkey() {
        Ok(pubkey) => pubkey,
        Err(e) => return Err(format!("Failed to get wallet pubkey: {}", e)),
    };

    // Get token account to determine how much we own
    let token_pubkey = match Pubkey::from_str(&token_mint) {
        Ok(pubkey) => pubkey,
        Err(e) => return Err(format!("Invalid token mint address: {}", e)),
    };
    let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

    // Get token account and amount
    let token_amount = match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
        Ok(Some(account)) => {
            // Parse the amount string instead of casting
            let amount_value = match account.token_amount.amount.parse::<f64>() {
                Ok(val) => val,
                Err(e) => return Err(format!("Failed to parse token amount: {}", e)),
            };
            let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
            logger.log(format!("Token amount to sell: {}", decimal_amount));
            decimal_amount
        },
        Ok(None) => {
            return Err(format!("No token account found for mint: {}", token_mint));
        },
        Err(e) => {
            return Err(format!("Failed to get token account: {}", e));
        }
    };
    
    // Update trade info with token amount
    let mut notification_trade_info = notification_trade_info.clone();
    // token_amount is already set in token_change field

    // Now that we have token_amount, set slippage based on token value
    // Use a fixed slippage since calculate_dynamic_slippage is private
    let token_value = token_amount * 0.01; // Estimate value at 0.01 SOL per token as a conservative default
    let slippage_bps = if token_value > 10.0 {
        300 // 3% for high value tokens (300 basis points)
    } else if token_value > 1.0 {
        200 // 2% for medium value tokens (200 basis points)
    } else {
        100 // 1% for low value tokens (100 basis points)
    };

    logger.log(format!("Using slippage of {}%", slippage_bps as f64 / 100.0));
    sell_config.slippage = slippage_bps;
    
    // Always use immediate sell (progressive selling removed)
    if false {
        let chunks_count = chunks.unwrap_or(3);
        let interval = interval_ms.unwrap_or(2000); // 2 seconds default
        
        logger.log(format!("Executing progressive sell in {} chunks with {} ms intervals", chunks_count, interval));
        
        // Calculate chunk size
        let chunk_size = token_amount / chunks_count as f64;
        
        // Execute each chunk
        for i in 0..chunks_count {
            // Create a fresh sell config for each iteration by cloning
            let mut chunk_sell_config = (*swap_config).clone();
            chunk_sell_config.swap_direction = SwapDirection::Sell;
            chunk_sell_config.slippage = slippage_bps;
            
            // Adjust the final chunk to account for any rounding errors
            let amount_to_sell = if i == chunks_count - 1 {
                // For the last chunk, sell whatever is left
                match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                    Ok(Some(account)) => {
                        // Parse the amount string instead of casting
                        let amount_value = match account.token_amount.amount.parse::<f64>() {
                            Ok(val) => val,
                            Err(e) => return Err(format!("Failed to parse token amount: {}", e)),
                        };
                        let remaining = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                        if remaining < 0.000001 { // Very small amount, not worth selling
                            logger.log("Remaining amount too small, skipping final chunk".to_string());
                            break;
                        }
                        remaining
                    },
                    Ok(None) => chunk_size, // Fallback if we can't get the account
                    Err(e) => return Err(format!("Failed to get token account: {}", e)),
                }
            } else {
                chunk_size
            };
            
            if amount_to_sell <= 0.0 {
                logger.log("No tokens left to sell in this chunk".to_string());
                continue;
            }
            
            // Update config for this chunk
            chunk_sell_config.amount_in = amount_to_sell;
            
            logger.log(format!("Selling chunk {}/{}: {} tokens", i + 1, chunks_count, amount_to_sell));
            
            // Update trade info for this chunk
            let mut chunk_trade_info = notification_trade_info.clone();
            chunk_trade_info.token_change = amount_to_sell;
            
            // Execute sell based on protocol
            let result = match protocol {
                SwapProtocol::PumpFun => {
                    logger.log("Using PumpFun protocol for sell".to_string());
                    
                    // Create the PumpFun instance
                    let pump = crate::dex::pump_fun::Pump::new(
                        app_state.rpc_nonblocking_client.clone(),
                        app_state.rpc_client.clone(),
                        app_state.wallet.clone(),
                    );
                    
                    // Create a minimal trade info struct for the sell
                    let trade_info_clone = transaction_parser::TradeInfoFromToken {
                        dex_type: transaction_parser::DexType::PumpFun,
                        slot: 0,
                        signature: "standard_sell".to_string(),
                        pool_id: String::new(),
                        mint: token_mint.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        is_buy: false,
                        price: 0,
                        is_reverse_when_pump_swap: false,
                        coin_creator: None,
                        sol_change: 0.0,
                        token_change: amount_to_sell,
                        liquidity: 0.0,
                        virtual_sol_reserves: 0,
                        virtual_token_reserves: 0,
                    };
                    
                    // Build swap instructions for sell
                    match pump.build_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
                            // Execute the transaction
                            match crate::core::tx::new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                    Some(hash) => hash,
                                    None => {
                                        logger.log("Failed to get recent blockhash".red().to_string());
                                        return Err("Failed to get recent blockhash".to_string());
                                    }
                                },
                                &keypair,
                                instructions,
                                &logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    logger.log(format!("Sell transaction sent: {}", signature));
                                    
                                    // Verify transaction
                                    match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                logger.log("Sell transaction verified successfully".to_string());
                                                
                                                Ok(())
                                            } else {
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed to build PumpFun sell instruction: {}", e))
                        },
                    }
                },
                SwapProtocol::PumpSwap => {
                    logger.log("Using PumpSwap protocol for sell".to_string());
                    
                    // Create the PumpSwap instance
                    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
                        app_state.wallet.clone(),
                        Some(app_state.rpc_client.clone()),
                        Some(app_state.rpc_nonblocking_client.clone()),
                    );
                    
                    // Create a minimal trade info struct for the sell
                    let trade_info_clone = transaction_parser::TradeInfoFromToken {
                        dex_type: transaction_parser::DexType::PumpSwap,
                        slot: trade_info.slot,
                        signature: "standard_sell".to_string(),
                        pool_id: trade_info.pool_id.clone(),
                        mint: token_mint.clone(),
                        timestamp: trade_info.timestamp,
                        is_buy: false,
                        price: trade_info.price,
                        is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
                        coin_creator: trade_info.coin_creator.clone(),
                        sol_change: trade_info.sol_change,
                        token_change: amount_to_sell,
                        liquidity: trade_info.liquidity,
                        virtual_sol_reserves: trade_info.virtual_sol_reserves,
                        virtual_token_reserves: trade_info.virtual_token_reserves,
                    };
                    
                    // Build swap instructions for sell - use chunk_sell_config
                    match pump_swap.build_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            // Get recent blockhash from the processor
                            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                Some(hash) => hash,
                                None => {
                                    logger.log("Failed to get recent blockhash".red().to_string());
                                    return Err("Failed to get recent blockhash".to_string());
                                }
                            };
                            logger.log(format!("Generated PumpSwap sell instruction at price: {}", price));
                            logger.log(format!("copy transaction {}", trade_info_clone.signature));
                            
                            // Execute the transaction
                            match crate::core::tx::new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                recent_blockhash,
                                &keypair,
                                instructions,
                                &logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    logger.log(format!("Sell transaction sent: {}", signature));
                                    
                                    // Verify transaction
                                    match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                logger.log("Sell transaction verified successfully".to_string());
                                                
                                                
                                                Ok(())
                                            } else {
                                                
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed to build PumpSwap sell instruction: {}", e))
                        },
                    }
                },
                SwapProtocol::RaydiumLaunchpad => {
                    logger.log("Using Raydium protocol for sell".to_string());
                    
                    let raydium = crate::dex::raydium_launchpad::Raydium::new(
                        app_state.wallet.clone(),
                        Some(app_state.rpc_client.clone()),
                        Some(app_state.rpc_nonblocking_client.clone()),
                    );
                    
                    let trade_info_clone = transaction_parser::TradeInfoFromToken {
                        dex_type: transaction_parser::DexType::RaydiumLaunchpad,
                        slot: trade_info.slot,
                        signature: "standard_sell".to_string(),
                        pool_id: trade_info.pool_id.clone(),
                        mint: token_mint.clone(),
                        timestamp: trade_info.timestamp,
                        is_buy: false,
                        price: trade_info.price,
                        is_reverse_when_pump_swap: false,
                        coin_creator: trade_info.coin_creator.clone(),
                        sol_change: trade_info.sol_change,
                        token_change: amount_to_sell,
                        liquidity: trade_info.liquidity,
                        virtual_sol_reserves: trade_info.virtual_sol_reserves,
                        virtual_token_reserves: trade_info.virtual_token_reserves,
                    };
                    
                    match raydium.build_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                Some(hash) => hash,
                                None => {
                                    logger.log("Failed to get recent blockhash".red().to_string());
                                    return Err("Failed to get recent blockhash".to_string());
                                }
                            };
                            logger.log(format!("Generated Raydium sell instruction at price: {}", price));
                            
                            match crate::core::tx::new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                recent_blockhash,
                                &keypair,
                                instructions,
                                &logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    logger.log(format!("Sell transaction sent: {}", signature));
                                    
                                    match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                logger.log("Sell transaction verified successfully".to_string());
                                                Ok(())
                                            } else {
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed to build Raydium sell instruction: {}", e))
                        },
                    }
                },
                SwapProtocol::Auto | SwapProtocol::Unknown => {
                    logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for sell".yellow().to_string());
                    
                    let pump = crate::dex::pump_fun::Pump::new(
                        app_state.rpc_nonblocking_client.clone(),
                        app_state.rpc_client.clone(),
                        app_state.wallet.clone(),
                    );
                    
                    let trade_info_clone = transaction_parser::TradeInfoFromToken {
                        dex_type: transaction_parser::DexType::PumpFun,
                        slot: 0,
                        signature: "standard_sell".to_string(),
                        pool_id: String::new(),
                        mint: token_mint.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        is_buy: false,
                        price: 0,
                        is_reverse_when_pump_swap: false,
                        coin_creator: None,
                        sol_change: 0.0,
                        token_change: amount_to_sell,
                        liquidity: 0.0,
                        virtual_sol_reserves: 0,
                        virtual_token_reserves: 0,
                    };
                    
                    match pump.build_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
                            match crate::core::tx::new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                    Some(hash) => hash,
                                    None => {
                                        logger.log("Failed to get recent blockhash".red().to_string());
                                        return Err("Failed to get recent blockhash".to_string());
                                    }
                                },
                                &keypair,
                                instructions,
                                &logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    logger.log(format!("Sell transaction sent: {}", signature));
                                    
                                    match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                logger.log("Sell transaction verified successfully".to_string());
                                                Ok(())
                                            } else {
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed to build PumpFun sell instruction: {}", e))
                        },
                    }
                },
            };
            
            // If any chunk fails, return the error
            if let Err(e) = result {
                logger.log(format!("Failed to sell chunk {}/{}: {}", i + 1, chunks_count, e));
                

                
                return Err(e);
            }
            
            // Wait for the specified interval before next chunk
            if i < chunks_count - 1 {
                logger.log(format!("Waiting {}ms before next chunk", interval));
                tokio::time::sleep(Duration::from_millis(interval)).await;
            }
        }
        
        // Log execution time for progressive sell
        let elapsed = start_time.elapsed();
        logger.log(format!("Progressive sell execution time: {:?}", elapsed));

        // Increment sold counter and update tracking
        let sold_count = {
            let mut entry = SOLD_TOKENS.entry(()).or_insert(0);
            *entry += 1;
            *entry
        };
        logger.log(format!("Total sold: {}", sold_count));
        
        let bought_count = BOUGHT_TOKENS.get(&()).map(|r| *r).unwrap_or(0);
        let active_tokens: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
        
        // When all chunks are sold successfully, remove the token account from our global list
        // This ATA is now empty and may be closed
        WALLET_TOKEN_ACCOUNTS.remove(&ata);
        logger.log(format!("Removed token account {} from global list after progressive sell", ata));
        
        // Cancel monitoring task for this token since it's been sold
        if let Err(e) = cancel_token_monitoring(&token_mint, &logger).await {
            logger.log(format!("Failed to cancel monitoring for token {}: {}", token_mint, e).yellow().to_string());
        }
        

        

        
        Ok(())
    } else {
        // Standard single-transaction sell
        logger.log("Executing standard sell".to_string());
        
        // Configure to sell 100% of tokens
        sell_config.amount_in = token_amount;
        
        // Execute based on protocol
        let result = match protocol {
            SwapProtocol::PumpFun => {
                logger.log("Using PumpFun protocol for sell".to_string());
                
                // Create the PumpFun instance
                let pump = crate::dex::pump_fun::Pump::new(
                    app_state.rpc_nonblocking_client.clone(),
                    app_state.rpc_client.clone(),
                    app_state.wallet.clone(),
                );
                
                // Create a minimal trade info struct for the sell
                let trade_info_clone = transaction_parser::TradeInfoFromToken {
                    dex_type: transaction_parser::DexType::PumpFun,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: String::new(),
                    mint: token_mint.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    is_buy: false,
                    price: 0,
                    is_reverse_when_pump_swap: false,
                    coin_creator: None,
                    sol_change: 0.0,
                    token_change: token_amount,
                    liquidity: 0.0,
                    virtual_sol_reserves: 0,
                    virtual_token_reserves: 0,
                };
                
                // Build swap instructions for sell
                match pump.build_swap_from_parsed_data(&trade_info_clone, sell_config).await {
                    Ok((keypair, instructions, price)) => {
                        logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
                                                    // Execute the transaction
                            match crate::core::tx::new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                Some(hash) => hash,
                                None => {
                                    logger.log("Failed to get recent blockhash".red().to_string());
                                    return Err("Failed to get recent blockhash".to_string());
                                }
                            },
                            &keypair,
                            instructions,
                            &logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err("No transaction signature returned".to_string());
                                }
                                
                                let signature = &signatures[0];
                                logger.log(format!("Sell transaction sent: {}", signature));
                                

                                
                                // Verify transaction
                                match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                    Ok(verified) => {
                                        if verified {
                                            logger.log("Sell transaction verified successfully".to_string());
                                            

                                            
                                            Ok(())
                                        } else {
                                            Err("Sell transaction verification failed".to_string())
                                        }
                                    },
                                    Err(e) => {
                                        Err(format!("Transaction verification error: {}", e))
                                    },
                                }
                            },
                            Err(e) => {
                                Err(format!("Transaction error: {}", e))
                            },
                        }
                    },
                    Err(e) => {
                        Err(format!("Failed to build PumpFun sell instruction: {}", e))
                    },
                }
            },
            SwapProtocol::PumpSwap => {
                logger.log("Using PumpSwap protocol for sell".to_string());
                
                // Create the PumpSwap instance
                let pump_swap = crate::dex::pump_swap::PumpSwap::new(
                    app_state.wallet.clone(),
                    Some(app_state.rpc_client.clone()),
                    Some(app_state.rpc_nonblocking_client.clone()),
                );
                
                // Create a minimal trade info struct for the sell
                let trade_info_clone = transaction_parser::TradeInfoFromToken {
                    dex_type: transaction_parser::DexType::PumpSwap,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: String::new(),
                    mint: token_mint.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    is_buy: false,
                    price: 0,
                    is_reverse_when_pump_swap: false,
                    coin_creator: None,
                    sol_change: 0.0,
                                    token_change: token_amount,
                liquidity: 0.0,
                virtual_sol_reserves: 0,
                virtual_token_reserves: 0,
            };
                
                // Build swap instructions for sell
                match pump_swap.build_swap_from_parsed_data(&trade_info_clone, sell_config).await {
                    Ok((keypair, instructions, price)) => {
                        // Get recent blockhash from the processor
                        let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                            Some(hash) => hash,
                            None => {
                                logger.log("Failed to get recent blockhash".red().to_string());
                                return Err("Failed to get recent blockhash".to_string());
                            }
                        };
                        logger.log(format!("Generated PumpSwap sell instruction at price: {}", price));
                        // Execute the transaction
                        match crate::core::tx::new_signed_and_send_zeroslot(
                            app_state.zeroslot_rpc_client.clone(),
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err("No transaction signature returned".to_string());
                                }
                                
                                let signature = &signatures[0];
                                logger.log(format!("Sell transaction sent: {}", signature));
                                

                                
                                // Verify transaction
                                match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                    Ok(verified) => {
                                        if verified {
                                            logger.log("Sell transaction verified successfully".to_string());
                                            
                                            // Send notification for successful verification

                                            
                                            Ok(())
                                        } else {
                                            Err("Sell transaction verification failed".to_string())
                                        }
                                    },
                                    Err(e) => {
                                        Err(format!("Transaction verification error: {}", e))
                                    },
                                }
                            },
                            Err(e) => {
                                Err(format!("Transaction error: {}", e))
                            },
                        }
                    },
                    Err(e) => {
                        Err(format!("Failed to build PumpSwap sell instruction: {}", e))
                    },
                }
            },
            SwapProtocol::RaydiumLaunchpad => {
                logger.log("Using Raydium protocol for sell".to_string());
                
                let raydium = crate::dex::raydium_launchpad::Raydium::new(
                    app_state.wallet.clone(),
                    Some(app_state.rpc_client.clone()),
                    Some(app_state.rpc_nonblocking_client.clone()),
                );
                
                let trade_info_clone = transaction_parser::TradeInfoFromToken {
                    dex_type: transaction_parser::DexType::RaydiumLaunchpad,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: trade_info.pool_id.clone(),
                    mint: token_mint.clone(),
                    timestamp: trade_info.timestamp,
                    is_buy: false,
                    price: trade_info.price,
                    is_reverse_when_pump_swap: false,
                    coin_creator: trade_info.coin_creator.clone(),
                    sol_change: trade_info.sol_change,
                    token_change: token_amount,
                    liquidity: trade_info.liquidity,
                    virtual_sol_reserves: trade_info.virtual_sol_reserves,
                    virtual_token_reserves: trade_info.virtual_token_reserves,
                };
                
                match raydium.build_swap_from_parsed_data(&trade_info_clone, sell_config).await {
                    Ok((keypair, instructions, price)) => {
                        logger.log(format!("Generated Raydium sell instruction at price: {}", price));
                        match crate::core::tx::new_signed_and_send_with_landing_mode(
                            crate::common::config::TransactionLandingMode::Normal,
                            &app_state,
                            match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                Some(hash) => hash,
                                None => {
                                    logger.log("Failed to get recent blockhash".red().to_string());
                                    return Err("Failed to get recent blockhash".to_string());
                                }
                            },
                            &keypair,
                            instructions,
                            &logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err("No transaction signature returned".to_string());
                                }
                                
                                let signature = &signatures[0];
                                logger.log(format!("Sell transaction sent: {}", signature));
                                
                                match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                    Ok(verified) => {
                                        if verified {
                                            logger.log("Sell transaction verified successfully".to_string());
                                            Ok(())
                                        } else {
                                            Err("Sell transaction verification failed".to_string())
                                        }
                                    },
                                    Err(e) => {
                                        Err(format!("Transaction verification error: {}", e))
                                    },
                                }
                            },
                            Err(e) => {
                                Err(format!("Transaction error: {}", e))
                            },
                        }
                    },
                    Err(e) => {
                        Err(format!("Failed to build Raydium sell instruction: {}", e))
                    },
                }
            },
            SwapProtocol::Auto | SwapProtocol::Unknown => {
                logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for sell".yellow().to_string());
                
                let pump = crate::dex::pump_fun::Pump::new(
                    app_state.rpc_nonblocking_client.clone(),
                    app_state.rpc_client.clone(),
                    app_state.wallet.clone(),
                );
                
                let trade_info_clone = transaction_parser::TradeInfoFromToken {
                    dex_type: transaction_parser::DexType::PumpFun,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: String::new(),
                    mint: token_mint.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    is_buy: false,
                    price: 0,
                    is_reverse_when_pump_swap: false,
                    coin_creator: None,
                    sol_change: 0.0,
                    token_change: token_amount,
                    liquidity: 0.0,
                    virtual_sol_reserves: 0,
                    virtual_token_reserves: 0,
                };
                
                match pump.build_swap_from_parsed_data(&trade_info_clone, sell_config).await {
                    Ok((keypair, instructions, price)) => {
                        logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
                        match crate::core::tx::new_signed_and_send_with_landing_mode(
                            crate::common::config::TransactionLandingMode::Normal,
                            &app_state,
                            match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                Some(hash) => hash,
                                None => {
                                    logger.log("Failed to get recent blockhash".red().to_string());
                                    return Err("Failed to get recent blockhash".to_string());
                                }
                            },
                            &keypair,
                            instructions,
                            &logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err("No transaction signature returned".to_string());
                                }
                                
                                let signature = &signatures[0];
                                logger.log(format!("Sell transaction sent: {}", signature));
                                
                                match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                    Ok(verified) => {
                                        if verified {
                                            logger.log("Sell transaction verified successfully".to_string());
                                            Ok(())
                                        } else {
                                            Err("Sell transaction verification failed".to_string())
                                        }
                                    },
                                    Err(e) => {
                                        Err(format!("Transaction verification error: {}", e))
                                    },
                                }
                            },
                            Err(e) => {
                                Err(format!("Transaction error: {}", e))
                            },
                        }
                    },
                    Err(e) => {
                        Err(format!("Failed to build PumpFun sell instruction: {}", e))
                    },
                }
            },
        };
        
        // Log execution time for standard sell
        let elapsed = start_time.elapsed();
        logger.log(format!("Standard sell execution time: {:?}", elapsed));
        
        // Increment sold counter on success
        if result.is_ok() {
            let sold_count = {
                let mut entry = SOLD_TOKENS.entry(()).or_insert(0);
                *entry += 1;
                *entry
            };
            logger.log(format!("Total sold: {}", sold_count));
            
            let bought_count = BOUGHT_TOKENS.get(&()).map(|r| *r).unwrap_or(0);
            let active_tokens: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
            
            // Remove token account from our global list after successful sell
            WALLET_TOKEN_ACCOUNTS.remove(&ata);
            logger.log(format!("Removed token account {} from global list after standard sell", ata));
            
            // Use comprehensive verification and cleanup
            match verify_sell_transaction_and_cleanup(
                &token_mint,
                None, // No specific transaction signature available here
                app_state.clone(),
                &logger,
            ).await {
                Ok(cleaned_up) => {
                    if cleaned_up {
                        logger.log(format!("✅ Comprehensive cleanup completed for standard sell: {}", token_mint));
                    }
                },
                Err(e) => {
                    logger.log(format!("❌ Error during standard sell cleanup verification: {}", e).red().to_string());
                    // Fallback to cancel monitoring
                    if let Err(e) = cancel_token_monitoring(&token_mint, &logger).await {
                        logger.log(format!("Failed to cancel monitoring for token {}: {}", token_mint, e).yellow().to_string());
                    }
                }
            }
            

        }
        
        result
    }
}

/// Process incoming stream messages
async fn process_message(
    msg: &SubscribeUpdate,
    _subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
    config: Arc<CopyTradingConfig>,
    logger: &Logger,
) -> Result<(), String> {
    let start_time = Instant::now();
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }
    let mut target_signature = None;
    // Handle transaction messages
    if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
        let _start_time = Instant::now();
        // Extract transaction logs and account keys
        if let Some(transaction) = &txn.transaction {
            target_signature = match Signature::try_from(transaction.signature.clone()) {
                Ok(signature) => Some(signature),
                Err(e) => {
                    logger.log(format!("Invalid signature: {:?}", e).red().to_string());
                    return Err(format!("Invalid signature: {:?}", e));
                }
            };
        }
        let inner_instructions = match &txn.transaction {
            Some(txn_info) => match &txn_info.meta {
                Some(meta) => meta.inner_instructions.clone(),
                None => vec![],
            },
            None => vec![],
        };
        if !inner_instructions.is_empty() {
            // Find inner instruction with data length of 368 or 233
            let cpi_log_data = inner_instructions
                .iter()
                .flat_map(|inner| &inner.instructions)
                .find(|ix| ix.data.len() == 368 || ix.data.len() == 233 || ix.data.len() == 270  || ix.data.len() == 146)
                .map(|ix| ix.data.clone());

            if let Some(data) = cpi_log_data {
                let config = config.clone();
                let logger = logger.clone();
                let txn = txn.clone();  // Clone the transaction data
                tokio::spawn(async move {
                    if let Some(parsed_data) = crate::engine::transaction_parser::parse_transaction_data(&txn, &data) {
                    let _ =  handle_parsed_data_for_buying(parsed_data, config, target_signature, &logger).await;
                    }
                });
            }
        }

    }
    logger.log(format!("Processing overall grpc stream time: {:?}", start_time.elapsed()).blue().to_string());
    Ok(())  
}

async fn handle_parsed_data_for_selling(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<CopyTradingConfig>,
    logger: &Logger,
) -> Result<(), String> {
    let start_time = Instant::now();
    let instruction_type = parsed_data.dex_type.clone();
    let mint = parsed_data.mint.clone();
    
    // Log the parsed transaction data
    logger.log(format!(
        "Token transaction detected for {}: Instruction: {}, Is buy: {}",
        mint,
        match instruction_type {
            transaction_parser::DexType::PumpSwap => "PumpSwap",
            transaction_parser::DexType::PumpFun => "PumpFun",
            transaction_parser::DexType::RaydiumLaunchpad => "RaydiumLaunchpad",
            _ => "Unknown",
        },
        parsed_data.is_buy
    ).green().to_string());
    
    // Create selling engine
    let selling_engine = SellingEngine::new(
        config.app_state.clone().into(),
        Arc::new(config.swap_config.clone()),
        SellingConfig::set_from_env(), // SellingConfig::default(), 
    );
    
    // Update token metrics using the TradeInfoFromToken directly
    if let Err(e) = selling_engine.update_metrics(&mint, &parsed_data).await {
        logger.log(format!("Error updating metrics: {}", e).red().to_string());
    } else {
        logger.log(format!("Updated metrics for token: {}", mint).green().to_string());
    }
    
    // Check if we should sell this token
    match selling_engine.evaluate_sell_conditions(&mint).await {
        Ok((should_sell, sell_all)) => {
            if should_sell {
                logger.log(format!("Sell conditions met for token: {}", mint).green().to_string());
                
                // Determine protocol to use for selling
                let protocol = match instruction_type {
                    transaction_parser::DexType::PumpSwap => SwapProtocol::PumpSwap,
                    transaction_parser::DexType::PumpFun => SwapProtocol::PumpFun,
                    _ => config.protocol_preference.clone(),
                };
                
                if sell_all {
                    // Emergency sell all tokens immediately to prevent further losses
                    logger.log(format!("EMERGENCY SELL ALL triggered for token: {}", mint).red().bold().to_string());
                    
                    match selling_engine.emergency_sell_all(&mint, &parsed_data, protocol.clone()).await {
                        Ok(_) => {
                            logger.log(format!("Successfully executed emergency sell all for token: {}", mint).green().to_string());
                            // Cancel monitoring task for this token since it's been sold
                            if let Err(e) = cancel_token_monitoring(&mint, &logger).await {
                                logger.log(format!("Failed to cancel monitoring for token {}: {}", mint, e).yellow().to_string());
                            }

                        },
                        Err(e) => {
                            logger.log(format!("Error executing emergency sell all: {}", e).red().to_string());
                            

                            
                            return Err(format!("Failed to execute emergency sell all: {}", e));
                        }
                    }
                } else {
                    // Normal selling logic - try emergency sell all first, then fallback to standard sell
                    println!("emergency sell all : token creator: {:?}", parsed_data.coin_creator);
                    if let Err(e) = selling_engine.emergency_sell_all(&mint, &parsed_data, protocol.clone()).await {
                        logger.log(format!("Error executing emergency sell all: {}", e).red().to_string());
                        
                        // TODO: This logic might need to be updated later. If emergency sell all fails, try standard sell
                        // logger.log("Attempting standard sell as fallback".yellow().to_string());
                        // if let Err(e) = execute_sell(
                        //     mint.clone(),
                        //     parsed_data.clone(),
                        //     config.app_state.clone().into(),
                        //     Arc::new(config.swap_config.clone()),
                        //     protocol.clone(),
                        //     false,  // Not progressive
                        //     None,   // Default chunks
                        //     None,   // Default interval
                        // ).await {
                        //     logger.log(format!("Error executing standard sell: {}", e).red().to_string());
                        //     return Err(format!("Failed to sell token: {}", e));
                        // } else {
                        //     // Standard sell succeeded, cancel monitoring
                        //     if let Err(e) = cancel_token_monitoring(&mint, &logger).await {
                        //         logger.log(format!("Failed to cancel monitoring for token {}: {}", mint, e).yellow().to_string());
                        //     }
                        // }
                    } else {
                        // Progressive sell succeeded, cancel monitoring
                        if let Err(e) = cancel_token_monitoring(&mint, &logger).await {
                            logger.log(format!("Failed to cancel monitoring for token {}: {}", mint, e).yellow().to_string());
                        }
                    }
                    

                }
                
                logger.log(format!("Successfully processed sell for token: {}", mint).green().to_string());
            } else {
                logger.log(format!("Not selling token yet: {}", mint).blue().to_string());
            }
        },
        Err(e) => {
            logger.log(format!("Error evaluating sell conditions: {}", e).red().to_string());
        }
    }
    
    logger.log(format!("Processing time for sell transaction: {:?}", start_time.elapsed()).blue().to_string());
    Ok(())
}

/// Set up selling strategy for a token
async fn setup_selling_strategy(
    token_mint: String,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    protocol_preference: SwapProtocol,
) -> Result<(), String> {
    let logger = Logger::new("[SETUP-SELLING-STRATEGY] => ".green().to_string());
    
    // Initialize
    logger.log(format!("Setting up selling strategy for token: {}", token_mint));
    
    // Create cancellation token for this monitoring task
    let cancellation_token = CancellationToken::new();
    
    // Register the cancellation token
    MONITORING_TASKS.insert(token_mint.clone(), cancellation_token.clone());
    
    // Clone values that will be moved into the task
    let token_mint_cloned = token_mint.clone();
    let app_state_cloned = app_state.clone();
    let swap_config_cloned = swap_config.clone();
    let protocol_preference_cloned = protocol_preference.clone();
    let logger_cloned = logger.clone();
    
    // Spawn a task to handle the monitoring and selling
    tokio::spawn(async move {
        let _ = monitor_token_for_selling(
            token_mint_cloned, 
            app_state_cloned, 
            swap_config_cloned, 
            protocol_preference_cloned, 
            &logger_cloned,
            cancellation_token
        ).await;
    });
    Ok(())
}

/// Monitor a token specifically for selling opportunities
async fn monitor_token_for_selling(
    token_mint: String,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    protocol_preference: SwapProtocol,
    logger: &Logger,
    cancellation_token: CancellationToken,
) -> Result<(), String> {
    // Create config for the Yellowstone connection
    // This is a simplified version of what's in the main copy_trading function
    let mut yellowstone_grpc_http = "https://helsinki.rpcpool.com/".to_string(); // Default value
    let mut yellowstone_grpc_token = "your_token_here".to_string(); // Default value
    
    // Try to get config values from environment if available
    if let Ok(url) = std::env::var("YELLOWSTONE_GRPC_HTTP") {
        yellowstone_grpc_http = url;
    }
    
    if let Ok(token) = std::env::var("YELLOWSTONE_GRPC_TOKEN") {
        yellowstone_grpc_token = token;
    }
    
    logger.log("Connecting to Yellowstone gRPC for selling, will close connection after selling ...".green().to_string());
    
    // Connect to Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed to build client: {}", e))?
        .x_token::<String>(Some(yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed to set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed to set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;

    // Set up subscribe with retries
    let mut retry_count = 0;
    const MAX_RETRIES: u32 = 3;
    let (subscribe_tx, mut stream) = loop {
        match client.subscribe().await {
            Ok(pair) => break pair,
            Err(e) => {
                retry_count += 1;
                if retry_count >= MAX_RETRIES {
                    return Err(format!("Failed to subscribe after {} attempts: {}", MAX_RETRIES, e));
                }
                logger.log(format!(
                    "[CONNECTION ERROR] => Failed to subscribe (attempt {}/{}): {}. Retrying in 5 seconds...",
                    retry_count, MAX_RETRIES, e
                ).red().to_string());
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    // Convert to Arc to allow cloning across tasks
    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));
    // Set up subscription focused on the token mint
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "TokenMonitor".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false), // Exclude vote transactions
                failed: Some(false), // Exclude failed transactions
                signature: None,
                account_include: vec![token_mint.clone()], // Only include transactions involving our token
                account_exclude: vec![JUPITER_PROGRAM.to_string(), OKX_DEX_PROGRAM.to_string()], // Exclude some common programs
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
  
    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed to send subscribe request: {}", e))?;


    // Create config for tasks
    let copy_trading_config = CopyTradingConfig {
        yellowstone_grpc_http,
        yellowstone_grpc_token,
        app_state: (*app_state).clone(),
        swap_config: (*swap_config).clone(),
        counter_limit: 5,
        target_addresses: vec![token_mint.clone()],
        excluded_addresses: vec![JUPITER_PROGRAM.to_string(), OKX_DEX_PROGRAM.to_string()],
        protocol_preference,
    };

    let config = Arc::new(copy_trading_config);

    // Spawn heartbeat task
    let subscribe_tx_clone = subscribe_tx.clone();
    
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            
            if let Err(_e) = send_heartbeat_ping(&subscribe_tx_clone).await {
                break;
            }
        }
    });

    // Create timer for periodic selling checks (every 5 seconds)
    let mut selling_timer = time::interval(Duration::from_secs(5));
    
    // Main stream processing loop
    logger.log("Starting main processing loop with timer-based selling checks...".green().to_string());
    let monitoring_result = loop {
        tokio::select! {
            // Check for cancellation
            _ = cancellation_token.cancelled() => {
                logger.log(format!("Monitoring cancelled for token: {}", token_mint).yellow().to_string());
                break "cancelled";
            }
            // Timer-based selling checks
            _ = selling_timer.tick() => {
                // Check if token still exists in tracking
                if BOUGHT_TOKEN_LIST.contains_key(&token_mint) {
                    // Update token price first
                    if let Err(e) = update_token_price(&token_mint, &app_state).await {
                        logger.log(format!("Failed to update price for {}: {}", token_mint, e).yellow().to_string());
                    }
                    
                    // Execute threshold-based progressive selling (copy_trading.rs system)
                    if let Err(e) = execute_enhanced_sell(token_mint.clone(), app_state.clone(), swap_config.clone()).await {
                        logger.log(format!("Timer-based threshold selling check error for {}: {}", token_mint, e).yellow().to_string());
                    }
                    

                } else {
                    // Token no longer tracked, exit monitoring
                    logger.log(format!("Token {} no longer tracked, ending monitoring", token_mint).green().to_string());
                    break "token_sold";
                }
            }
            // Process stream messages
            msg_result = stream.next() => {
                match msg_result {
                    Some(Ok(msg)) => {
                        if let Err(e) = process_selling(&msg, &subscribe_tx, config.clone(), &logger).await {
                            logger.log(format!("Error processing message: {}", e).red().to_string());
                        }
                    },
                    Some(Err(e)) => {
                        logger.log(format!("Stream error: {:?}", e).red().to_string());
                        break "stream_error";
                    },
                    None => {
                        logger.log("Stream ended".yellow().to_string());
                        break "stream_ended";
                    }
                }
            }
        }
    };
    
    // Properly close the gRPC connection
    logger.log(format!("Closing gRPC connection for token {} (reason: {})", token_mint, monitoring_result).yellow().to_string());
    
    // Drop the subscription sender to close the subscription
    drop(subscribe_tx);
    
    // Drop the stream to close it
    drop(stream);
    
    // The client will be automatically dropped when the function ends, 
    // but we explicitly drop it here to ensure immediate cleanup
    drop(client);
    
    logger.log(format!("✅ Successfully closed gRPC connection for token: {}", token_mint).green().to_string());
    logger.log(format!("Monitoring task ended for token: {}", token_mint).yellow().to_string());
    
    Ok(())
}

/// Process incoming stream messages
async fn process_selling(
    msg: &SubscribeUpdate,
    _subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
    config: Arc<CopyTradingConfig>,
    logger: &Logger,
) -> Result<(), String> {
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }

    // Handle transaction messages
    if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
        let _start_time = Instant::now();
        
        // Extract transaction logs and account keys
        let inner_instructions = match &txn.transaction {
            Some(txn_info) => match &txn_info.meta {
                Some(meta) => meta.inner_instructions.clone(),
                None => vec![],
            },
            None => vec![],
        };

        if !inner_instructions.is_empty() {
            // Find the largest data payload in inner instructions
            let mut largest_data: Option<Vec<u8>> = None;
            let mut largest_size = 0;

            for inner in &inner_instructions {
                for ix in &inner.instructions {
                    if ix.data.len() > largest_size {
                        largest_size = ix.data.len();
                        largest_data = Some(ix.data.clone());
                    }
                }
            }

            if let Some(data) = largest_data {
                if let Some(parsed_data) = crate::engine::transaction_parser::parse_transaction_data(txn, &data, ) {
                    if parsed_data.mint != "So11111111111111111111111111111111111111112" {
                        return handle_parsed_data_for_selling(parsed_data, config, logger).await;
                    }
                }
            }
        }
    }
    
    Ok(())
}


async fn handle_parsed_data_for_buying(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<CopyTradingConfig>,
    target_signature: Option<Signature>,
    logger: &Logger,
) -> Result<(), String> {
    let start_time = Instant::now();
    let instruction_type = parsed_data.dex_type.clone();
    
    // WHALE DETECTION LOGIC - Check for large sell transactions
    if !parsed_data.is_buy {
        // This is a sell transaction - check if it's a whale selling
        // For sell transactions, sol_change represents SOL received from selling (positive value)
        // For buy transactions, sol_change represents SOL spent (negative value)
        let sol_amount = parsed_data.sol_change.abs();

        // Check if this is a whale selling (>= 10 SOL)
        if sol_amount >= crate::common::constants::WHALE_SELLING_AMOUNT_FOR_SELLING_TRIGGER {
            logger.log(format!(
                "🐋 WHALE SELLING DETECTED: {} SOL for token {} - triggering EMERGENCY SELL with zeroslot",
                sol_amount, parsed_data.mint
            ).red().bold().to_string());
            
            // Check if we own this token
            if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(&parsed_data.mint) {
                logger.log(format!(
                    "🚨 We own token {} that whale is selling - executing EMERGENCY SELL via zeroslot",
                    parsed_data.mint
                ).red().bold().to_string());
                
                // Execute emergency sell using zeroslot for maximum speed
                let result = execute_whale_emergency_sell(
                    &parsed_data.mint,
                    &mut token_info,
                    config.app_state.clone().into(),
                    Arc::new(config.swap_config.clone()),
                    &logger
                ).await;
                
                match result {
                    Ok(_) => {
                        logger.log(format!("✅ Successfully executed whale emergency sell for token: {}", parsed_data.mint).green().to_string());
                        
                        // Cancel monitoring for this token since it's been sold
                        if let Err(e) = cancel_token_monitoring(&parsed_data.mint, &logger).await {
                            logger.log(format!("Failed to cancel monitoring for token {}: {}", parsed_data.mint, e).yellow().to_string());
                        }
                    },
                    Err(e) => {
                        logger.log(format!("❌ Failed to execute whale emergency sell for token {}: {}", parsed_data.mint, e).red().to_string());
                    }
                }
            } else {
                logger.log(format!("🐋 Whale selling detected for {} but we don't own this token", parsed_data.mint).yellow().to_string());
            }
        }
        
        // For sell transactions, we don't proceed with buying logic
        return Ok(());
    }

    //TODO: add "if" condition based on the .env setting 
    //This is needed when copy whale wallet to prevent risk.
    //Whale wallet "cupsey" buy the same token serveral times and sell it at once.
    //So to prevent risk, we need to skip copying if the token is in the target wallet list.
    // Check if token is in target wallet list - if so, skip copying
    if crate::common::cache::TARGET_WALLET_TOKENS.contains(&parsed_data.mint) {
        logger.log(format!(
            "Skipping copy of token {} as it is already in target wallet list",
            parsed_data.mint
        ).yellow().to_string());
        return Ok(());
    }

    // Check active tokens count against counter_limit
    let active_tokens_count = TOKEN_TRACKING.len();
    let active_token_list: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
    
    if active_tokens_count >= config.counter_limit as usize {
        logger.log(format!(
            "Skipping buy for token {} - Active tokens ({}) at counter limit ({})",
            parsed_data.mint,
            active_tokens_count,
            config.counter_limit
        ).yellow().to_string());
        
        // Log details about current active tokens for debugging
        logger.log(format!(
            "📊 Current active tokens: [{}]",
            active_token_list.join(", ")
        ).cyan().to_string());

        return Ok(());
    }
    
    // Extract transaction data
    logger.log(format!(
        "{} transaction detected: SOL change: {}, Token change: {}, Is buy: {}",
        match instruction_type {
            transaction_parser::DexType::PumpSwap => "PumpSwap",
            transaction_parser::DexType::PumpFun => "PumpFun",
            transaction_parser::DexType::RaydiumLaunchpad => "RaydiumLaunchpad",
            _ => "Unknown",
        },
        parsed_data.sol_change,
        parsed_data.token_change,
        parsed_data.is_buy
    ).green().to_string());
    
    // Determine protocol based on instruction type
    let protocol = if matches!(instruction_type, transaction_parser::DexType::PumpSwap) {
        SwapProtocol::PumpSwap
    } else if matches!(instruction_type, transaction_parser::DexType::PumpFun) {
        SwapProtocol::PumpFun
    } else if matches!(instruction_type, transaction_parser::DexType::RaydiumLaunchpad) {
        SwapProtocol::RaydiumLaunchpad
    } else {
        // Default to the preferred protocol in config if instruction type is unknown
        config.protocol_preference.clone()
    };
    
    // Handle buy transaction
    if parsed_data.is_buy {
        match execute_buy(
            parsed_data.clone(),
            config.app_state.clone().into(),
            Arc::new(config.swap_config.clone()),
            protocol.clone(),
        ).await {
            Err(e) => {
                logger.log(format!("Error executing buy: {}", e).red().to_string());
                
                Err(e) // Return the error from execute_buy
            },
            Ok(_) => {      
                logger.log(format!("Processing time for buy transaction: {:?}", start_time.elapsed()).blue().to_string());
                logger.log(format!("copied transaction {}", target_signature.clone().unwrap_or_default()).blue().to_string());
                logger.log(format!("Now starting to monitor this token to sell at a profit").blue().to_string());
                
                // Wait for buy transaction to be confirmed and processed before starting selling monitor
                // This prevents race condition where selling monitor starts before entry_price is set
                logger.log("Waiting 2 seconds for buy transaction to be fully processed...".yellow().to_string());
                tokio::time::sleep(Duration::from_secs(2)).await;
                
                // Setup selling strategy based on take profit and stop loss
                match setup_selling_strategy(
                    parsed_data.mint.clone(), 
                    config.app_state.clone().into(), 
                    Arc::new(config.swap_config.clone()), 
                    protocol.clone(),
                ).await {
                    Ok(_) => {
                        logger.log("Selling strategy set up successfully".green().to_string());
                        Ok(())
                    },
                    Err(e) => {
                        logger.log(format!("Failed to set up selling strategy: {}", e).red().to_string());
                        Err(e)
                    }
                }
            }
        }
    } else {
        // For sell transactions, we don't copy them
        // We rely on our own take profit and stop loss strategy
        logger.log(format!("Processing time for buy transaction: {:?}", start_time.elapsed()).blue().to_string());
        logger.log(format!("Not copying selling transaction - using take profit and stop loss").blue().to_string());
        Ok(())
    }
}

/// Comprehensive token verification and cleanup after selling
async fn verify_sell_transaction_and_cleanup(
    token_mint: &str,
    transaction_signature: Option<&str>,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<bool, String> {
    // Skip if token mint is empty (used for general cleanup calls)
    if token_mint.is_empty() {
        return Ok(false);
    }
    
    logger.log(format!("Starting comprehensive verification and cleanup for token: {}", token_mint));
    
    let mut is_fully_sold = false;
    
    // Step 1: Verify transaction if signature provided
    if let Some(signature) = transaction_signature {
        match verify_transaction(signature, app_state.clone(), logger).await {
            Ok(verified) => {
                if verified {
                    logger.log(format!("✅ Sell transaction verified successfully: {}", signature));
                } else {
                    logger.log(format!("❌ Sell transaction verification failed: {}", signature).red().to_string());
                    return Ok(false);
                }
            },
            Err(e) => {
                logger.log(format!("❌ Error verifying transaction {}: {}", signature, e).red().to_string());
                return Ok(false);
            }
        }
    }
    
    // Step 2: Check actual token balance from blockchain
    if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
        if let Ok(token_pubkey) = Pubkey::from_str(token_mint) {
            let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
            
            match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                Ok(account_result) => {
                    match account_result {
                        Some(account) => {
                            if let Ok(amount_value) = account.token_amount.amount.parse::<f64>() {
                                let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                                logger.log(format!("Current token balance for {}: {}", token_mint, decimal_amount));
                                
                                if decimal_amount <= 0.000001 {
                                    is_fully_sold = true;
                                    logger.log(format!("✅ Token {} has zero balance - fully sold", token_mint));
                                } else {
                                    logger.log(format!("⚠️  Token {} still has balance: {} - not fully sold", token_mint, decimal_amount));
                                }
                            }
                        },
                        None => {
                            // Token account doesn't exist - means fully sold
                            is_fully_sold = true;
                            logger.log(format!("✅ Token account for {} doesn't exist - fully sold", token_mint));
                        }
                    }
                },
                Err(e) => {
                    logger.log(format!("❌ Error checking token balance for {}: {}", token_mint, e).yellow().to_string());
                    // Don't remove from tracking if we can't verify
                    return Ok(false);
                }
            }
        }
    }
    
    // Step 3: If fully sold, remove from all tracking systems
    if is_fully_sold {
        let mut removed_systems = Vec::new();
        
        // Remove from BOUGHT_TOKEN_LIST
        if BOUGHT_TOKEN_LIST.remove(token_mint).is_some() {
            removed_systems.push("BOUGHT_TOKEN_LIST");
        }
        
        // Remove from TOKEN_TRACKING (copy_trading.rs)
        if TOKEN_TRACKING.remove(token_mint).is_some() {
            removed_systems.push("TOKEN_TRACKING");
        }
        
        // Remove from selling_strategy TOKEN_TRACKING and TOKEN_METRICS
        if crate::engine::selling_strategy::TOKEN_TRACKING.remove(token_mint).is_some() {
            removed_systems.push("SELLING_STRATEGY_TOKEN_TRACKING");
        }
        
        if crate::engine::selling_strategy::TOKEN_METRICS.remove(token_mint).is_some() {
            removed_systems.push("TOKEN_METRICS");
        }
        
        // Cancel monitoring task
        if let Some(entry) = MONITORING_TASKS.remove(token_mint) {
            entry.1.cancel();
            removed_systems.push("MONITORING_TASKS");
        }
        
        // Remove from wallet token accounts
        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
            if let Ok(token_pubkey) = Pubkey::from_str(token_mint) {
                let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
                if WALLET_TOKEN_ACCOUNTS.remove(&ata) {
                    removed_systems.push("WALLET_TOKEN_ACCOUNTS");
                }
            }
        }
        
        logger.log(format!(
            "🧹 Comprehensive cleanup completed for {}. Removed from: [{}]", 
            token_mint, 
            removed_systems.join(", ")
        ).green().to_string());
        
        // Log updated active token count
        let active_count = TOKEN_TRACKING.len();
        logger.log(format!("📊 Active tokens count after cleanup: {}", active_count).blue().to_string());
    }
    
    Ok(is_fully_sold)
}

/// Periodic comprehensive cleanup of all tracked tokens
async fn periodic_comprehensive_cleanup(app_state: Arc<AppState>, logger: &Logger) {
    logger.log("🔄 Starting periodic comprehensive cleanup...".blue().to_string());
    
    // Get all tokens from both tracking systems
    let tokens_to_check: HashSet<String> = BOUGHT_TOKEN_LIST.iter()
        .map(|entry| entry.key().clone())
        .chain(TOKEN_TRACKING.iter().map(|entry| entry.key().clone()))
        .collect();
    
    if tokens_to_check.is_empty() {
        logger.log("📝 No tokens to check during periodic cleanup".blue().to_string());
        return;
    }
    
    logger.log(format!("🔍 Checking {} tokens during periodic cleanup", tokens_to_check.len()));
    
    let mut cleaned_count = 0;
    
    for token_mint in tokens_to_check {
        match verify_sell_transaction_and_cleanup(
            &token_mint,
            None,
            app_state.clone(),
            logger,
        ).await {
            Ok(cleaned_up) => {
                if cleaned_up {
                    cleaned_count += 1;
                    logger.log(format!("🧹 Periodic cleanup removed: {}", token_mint));
                }
            },
            Err(e) => {
                logger.log(format!("⚠️  Periodic cleanup error for {}: {}", token_mint, e).yellow().to_string());
            }
        }
        
        // Small delay between checks to avoid overwhelming RPC
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    
    // Also cleanup selling strategy tokens
    use crate::engine::selling_strategy::TokenManager;
    let token_manager = TokenManager::new();
    match token_manager.verify_and_cleanup_sold_tokens(&app_state).await {
        Ok(selling_cleaned) => {
            if selling_cleaned > 0 {
                logger.log(format!("🧹 Selling strategy cleanup removed {} additional tokens", selling_cleaned));
            }
        },
        Err(e) => {
            logger.log(format!("⚠️  Error in selling strategy cleanup: {}", e).yellow().to_string());
        }
    }

    let final_active_count = TOKEN_TRACKING.len();
    logger.log(format!(
        "✅ Periodic cleanup completed. Removed: {}, Active tokens remaining: {}", 
        cleaned_count, 
        final_active_count
    ).green().to_string());
}

/// Manually trigger comprehensive cleanup of all tracking systems
/// Useful for debugging and maintenance
pub async fn trigger_comprehensive_cleanup(app_state: Arc<AppState>) -> Result<(usize, usize), String> {
    let logger = Logger::new("[MANUAL-CLEANUP] => ".magenta().bold().to_string());
    logger.log("🔧 Manual comprehensive cleanup triggered".to_string());
    
    // First, run the copy trading cleanup
    let copy_trading_cleaned = match verify_sell_transaction_and_cleanup(
        "",  // Empty string will be ignored in the function
        None,
        app_state.clone(),
        &logger,
    ).await {
        Ok(_) => {
            // Run periodic cleanup for all tokens
            let tokens_to_check: HashSet<String> = BOUGHT_TOKEN_LIST.iter()
                .map(|entry| entry.key().clone())
                .chain(TOKEN_TRACKING.iter().map(|entry| entry.key().clone()))
                .collect();
            
            let mut cleaned = 0;
            for token_mint in tokens_to_check {
                match verify_sell_transaction_and_cleanup(
                    &token_mint,
                    None,
                    app_state.clone(),
                    &logger,
                ).await {
                    Ok(was_cleaned) => {
                        if was_cleaned {
                            cleaned += 1;
                        }
                    },
                    Err(_) => {}
                }
            }
            cleaned
        },
        Err(_) => 0,
    };
    
    // Then run selling strategy cleanup
    use crate::engine::selling_strategy::TokenManager;
    let token_manager = TokenManager::new();
    let selling_strategy_cleaned = match token_manager.verify_and_cleanup_sold_tokens(&app_state).await {
        Ok(cleaned) => cleaned,
        Err(e) => {
            logger.log(format!("Error in selling strategy cleanup: {}", e).red().to_string());
            0
        }
    };
    
    let final_active_count = TOKEN_TRACKING.len();
    logger.log(format!(
        "🔧 Manual cleanup completed. Copy trading: {}, Selling strategy: {}, Active remaining: {}", 
        copy_trading_cleaned,
        selling_strategy_cleaned,
        final_active_count
    ).magenta().to_string());
    
    Ok((copy_trading_cleaned, selling_strategy_cleaned))
}

