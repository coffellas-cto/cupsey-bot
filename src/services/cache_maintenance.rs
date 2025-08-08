use std::time::Duration;
use tokio::time;
use colored::Colorize;

use crate::common::logger::Logger;
use crate::common::cache::{
    TOKEN_ACCOUNT_CACHE, TOKEN_MINT_CACHE, WALLET_TOKEN_ACCOUNTS, 
    TARGET_WALLET_TOKENS, TRANSACTION_CACHE
};

/// Cache maintenance service that periodically cleans up expired entries
pub struct CacheMaintenanceService {
    logger: Logger,
    cleanup_interval: Duration,
}

impl CacheMaintenanceService {
    pub fn new(cleanup_interval_secs: u64) -> Self {
        Self {
            logger: Logger::new("CacheMaintenance".to_string()),
            cleanup_interval: Duration::from_secs(cleanup_interval_secs),
        }
    }
    
    /// Start the cache maintenance loop
    pub async fn start(&self) {
        let mut interval = time::interval(self.cleanup_interval);
        
        self.logger.log(format!(
            "🧹 Starting cache maintenance service (interval: {}s)",
            self.cleanup_interval.as_secs()
        ));
        
        loop {
            interval.tick().await;
            self.perform_maintenance().await;
        }
    }
    
    /// Perform cache maintenance tasks
    async fn perform_maintenance(&self) {
        let start_time = std::time::Instant::now();
        
        // Track cache sizes before cleanup
        let token_account_size_before = TOKEN_ACCOUNT_CACHE.size();
        let token_mint_size_before = TOKEN_MINT_CACHE.size();
        let transaction_cache_size_before = TRANSACTION_CACHE.size();
        let wallet_accounts_size = WALLET_TOKEN_ACCOUNTS.size();
        let target_tokens_size = TARGET_WALLET_TOKENS.size();
        
        // Clean up expired entries
        TOKEN_ACCOUNT_CACHE.clear_expired();
        TOKEN_MINT_CACHE.clear_expired();
        TRANSACTION_CACHE.clear_expired();
        
        // Track cache sizes after cleanup
        let token_account_size_after = TOKEN_ACCOUNT_CACHE.size();
        let token_mint_size_after = TOKEN_MINT_CACHE.size();
        let transaction_cache_size_after = TRANSACTION_CACHE.size();
        
        let maintenance_duration = start_time.elapsed();
        
        // Log cleanup results
        self.logger.log(format!(
            "🧹 Cache maintenance completed in {:?}ms",
            maintenance_duration.as_millis()
        ));
        
        // Log detailed statistics
        if token_account_size_before != token_account_size_after ||
           token_mint_size_before != token_mint_size_after ||
           transaction_cache_size_before != transaction_cache_size_after {
            
            self.logger.log(format!(
                "📊 Cache cleanup stats:\n\
                 • Token Accounts: {} → {} ({})\n\
                 • Token Mints: {} → {} ({})\n\
                 • Transaction Cache: {} → {} ({})\n\
                 • Wallet Accounts: {} (active)\n\
                 • Target Tokens: {} (tracking)",
                token_account_size_before, token_account_size_after, 
                if token_account_size_before > token_account_size_after { 
                    format!("-{}", token_account_size_before - token_account_size_after) 
                } else { "0".to_string() },
                token_mint_size_before, token_mint_size_after,
                if token_mint_size_before > token_mint_size_after { 
                    format!("-{}", token_mint_size_before - token_mint_size_after) 
                } else { "0".to_string() },
                transaction_cache_size_before, transaction_cache_size_after,
                if transaction_cache_size_before > transaction_cache_size_after { 
                    format!("-{}", transaction_cache_size_before - transaction_cache_size_after) 
                } else { "0".to_string() },
                wallet_accounts_size,
                target_tokens_size
            ));
        }
        
        // Log transaction cache details if there are cached transactions
        if transaction_cache_size_after > 0 {
            let cached_tokens = TRANSACTION_CACHE.get_cached_tokens();
            self.logger.log(format!(
                "🎯 Active cached transactions ({}): {:?}",
                transaction_cache_size_after,
                cached_tokens.iter().take(5).collect::<Vec<_>>() // Show first 5 tokens
            ));
        }
        
        // Alert if caches are growing too large
        let total_cache_size = token_account_size_after + token_mint_size_after + transaction_cache_size_after;
        if total_cache_size > 1000 {
            self.logger.log(format!(
                "⚠️ Large cache size detected: {} total entries",
                total_cache_size
            ));
        }
    }
    
    /// Get current cache statistics
    pub fn get_cache_stats(&self) -> CacheStats {
        CacheStats {
            token_account_cache_size: TOKEN_ACCOUNT_CACHE.size(),
            token_mint_cache_size: TOKEN_MINT_CACHE.size(),
            transaction_cache_size: TRANSACTION_CACHE.size(),
            wallet_token_accounts_size: WALLET_TOKEN_ACCOUNTS.size(),
            target_wallet_tokens_size: TARGET_WALLET_TOKENS.size(),
            cached_transaction_tokens: TRANSACTION_CACHE.get_cached_tokens(),
        }
    }
    
    /// Force immediate cache cleanup
    pub async fn force_cleanup(&self) {
        self.logger.log("🔧 Forcing immediate cache cleanup".to_string());
        self.perform_maintenance().await;
    }
}

/// Cache statistics structure
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub token_account_cache_size: usize,
    pub token_mint_cache_size: usize,
    pub transaction_cache_size: usize,
    pub wallet_token_accounts_size: usize,
    pub target_wallet_tokens_size: usize,
    pub cached_transaction_tokens: Vec<String>,
}

impl CacheStats {
    pub fn total_cache_entries(&self) -> usize {
        self.token_account_cache_size + 
        self.token_mint_cache_size + 
        self.transaction_cache_size
    }
    
    pub fn has_cached_transactions(&self) -> bool {
        self.transaction_cache_size > 0
    }
} 