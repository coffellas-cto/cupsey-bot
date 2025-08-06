use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::str::FromStr;
use anchor_client::solana_sdk::signature::Signature;
use anchor_client::solana_client::nonblocking::rpc_client::RpcClient;
use anchor_client::solana_sdk::transaction::Transaction;
use colored::Colorize;
use tokio::time::timeout;
use crate::common::{config::AppState, logger::Logger};
use bloom::{BloomFilter, ASMS};
use lazy_static::lazy_static;
use std::sync::Mutex;
use solana_transaction_status::TransactionConfirmationStatus;

lazy_static! {
    /// Global bloom filter for signature deduplication
    static ref SIGNATURE_BLOOM: Mutex<BloomFilter> = Mutex::new(BloomFilter::with_rate(0.1, 100000));
}

/// Circuit breaker for transaction verification with failure tracking
pub struct TransactionCircuitBreaker {
    failure_count: AtomicU32,
    last_failure: AtomicU64,
    threshold: u32,
    reset_timeout: Duration,
}

impl TransactionCircuitBreaker {
    pub fn new(threshold: u32, reset_timeout: Duration) -> Self {
        Self {
            failure_count: AtomicU32::new(0),
            last_failure: AtomicU64::new(0),
            threshold,
            reset_timeout,
        }
    }

    /// Check if the circuit breaker is open (should reject requests)
    pub fn is_open(&self) -> bool {
        let current_failures = self.failure_count.load(Ordering::SeqCst);
        if current_failures >= self.threshold {
            let last_failure = self.last_failure.load(Ordering::SeqCst);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            
            // Check if we should reset the circuit breaker
            if now.saturating_sub(last_failure) > self.reset_timeout.as_millis() as u64 {
                self.reset();
                false
            } else {
                true
            }
        } else {
            false
        }
    }

    /// Record a failure
    pub fn record_failure(&self) {
        self.failure_count.fetch_add(1, Ordering::SeqCst);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_failure.store(now, Ordering::SeqCst);
    }

    /// Record a success
    pub fn record_success(&self) {
        self.failure_count.store(0, Ordering::SeqCst);
    }

    /// Reset the circuit breaker
    fn reset(&self) {
        self.failure_count.store(0, Ordering::SeqCst);
        self.last_failure.store(0, Ordering::SeqCst);
    }
}

/// Connection pool with health monitoring for RPC clients
pub struct RpcConnectionPool {
    clients: Vec<Arc<RpcClient>>,
    current_index: AtomicU32,
    health_check_interval: Duration,
    logger: Logger,
}

impl RpcConnectionPool {
    pub fn new(rpc_urls: Vec<String>, health_check_interval: Duration) -> Self {
        let clients: Vec<Arc<RpcClient>> = rpc_urls
            .into_iter()
            .map(|url| {
                Arc::new(RpcClient::new_with_timeout_and_commitment(
                    url,
                    Duration::from_secs(5),
                    anchor_client::solana_sdk::commitment_config::CommitmentConfig::processed(),
                ))
            })
            .collect();

        Self {
            clients,
            current_index: AtomicU32::new(0),
            health_check_interval,
            logger: Logger::new("[RPC-POOL] => ".cyan().to_string()),
        }
    }

    /// Get the next available healthy client using round-robin
    pub async fn get_client(&self) -> Option<Arc<RpcClient>> {
        if self.clients.is_empty() {
            return None;
        }

        let start_index = self.current_index.load(Ordering::SeqCst) as usize % self.clients.len();
        
        // Try each client starting from current index
        for i in 0..self.clients.len() {
            let index = (start_index + i) % self.clients.len();
            let client = &self.clients[index];
            
            // Simple health check - try to get latest blockhash with short timeout
            if let Ok(_) = timeout(Duration::from_millis(500), client.get_latest_blockhash()).await {
                self.current_index.store((index + 1) as u32, Ordering::SeqCst);
                return Some(client.clone());
            }
        }

        // If no healthy clients, return the first one as fallback
        Some(self.clients[0].clone())
    }

    /// Start background health monitoring
    pub async fn start_health_monitoring(self: Arc<Self>) {
        let pool = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(pool.health_check_interval);
            loop {
                interval.tick().await;
                pool.perform_health_check().await;
            }
        });
    }

    async fn perform_health_check(&self) {
        for (i, client) in self.clients.iter().enumerate() {
            match timeout(Duration::from_millis(1000), client.get_latest_blockhash()).await {
                Ok(_) => {
                    // Client is healthy
                }
                Err(_) => {
                    self.logger.log(format!("RPC client {} is unhealthy", i).yellow().to_string());
                }
            }
        }
    }
}

