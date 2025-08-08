use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::time::{Duration, Instant};
use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::solana_sdk::instruction::Instruction;
use anchor_client::solana_sdk::signature::Keypair;
use spl_token_2022::state::{Account, Mint};
use spl_token_2022::extension::StateWithExtensionsOwned;
use lazy_static::lazy_static;

/// TTL Cache entry that stores a value with an expiration time
pub struct CacheEntry<T> {
    pub value: T,
    pub expires_at: Instant,
}

impl<T> CacheEntry<T> {
    pub fn new(value: T, ttl_seconds: u64) -> Self {
        Self {
            value,
            expires_at: Instant::now() + Duration::from_secs(ttl_seconds),
        }
    }
    
    pub fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }
}

/// Token account cache
pub struct TokenAccountCache {
    accounts: RwLock<HashMap<Pubkey, CacheEntry<StateWithExtensionsOwned<Account>>>>,
    default_ttl: u64,
}

impl TokenAccountCache {
    pub fn new(default_ttl: u64) -> Self {
        Self {
            accounts: RwLock::new(HashMap::new()),
            default_ttl,
        }
    }
    
    pub fn get(&self, key: &Pubkey) -> Option<StateWithExtensionsOwned<Account>> {
        let accounts = self.accounts.read().unwrap();
        if let Some(entry) = accounts.get(key) {
            if !entry.is_expired() {
                return Some(entry.value.clone());
            }
        }
        None
    }
    
    pub fn insert(&self, key: Pubkey, value: StateWithExtensionsOwned<Account>, ttl: Option<u64>) {
        let ttl = ttl.unwrap_or(self.default_ttl);
        let mut accounts = self.accounts.write().unwrap();
        accounts.insert(key, CacheEntry::new(value, ttl));
    }
    
    pub fn remove(&self, key: &Pubkey) {
        let mut accounts = self.accounts.write().unwrap();
        accounts.remove(key);
    }
    
    pub fn clear_expired(&self) {
        let mut accounts = self.accounts.write().unwrap();
        accounts.retain(|_, entry| !entry.is_expired());
    }
    
    // Get the current size of the cache
    pub fn size(&self) -> usize {
        let accounts = self.accounts.read().unwrap();
        accounts.len()
    }
}

/// Token mint cache
pub struct TokenMintCache {
    mints: RwLock<HashMap<Pubkey, CacheEntry<StateWithExtensionsOwned<Mint>>>>,
    default_ttl: u64,
}

impl TokenMintCache {
    pub fn new(default_ttl: u64) -> Self {
        Self {
            mints: RwLock::new(HashMap::new()),
            default_ttl,
        }
    }
    
    pub fn get(&self, key: &Pubkey) -> Option<StateWithExtensionsOwned<Mint>> {
        let mints = self.mints.read().unwrap();
        if let Some(entry) = mints.get(key) {
            if !entry.is_expired() {
                return Some(entry.value.clone());
            }
        }
        None
    }
    
    pub fn insert(&self, key: Pubkey, value: StateWithExtensionsOwned<Mint>, ttl: Option<u64>) {
        let ttl = ttl.unwrap_or(self.default_ttl);
        let mut mints = self.mints.write().unwrap();
        mints.insert(key, CacheEntry::new(value, ttl));
    }
    
    pub fn remove(&self, key: &Pubkey) {
        let mut mints = self.mints.write().unwrap();
        mints.remove(key);
    }
    
    pub fn clear_expired(&self) {
        let mut mints = self.mints.write().unwrap();
        mints.retain(|_, entry| !entry.is_expired());
    }
    
    // Get the current size of the cache
    pub fn size(&self) -> usize {
        let mints = self.mints.read().unwrap();
        mints.len()
    }
}

// PumpSwap pool cache removed - now using TradeInfoFromToken directly

/// Simple wallet token account tracker
pub struct WalletTokenAccounts {
    accounts: RwLock<HashSet<Pubkey>>,
}

impl WalletTokenAccounts {
    pub fn new() -> Self {
        Self {
            accounts: RwLock::new(HashSet::new()),
        }
    }
    
    pub fn contains(&self, account: &Pubkey) -> bool {
        let accounts = self.accounts.read().unwrap();
        accounts.contains(account)
    }
    
    pub fn insert(&self, account: Pubkey) -> bool {
        let mut accounts = self.accounts.write().unwrap();
        accounts.insert(account)
    }
    
    pub fn remove(&self, account: &Pubkey) -> bool {
        let mut accounts = self.accounts.write().unwrap();
        accounts.remove(account)
    }
    
    pub fn get_all(&self) -> HashSet<Pubkey> {
        let accounts = self.accounts.read().unwrap();
        accounts.clone()
    }
    
    pub fn clear(&self) {
        let mut accounts = self.accounts.write().unwrap();
        accounts.clear();
    }
    
    pub fn size(&self) -> usize {
        let accounts = self.accounts.read().unwrap();
        accounts.len()
    }
}

/// Target wallet token list tracker
pub struct TargetWalletTokens {
    tokens: RwLock<HashSet<String>>,
}

impl TargetWalletTokens {
    pub fn new() -> Self {
        Self {
            tokens: RwLock::new(HashSet::new()),
        }
    }
    
