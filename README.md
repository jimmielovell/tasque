# tasque

A small task queue on [Tokio](https://tokio.rs).

```toml
[dependencies]
tasque = { git = "https://github.com/jimmielovell/tasque" }
```

## Usage

```rust,ignore
use serde::{Deserialize, Serialize};
use tasque::{MokaStore, Priority, Step, Tasque};

#[derive(Clone, Serialize, Deserialize)]
struct Email { address: String, body: String }

#[derive(Clone, Serialize, Deserialize)]
struct Pdf { email_address: String, bytes: Vec<u8> }

let tasque = Tasque::new(MokaStore::default(), clients)
    .add("email", |ctx, Email { address, body }| async move {
        ctx.postmark.send(&address, &body).await?;
        Ok(())
    })
    .add("pdf", |_ctx, Pdf { email_address, bytes }| async move {
        let body = extract_text(bytes)?;
        Ok(Step::next(Email { address: email_address, body }))
    })
    .run()
    .await?;

tasque.queue(Email { address, body }, Priority::High, Some(3), false).await?;
```

- `ctx` derefs to the state given to `Tasque::new`. It also has `ctx.attempt_count()` and `ctx.queue(..)`.
- Return `Ok(())` when done, or `Ok(Step::next(job))` to hand off to `job`'s handler.
- The next job inherits priority, max_retries and durability unless overridden: `Step::next(job).priority(..).max_retries(..).durable()`.
- `max_retries: None` means 3.
- `run` checks every `Step::next` type has a handler, then replays stored jobs.

## How jobs run

- Each handler runs up to 4 jobs at once; the rest wait in line.
- Higher priority goes first, but each level is only a 60s head start, so nothing starves.
- Each attempt gets 30s. Failures, timeouts and panics retry after 1s, 2s, 4s… (±10%, max 5 min).

## Durability

Jobs queued with `durable: true` are saved to a `Store` until they finish. When a process stops, through `tasque.shutdown()` or a crash, another reclaims its unfinished jobs.

- Jobs run at least once, so persisted handlers should be safe to repeat.
- Jobs that run out of max_retries are marked failed.
- Durable jobs are matched by handler name and stored as bincode, so renaming a handler or changing a job's fields strands old records.
- For JSON instead: `default-features = false, features = ["json", "moka-store"]`.
- `MokaStore` is behind the default `moka-store` feature, `ScyllaStore` behind `scylla-store`.

### ScyllaDB

```rust,ignore
let store = ScyllaStoreBuilder::new(session)
    .keyspace_name("my_app")?
    .create_tables(true) // or create them in a migration
    .build()
    .await?;
```

Any number of processes can share the tables (`t_tasque_jobs`, `t_tasque_workers`, `t_tasque_failed` by default). A process that stops checking in for 15s is taken over by another. Job rows live 7 days; failed jobs stay in `t_tasque_failed`.
