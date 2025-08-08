use std::sync::Arc;
use anyhow::{anyhow, Result};
use anchor_client::solana_sdk::instruction::Instruction;

use crate::common::cache::{CachedSellTransaction, TRANSACTION_CACHE};
use crate::common::logger::Logger;
use crate::common::config::{AppState, SwapConfig};
use crate::engine::swap::SwapInType;
use crate::engine::swap::{SwapDirection, SwapProtocol};
use crate::engine::transaction_parser::TradeInfoFromToken;

/// Transaction cache manager for pre-building selling transactions
pub struct TransactionCacheManager {
    app_state: Arc<AppState>,
    logger: Logger,
}

impl TransactionCacheManager {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self {
            app_state,
            logger: Logger::new("TxCache".to_string()),
        }
    }
    
    /// Pre-build and cache a selling transaction for a token
    /// Uses high slippage tolerance (10%) to ensure transaction validity even with price movement
    pub async fn cache_sell_transaction(
        &self,
        token_mint: &str,
        trade_info: &TradeInfoFromToken,
        protocol: SwapProtocol,
    ) -> Result<()> {
        self.logger.log(format!("🔄 Pre-building sell transaction for token: {}", token_mint));
        
        // Create high-slippage sell config for cached transactions
        let mut sell_config = SwapConfig {
            swap_direction: SwapDirection::Sell,
            amount_in: 1.0, // 100% of tokens (full sell)
            in_type: SwapInType::Pct,
            slippage: 1000, // 10% slippage tolerance for cached transactions
            ..Default::default()
        };
        
        // Build transaction based on protocol
        let result = match protocol {
            SwapProtocol::PumpFun => {
                self.build_pumpfun_sell_transaction(trade_info, sell_config.clone()).await
            },
            SwapProtocol::PumpSwap => {
                self.build_pumpswap_sell_transaction(trade_info, sell_config.clone()).await
            },
            SwapProtocol::RaydiumLaunchpad => {
                self.build_raydium_sell_transaction(trade_info, sell_config.clone()).await
            },
            _ => {
                return Err(anyhow!("Unsupported protocol for transaction caching: {:?}", protocol));
            }
        };
        
        match result {
            Ok((keypair, instructions, price)) => {
                // Create cached transaction
                let cached_tx = CachedSellTransaction::new(
                    instructions,
                    keypair,
                    token_mint.to_string(),
                    trade_info.pool_id.clone(),
                    format!("{:?}", protocol),
                    price,
                    sell_config.slippage as u64,
                    "full".to_string(),
                );
                
                // Store in cache
                TRANSACTION_CACHE.insert(token_mint.to_string(), cached_tx, None);
                
                self.logger.log(format!(
                    "✅ Cached sell transaction for {} (Protocol: {:?}, Price: {:.8}, Slippage: {}%)",
                    token_mint, protocol, price, sell_config.slippage as f64 / 100.0
                ));
                
                Ok(())
            },
            Err(e) => {
                self.logger.log(format!(
                    "❌ Failed to cache sell transaction for {}: {}",
                    token_mint, e
                ));
                Err(e)
            }
        }
    }
    
    /// Get cached transaction for quick selling
    pub fn get_cached_transaction(&self, token_mint: &str) -> Option<CachedSellTransaction> {
        let cached_tx = TRANSACTION_CACHE.get(token_mint);
        
        if let Some(ref tx) = cached_tx {
            self.logger.log(format!(
                "🚀 Using cached sell transaction for {} (Age: {}s, Protocol: {})",
                token_mint, tx.age_seconds(), tx.protocol
            ));
        }
        
        cached_tx
    }
    
    /// Execute cached transaction with current blockhash
    pub async fn execute_cached_sell(
        &self,
        token_mint: &str,
        use_zeroslot: bool,
    ) -> Result<Vec<String>> {
        let cached_tx = match self.get_cached_transaction(token_mint) {
            Some(tx) => tx,
            None => return Err(anyhow!("No cached transaction found for token: {}", token_mint)),
        };
        
        self.logger.log(format!(
            "⚡ Executing cached sell transaction for {} via {}",
            token_mint,
            if use_zeroslot { "ZeroSlot" } else { "Normal RPC" }
        ));
        
        // Get fresh blockhash
        let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
            Some(hash) => hash,
            None => return Err(anyhow!("Failed to get recent blockhash")),
        };
        
        // Execute transaction
        let result = if use_zeroslot {
            crate::core::tx::new_signed_and_send_zeroslot(
                self.app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &cached_tx.keypair,
                cached_tx.instructions,
                &self.logger,
            ).await
        } else {
            crate::core::tx::new_signed_and_send_normal(
                self.app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &cached_tx.keypair,
                cached_tx.instructions,
                &self.logger,
            ).await
        };
        
        match result {
            Ok(signatures) => {
                self.logger.log(format!(
                    "✅ Cached sell transaction executed successfully for {}: {:?}",
                    token_mint, signatures
                ));
                
                // Remove from cache after successful execution
                TRANSACTION_CACHE.remove(token_mint);
                
                Ok(signatures)
            },
            Err(e) => {
                self.logger.log(format!(
                    "❌ Cached sell transaction failed for {}: {}",
                    token_mint, e
                ));
                
                // Remove failed transaction from cache
                TRANSACTION_CACHE.remove(token_mint);
                
                Err(e)
            }
        }
    }
    
    /// Check if token has a valid cached transaction
    pub fn has_cached_transaction(&self, token_mint: &str) -> bool {
        TRANSACTION_CACHE.has_cached_transaction(token_mint)
    }
    
    /// Refresh cache TTL for active tokens
    pub fn refresh_cache(&self, token_mint: &str) -> bool {
        TRANSACTION_CACHE.refresh(token_mint)
    }
    
    /// Clear expired transactions from cache
    pub fn cleanup_expired(&self) {
        let before_size = TRANSACTION_CACHE.size();
        TRANSACTION_CACHE.clear_expired();
        let after_size = TRANSACTION_CACHE.size();
        
        if before_size != after_size {
            self.logger.log(format!(
                "🧹 Cleaned up {} expired cached transactions ({} -> {})",
                before_size - after_size, before_size, after_size
            ));
        }
    }
    
    /// Get cache statistics
    pub fn get_cache_stats(&self) -> (usize, Vec<String>) {
        let size = TRANSACTION_CACHE.size();
        let tokens = TRANSACTION_CACHE.get_cached_tokens();
        (size, tokens)
    }
    
    // Private helper methods for building transactions by protocol
    
    async fn build_pumpfun_sell_transaction(
        &self,
        trade_info: &TradeInfoFromToken,
        sell_config: SwapConfig,
    ) -> Result<(Arc<anchor_client::solana_sdk::signature::Keypair>, Vec<Instruction>, f64)> {
        let pump = crate::dex::pump_fun::Pump::new(
            self.app_state.rpc_nonblocking_client.clone(),
            self.app_state.rpc_client.clone(),
            self.app_state.wallet.clone(),
        );
        
        pump.build_swap_from_parsed_data(trade_info, sell_config).await
    }
    
    async fn build_pumpswap_sell_transaction(
        &self,
        trade_info: &TradeInfoFromToken,
        sell_config: SwapConfig,
    ) -> Result<(Arc<anchor_client::solana_sdk::signature::Keypair>, Vec<Instruction>, f64)> {
        let pump_swap = crate::dex::pump_swap::PumpSwap::new(
            self.app_state.wallet.clone(),
            Some(self.app_state.rpc_client.clone()),
            Some(self.app_state.rpc_nonblocking_client.clone()),
        );
        
        pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await
    }
    
    async fn build_raydium_sell_transaction(
        &self,
        trade_info: &TradeInfoFromToken,
        sell_config: SwapConfig,
    ) -> Result<(Arc<anchor_client::solana_sdk::signature::Keypair>, Vec<Instruction>, f64)> {
        let raydium = crate::dex::raydium_launchpad::Raydium::new(
            self.app_state.wallet.clone(),
            Some(self.app_state.rpc_client.clone()),
            Some(self.app_state.rpc_nonblocking_client.clone()),
        );
        
        raydium.build_swap_from_parsed_data(trade_info, sell_config).await
    }
} 