    pub fn contains(&self, token_mint: &str) -> bool {
        let tokens = self.tokens.read().unwrap();
        tokens.contains(token_mint)
    }
    
    pub fn insert(&self, token_mint: String) -> bool {
        let mut tokens = self.tokens.write().unwrap();
        tokens.insert(token_mint)
    }
    
    pub fn remove(&self, token_mint: &str) -> bool {
        let mut tokens = self.tokens.write().unwrap();
        tokens.remove(token_mint)
    }
    
    pub fn get_all(&self) -> HashSet<String> {
        let tokens = self.tokens.read().unwrap();
        tokens.clone()
    }
    
    pub fn clear(&self) {
        let mut tokens = self.tokens.write().unwrap();
        tokens.clear();
    }
    
    pub fn size(&self) -> usize {
        let tokens = self.tokens.read().unwrap();
        tokens.len()
    }
}

/// Cached transaction for quick selling
#[derive(Clone)]
pub struct CachedSellTransaction {
    pub instructions: Vec<Instruction>,
    pub keypair: Keypair,
    pub token_mint: String,
    pub pool_id: String,
    pub protocol: String, // "PumpFun", "PumpSwap", "RaydiumLaunchpad"
    pub expected_price: f64,
    pub slippage_tolerance: u64, // in basis points (e.g., 1000 = 10%)
    pub created_at: Instant,
    pub amount_type: String, // "full" or "percentage"
}

impl CachedSellTransaction {
    pub fn new(
        instructions: Vec<Instruction>,
        keypair: Keypair,
        token_mint: String,
        pool_id: String,
        protocol: String,
        expected_price: f64,
        slippage_tolerance: u64,
        amount_type: String,
    ) -> Self {
        Self {
            instructions,
            keypair,
            token_mint,
            pool_id,
            protocol,
            expected_price,
            slippage_tolerance,
            created_at: Instant::now(),
            amount_type,
        }
    }
    
    pub fn is_expired(&self, ttl_seconds: u64) -> bool {
        self.created_at.elapsed().as_secs() > ttl_seconds
    }
    
    pub fn age_seconds(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }
}

/// Transaction cache for pre-built selling transactions
pub struct TransactionCache {
    transactions: RwLock<HashMap<String, CacheEntry<CachedSellTransaction>>>,
    default_ttl: u64,
}

impl TransactionCache {
    pub fn new(default_ttl: u64) -> Self {
        Self {
            transactions: RwLock::new(HashMap::new()),
            default_ttl,
        }
    }
    
    /// Get cached transaction by token mint
    pub fn get(&self, token_mint: &str) -> Option<CachedSellTransaction> {
        let transactions = self.transactions.read().unwrap();
        if let Some(entry) = transactions.get(token_mint) {
            if !entry.is_expired() {
                return Some(entry.value.clone());
            }
        }
        None
    }
    
    /// Insert transaction into cache
    pub fn insert(&self, token_mint: String, transaction: CachedSellTransaction, ttl: Option<u64>) {
        let ttl = ttl.unwrap_or(self.default_ttl);
        let mut transactions = self.transactions.write().unwrap();
        transactions.insert(token_mint, CacheEntry::new(transaction, ttl));
    }
    
    /// Remove transaction from cache
    pub fn remove(&self, token_mint: &str) {
        let mut transactions = self.transactions.write().unwrap();
        transactions.remove(token_mint);
    }
    
    /// Clear expired transactions
    pub fn clear_expired(&self) {
        let mut transactions = self.transactions.write().unwrap();
        transactions.retain(|_, entry| !entry.is_expired());
    }
    
    /// Get cache size
    pub fn size(&self) -> usize {
        let transactions = self.transactions.read().unwrap();
        transactions.len()
    }
    
    /// Get all cached token mints
    pub fn get_cached_tokens(&self) -> Vec<String> {
        let transactions = self.transactions.read().unwrap();
        transactions.keys().cloned().collect()
    }
    
    /// Check if token has cached transaction
    pub fn has_cached_transaction(&self, token_mint: &str) -> bool {
        let transactions = self.transactions.read().unwrap();
        if let Some(entry) = transactions.get(token_mint) {
            !entry.is_expired()
        } else {
            false
        }
    }
    
    /// Update transaction timestamp (refresh TTL)
    pub fn refresh(&self, token_mint: &str) -> bool {
        let mut transactions = self.transactions.write().unwrap();
        if let Some(entry) = transactions.get_mut(token_mint) {
            if !entry.is_expired() {
                entry.expires_at = Instant::now() + Duration::from_secs(self.default_ttl);
                return true;
            }
        }
        false
    }
}

// Global cache instances with reasonable TTL values
lazy_static! {
    pub static ref TOKEN_ACCOUNT_CACHE: TokenAccountCache = TokenAccountCache::new(60); // 60 seconds TTL
    pub static ref TOKEN_MINT_CACHE: TokenMintCache = TokenMintCache::new(300); // 5 minutes TTL
    pub static ref WALLET_TOKEN_ACCOUNTS: WalletTokenAccounts = WalletTokenAccounts::new();
    pub static ref TARGET_WALLET_TOKENS: TargetWalletTokens = TargetWalletTokens::new();
    pub static ref TRANSACTION_CACHE: TransactionCache = TransactionCache::new(30); // 30 seconds TTL for pre-built transactions
} 