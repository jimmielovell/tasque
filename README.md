# tasque

A small task queue on [Tokio](https://tokio.rs).

```toml
[dependencies]
tasque = { git = "https://github.com/jimmielovell/tasque" }
```

## Usage

```rust,ignore
use serde::{Deserialize, Serialize};
use tasque::{MemoryStore, Priority, Step, Tasque};

#[derive(Clone, Serialize, Deserialize)]
struct Email { address: String, body: String }

#[derive(Clone, Serialize, Deserialize)]
struct Pdf { email_address: String, bytes: Vec<u8> }

let tasque = Tasque::new(MemoryStore::default(), clients)
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

- `ctx` derefs to the state given to `Tasque::new`. It also has `ctx.attempt()` and `ctx.queue(..)`.
- Return `Ok(())` when done, or `Ok(Step::next(job))` to hand off to `job`'s handler.
- The next job inherits priority, retries and persistence unless overridden: `Step::next(job).priority(..).retries(..).persist()`.
- `retries: None` means 3.
- `run` checks every `Step::next` type has a handler, then replays stored jobs.

## How jobs run

- Each handler runs up to 4 jobs at once; the rest wait in line.
- Higher priority goes first, but each level is only a 60s head start, so nothing starves.
- Each attempt gets 30s. Failures, timeouts and panics retry after 1s, 2s, 4s… (±10%, max 5 min).

## Persistence

Jobs queued with `persist: true` are saved to a `Store` until they finish, and `run` replays what's left after a restart.

- Jobs run at least once, so persisted handlers should be safe to repeat.
- Records are matched by handler name and stored as bincode, so renaming a handler or changing a job's fields strands old records.
- For JSON instead: `default-features = false, features = ["json", "memory"]`.
- `MemoryStore` is behind the default `memory` feature.
