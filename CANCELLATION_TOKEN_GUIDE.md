# CancellationToken with gRPC Streams - Complete Guide

This guide demonstrates how to use `tokio_util::sync::CancellationToken` with gRPC streams for building robust, cancellable async applications.

## 🎯 Key Concepts

### What is CancellationToken?

`CancellationToken` is a cooperative cancellation primitive that allows you to:
- **Cancel long-running tasks gracefully**
- **Coordinate shutdown across multiple async tasks**
- **Handle timeouts and conditional cancellation**
- **Implement clean resource cleanup**

### Why Use CancellationToken with gRPC?
gRPC streams are long-lived connections that need proper lifecycle management:
- **Graceful Shutdown**: Cleanly close connections when shutting down
- **Resource Management**: Prevent memory leaks from abandoned tasks
- **Error Recovery**: Cancel and restart failed connections
- **Dynamic Control**: Stop/start monitoring based on business logic

## 🔧 Core Patterns

### 1. Basic Cancellation Pattern

```rust
use tokio_util::sync::CancellationToken;
use tokio::select;

async fn cancellable_task(cancellation_token: CancellationToken) -> Result<()> {
    loop {
        select! {
            // Handle cancellation
            _ = cancellation_token.cancelled() => {
                println!("Task was cancelled!");
                break;
            }
            
            // Do actual work
            result = do_work() => {
                match result {
                    Ok(_) => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }
    
    // Cleanup code here
    Ok(())
}
```

### 2. Global Task Registry Pattern

```rust
use dashmap::DashMap;
use std::sync::Arc;

lazy_static::lazy_static! {
    static ref ACTIVE_TASKS: Arc<DashMap<String, CancellationToken>> = Arc::new(DashMap::new());
}

// Register a task
let token = CancellationToken::new();
ACTIVE_TASKS.insert("my_task".to_string(), token.clone());

// Cancel a specific task
if let Some((_, token)) = ACTIVE_TASKS.remove("my_task") {
    token.cancel();
}

// Cancel all tasks
for entry in ACTIVE_TASKS.iter() {
    entry.value().cancel();
}
ACTIVE_TASKS.clear();
```

### 3. gRPC Stream with Cancellation

```rust
async fn cancellable_grpc_stream(
    token: CancellationToken,
    mut stream: impl Stream<Item = Result<Message, Status>> + Unpin,
) -> Result<()> {
    loop {
        select! {
            _ = token.cancelled() => {
                println!("Stream cancelled");
                break;
            }
            
            msg = stream.next() => {
                match msg {
                    Some(Ok(message)) => {
                        // Process message
                        process_message(message).await?;
                    }
                    Some(Err(e)) => {
                        return Err(e.into());
                    }
                    None => {
                        println!("Stream ended");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}
```

## 🚀 Usage Examples

### Example 1: Basic gRPC Monitoring with Manual Cancellation

```rust
use crate::token_cancelation_pipeline::{TokenCancellationPipeline, PipelineConfig, cancellation_utils};

#[tokio::main]
async fn main() -> Result<()> {
    let config = PipelineConfig {
        grpc_endpoint: "your_grpc_endpoint".to_string(),
        grpc_token: "your_token".to_string(),
        task_timeout_seconds: 300,
        heartbeat_interval_seconds: 30,
        max_retries: 3,
    };

    let pipeline = TokenCancellationPipeline::new(config);
    
    // Start monitoring
    let task_handle = tokio::spawn(async move {
        pipeline.basic_cancellable_stream(
            "monitor_task".to_string(),
            vec!["target_address".to_string()],
        ).await
    });
    
    // Let it run for a while
    tokio::time::sleep(Duration::from_secs(60)).await;
    
    // Cancel the task
    cancellation_utils::cancel_task("monitor_task").await?;
    
    // Wait for completion
    task_handle.await??;
    
    Ok(())
}
```

### Example 2: Timeout-Based Cancellation

