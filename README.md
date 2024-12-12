# **tasque** 

`tasque` is an asynchronous **priority-based task queue** for Rust applications, built using **Tokio**. Tasks can be prioritized as `High`, `Medium`, or `Low`, and will automatically age into higher priority queues to avoid starvation. Failed tasks can be retried with exponential backoff and jitter to ensure efficient handling.

---

## **Features**

- **Priority Queues**: Tasks are categorized as `High`, `Medium`, or `Low` priority.
- **Automatic Aging**: Tasks upgrade to higher priority if they remain in the queue for too long.
- **Retry Mechanism**: Failed tasks are retried with exponential backoff and jitter to avoid synchronized retries.
- **Timeout Management**: Tasks are executed with a configurable timeout to prevent indefinite blocking.
- **Concurrency-Safe**: Supports concurrent task production and consumption using Tokio primitives.

---

## **Installation**

Add `tasque` to your `Cargo.toml`:

```toml
[dependencies]
tasque = "0.1.0"
```

## Retry and Aging Strategies

The Tasque queue implements retry and aging mechanisms to ensure robust task processing and fair task prioritization.

### Exponential Backoff with Jitter

The retry mechanism uses an exponential backoff algorithm with jitter to handle task failures:

#### Backoff Calculation
1. **Base Delay**: Starts with a 1-second base delay
2. **Exponential Progression**: Doubles the delay with each retry
3. **Maximum Delay Cap**: Prevents excessive wait times
4. **Jitter**: Adds randomness to prevent synchronized retries

#### Example Retry Sequence
```
Retry 0: 1s (base delay)
Retry 1: 2s (2^1 * base delay)
Retry 2: 4s (2^2 * base delay)
Retry 3: 8s (2^3 * base delay)
Max Delay: Capped at configured max_delay (default 5 minutes)
```

#### Jitter Mechanism
- Randomly adjusts delay by ±10%
- Prevents thundering herd problem
- Distributes retry attempts

### Retry Limitations
- Maximum retry attempts configurable
- Tracks retry count per task
- Drops task after max retries exhausted

## Aging Strategy

### Priority Escalation

Tasks can be automatically upgraded to higher priority queues based on waiting time.

#### Key Characteristics
- Prevents task starvation
- Ensures long-waiting tasks get attention
- Configurable aging duration

#### Aging Rules
- Low priority → Medium priority
- Medium priority → High priority
- High priority remains unchanged

#### Aging Process
1. Track task creation time
2. Compare against configured aging duration
3. Automatically upgrade priority
4. Reset creation timestamp

### Aging Example
```
Aging Duration: 5 minutes
Low Priority Task Created → Waits 5+ minutes
   ↓
Automatically Promoted to Medium Priority
```

## Configuration Parameters

```rust
Tasque::new(
    timeout: Duration,           // Task execution timeout
    max_delay: Duration,         // Maximum retry delay
    aging_duration: Duration     // Time before priority upgrade
)
```

## Best Practices

1. Choose appropriate timeout values
2. Set realistic max retry delays
3. Configure aging duration based on system load
4. Monitor and adjust strategies periodically

## Potential Use Cases

- Distributed task processing
- Background job systems
- Resilient microservices
- Event-driven architectures

## Limitations

- Does not guarantee exactly-once processing
- Potential for task duplication on persistent failures
- Overhead of tracking and managing task metadata