/// Check if signature was already processed using bloom filter
pub fn is_signature_processed(signature: &str) -> bool {
    let mut bloom = SIGNATURE_BLOOM.lock().unwrap();
    bloom.contains(signature)
}

/// Mark signature as processed in bloom filter
pub fn mark_signature_processed(signature: &str) {
    let mut bloom = SIGNATURE_BLOOM.lock().unwrap();
    bloom.insert(signature);
}

/// Optimized transaction verification with circuit breaker and bloom filter
pub async fn verify_transaction_optimized(
    signature_str: &str,
    app_state: Arc<AppState>,
    circuit_breaker: &TransactionCircuitBreaker,
    logger: &Logger,
) -> Result<bool, String> {
    // Check circuit breaker state
    if circuit_breaker.is_open() {
        logger.log("Circuit breaker open - skipping verification".yellow().to_string());
        return Err("Circuit breaker open - skipping verification".to_string());
    }

    // Check bloom filter for duplicate signatures
    if is_signature_processed(signature_str) {
        logger.log(format!("Signature {} already processed (bloom filter)", signature_str).blue().to_string());
        return Ok(true);
    }

    // Parse signature
    let signature = match Signature::from_str(signature_str) {
        Ok(sig) => sig,
        Err(e) => {
            circuit_breaker.record_failure();
            return Err(format!("Invalid signature: {}", e));
        }
    };
    
    // Single RPC call with shorter timeout
    match timeout(Duration::from_millis(100), async {
        app_state.rpc_nonblocking_client.get_signature_statuses(&[signature]).await
    }).await {
        Ok(Ok(result)) => {
            if let Some(status_opt) = result.value.get(0) {
                if let Some(status) = status_opt {
                    if status.err.is_some() {
                        circuit_breaker.record_failure();
                        return Err(format!("Transaction failed: {:?}", status.err));
                    } else if let Some(conf_status) = &status.confirmation_status {
                        if matches!(conf_status, 
                            TransactionConfirmationStatus::Finalized | 
                            TransactionConfirmationStatus::Confirmed) {
                            circuit_breaker.record_success();
                            mark_signature_processed(signature_str);
                            return Ok(true);
                        }
                    }
                }
            }
            circuit_breaker.record_failure();
            Err("Transaction not confirmed".to_string())
        },
        Ok(Err(e)) => {
            circuit_breaker.record_failure();
            Err(format!("RPC error: {}", e))
        },
        Err(_) => {
            circuit_breaker.record_failure();
            Err("Verification timeout".to_string())
        }
    }
}

/// Parallel processing utilities for improved performance
pub struct ParallelDexBuilder {
    logger: Logger,
}

impl ParallelDexBuilder {
    pub fn new() -> Self {
        Self {
            logger: Logger::new("[PARALLEL-DEX] => ".green().to_string()),
        }
    }

    /// Optimize instruction building with parallel processing patterns
    pub async fn optimize_instruction_building(&self, token_mint: &str) -> Result<(), String> {
        self.logger.log(format!("Optimizing instruction building for token: {}", token_mint));
        
        // This is a placeholder for future parallel instruction building optimizations
        // The actual DEX instruction building is handled by the existing modules
        // This structure allows for easy extension when parallel building is needed
        
        self.logger.log("Parallel optimization patterns ready".to_string());
        Ok(())
    }

    /// Batch process multiple token operations
    pub async fn batch_process_tokens(&self, tokens: Vec<String>) -> Result<(), String> {
        self.logger.log(format!("Batch processing {} tokens", tokens.len()));
        
        // Process tokens in parallel batches
        let batch_size = 10;
        for chunk in tokens.chunks(batch_size) {
            let tasks: Vec<_> = chunk.iter().map(|token| {
                let token = token.clone();
                let logger = self.logger.clone();
                tokio::spawn(async move {
                    logger.log(format!("Processing token: {}", token));
                    // Token processing logic would go here
                    Ok::<(), String>(())
                })
            }).collect();

            // Wait for all tasks in this batch to complete
            for task in tasks {
                if let Err(e) = task.await {
                    self.logger.log(format!("Task error: {}", e).red().to_string());
                }
            }
        }

        self.logger.log("Batch processing completed".to_string());
        Ok(())
    }
} 