```rust
use tokio::time::{timeout, Duration};

async fn auto_cancel_example() -> Result<()> {
    let config = PipelineConfig::default();
    let pipeline = TokenCancellationPipeline::new(config);
    
    // Automatically cancel after 5 minutes
    match timeout(
        Duration::from_secs(300),
        pipeline.basic_cancellable_stream("task".to_string(), vec!["addr".to_string()])
    ).await {
        Ok(result) => result?,
        Err(_) => {
            println!("Task timed out");
            // Cleanup timed-out task
            cancellation_utils::cancel_task("task").await?;
        }
    }
    
    Ok(())
}
```

### Example 3: Multiple Concurrent Streams

```rust
async fn multi_stream_example() -> Result<()> {
    let config = PipelineConfig::default();
    let pipeline = TokenCancellationPipeline::new(config);
    
    // Monitor multiple targets
    let targets = vec![
        ("stream_1", vec!["addr1"]),
        ("stream_2", vec!["addr2"]),
        ("stream_3", vec!["addr3"]),
    ];
    
    // Start all streams
    let handles: Vec<_> = targets.into_iter().map(|(id, addrs)| {
        let pipeline = pipeline.clone();
        tokio::spawn(async move {
            pipeline.basic_cancellable_stream(
                id.to_string(),
                addrs.into_iter().map(String::from).collect()
            ).await
        })
    }).collect();
    
    // Later: shutdown all at once
    TokenCancellationPipeline::shutdown_all_tasks().await?;
    
    // Wait for all to complete
    for handle in handles {
        handle.await??;
    }
    
    Ok(())
}
```

### Example 4: Conditional Cancellation

```rust
async fn conditional_cancel_example() -> Result<()> {
    let config = PipelineConfig::default();
    let pipeline = TokenCancellationPipeline::new(config);
    
    // Start monitoring
    tokio::spawn({
        let pipeline = pipeline.clone();
        async move {
            pipeline.conditional_cancellation_stream(
                "conditional_task".to_string(),
                vec!["target".to_string()],
            ).await
        }
    });
    
    // Monitor some condition
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        
        // Check your business logic condition
        if should_stop_monitoring().await {
            cancellation_utils::cancel_task("conditional_task").await?;
            break;
        }
    }
    
    Ok(())
}

async fn should_stop_monitoring() -> bool {
    // Your business logic here
    // e.g., check market conditions, error rates, etc.
    false
}
```

## 🛠️ Best Practices

### 1. Always Register Cancellation Tokens

```rust
// ✅ Good: Register token for external cancellation
let token = CancellationToken::new();
ACTIVE_TASKS.insert(task_id.clone(), token.clone());

// ❌ Bad: Token only accessible within the task
let token = CancellationToken::new();
// No way to cancel from outside
```

### 2. Use `tokio::select!` for Cancellation

```rust
// ✅ Good: Responsive to cancellation
loop {
    select! {
        _ = token.cancelled() => break,
        result = work() => {
            // Handle result
        }
    }
}

// ❌ Bad: Not checking for cancellation
loop {
    let result = work().await;
    // This might run forever even if cancelled
}
```

### 3. Clean Up Resources

```rust
async fn task_with_cleanup(token: CancellationToken) -> Result<()> {
    let resource = acquire_resource().await?;
    
    let result = select! {
        _ = token.cancelled() => {
            println!("Task cancelled");
            Ok(())
        }
        result = do_work_with_resource(&resource) => result,
    };
    
    // ✅ Always clean up, even on cancellation
    release_resource(resource).await;
    
    result
}
```

### 4. Remove Tasks from Registry

```rust
// ✅ Good: Clean up registry
async fn managed_task(task_id: String, token: CancellationToken) -> Result<()> {
    // Do work...
    
    // Always remove from registry when done
    ACTIVE_TASKS.remove(&task_id);
    Ok(())
}
```

### 5. Handle Heartbeats with Cancellation

```rust
fn spawn_heartbeat_with_cancellation(
    tx: Arc<Mutex<impl Sink<Request>>>,
    token: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        
        loop {
            select! {
                _ = token.cancelled() => {
                    println!("Heartbeat cancelled");
                    break;
                }
                _ = interval.tick() => {
                    let mut tx = tx.lock().await;
                    if tx.send(heartbeat_request()).await.is_err() {
                        break;
                    }
                }
            }
        }
    })
}
```

## 🔍 Common Patterns in Your Code

Based on your `copy_trading.rs`, here are the key patterns you're already using:

### 1. Global Task Registry
```rust
lazy_static::lazy_static! {
    static ref MONITORING_TASKS: Arc<DashMap<String, CancellationToken>> = Arc::new(DashMap::new());
}
```

### 2. Task Registration and Cancellation
```rust
// Register
let cancellation_token = CancellationToken::new();
MONITORING_TASKS.insert(token_mint.clone(), cancellation_token.clone());

// Cancel
if let Some(entry) = MONITORING_TASKS.remove(token_mint) {
    entry.1.cancel();
}
```

### 3. Main Processing Loop
```rust
loop {
    tokio::select! {
        _ = cancellation_token.cancelled() => {
            logger.log(format!("Monitoring cancelled for token: {}", token_mint));
            break;
        }
        msg_result = stream.next() => {
            // Process messages
        }
    }
}
```

## 🚨 Common Pitfalls

### 1. Not Checking Cancellation Frequently Enough
```rust
// ❌ Bad: Long-running work without cancellation checks
async fn bad_task(token: CancellationToken) {
    for i in 0..1000000 {
        expensive_operation(i).await; // This could take forever
    }
}

// ✅ Good: Regular cancellation checks
async fn good_task(token: CancellationToken) {
    for i in 0..1000000 {
        if token.is_cancelled() {
            break;
        }
        expensive_operation(i).await;
    }
}
```

### 2. Forgetting to Remove from Registry
```rust
// ❌ Bad: Memory leak in registry
async fn leaky_task(task_id: String) {
    let token = CancellationToken::new();
    ACTIVE_TASKS.insert(task_id, token);
    // Task ends but never removed from ACTIVE_TASKS
}
```

### 3. Not Handling Cancellation in Heartbeats
```rust
// ❌ Bad: Heartbeat continues after main task cancelled
tokio::spawn(async move {
    loop {
        send_heartbeat().await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
});

// ✅ Good: Heartbeat respects cancellation
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        select! {
            _ = token.cancelled() => break,
            _ = interval.tick() => {
                if send_heartbeat().await.is_err() {
                    break;
                }
            }
        }
    }
});
```

## 📚 Advanced Usage

### Child Tokens for Hierarchical Cancellation

```rust
// Parent token controls multiple child tasks
let parent_token = CancellationToken::new();

// Child tokens are cancelled when parent is cancelled
let child1 = parent_token.child_token();
let child2 = parent_token.child_token();

// Cancel all children by cancelling parent
parent_token.cancel();
```

### Cancellation with Timeout

```rust
let token = CancellationToken::new();

// Cancel after timeout
let timeout_token = token.clone();
tokio::spawn(async move {
    tokio::time::sleep(Duration::from_secs(300)).await;
    timeout_token.cancel();
});

// Use the token normally
my_task(token).await?;
```

### Graceful vs Forced Shutdown

```rust
async fn graceful_shutdown_with_timeout(
    token: CancellationToken,
    graceful_timeout: Duration,
) -> Result<()> {
    // Try graceful shutdown first
    token.cancel();
    
    // Wait for graceful completion
    match timeout(graceful_timeout, wait_for_all_tasks()).await {
        Ok(_) => println!("Graceful shutdown completed"),
        Err(_) => {
            println!("Graceful shutdown timeout, forcing shutdown");
            force_shutdown().await?;
        }
    }
    
    Ok(())
}
```

## 🎯 Summary

The key to using `CancellationToken` effectively with gRPC:

1. **Always use `tokio::select!`** to make your tasks responsive to cancellation
2. **Register tokens in a global registry** for external control
3. **Clean up resources** even when cancelled
4. **Remove tasks from registries** to prevent memory leaks
5. **Handle cancellation in heartbeats** and auxiliary tasks
6. **Use timeouts** for automatic cancellation
7. **Implement graceful shutdown** patterns

This approach gives you robust, controllable async applications that can handle failures, shutdowns, and dynamic requirements gracefully. 