use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tasque::{MemoryStore, Priority, Step, Tasque};
use tokio::time::sleep;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Email {
    address: String,
    body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Pdf {
    email_address: String,
    bytes: Vec<u8>,
}

/// Stands in for an email client that fails the first send.
struct Mailer {
    sends: AtomicU32,
}

impl Mailer {
    async fn send(&self, address: &str, body: &str) -> Result<(), String> {
        if self.sends.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err("503 Service Unavailable".into());
        }
        sleep(Duration::from_millis(100)).await;
        println!("sent to {address}: {body}");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), tasque::Error> {
    tracing_subscriber::fmt::init();

    let mailer = Mailer {
        sends: AtomicU32::new(0),
    };

    let tasque = Tasque::new(MemoryStore::default(), mailer)
        .add("email", |ctx, Email { address, body }| async move {
            ctx.send(&address, &body).await?;
            Ok(())
        })
        .add(
            "pdf",
            |_ctx,
             Pdf {
                 email_address,
                 bytes,
             }| async move {
                let body = String::from_utf8(bytes)?;
                Ok(Step::next(Email {
                    address: email_address,
                    body,
                }))
            },
        )
        .run()
        .await?;

    let welcome = Email {
        address: "new@example.com".into(),
        body: "Hello buddy".into(),
    };
    tasque.queue(welcome, Priority::Low, Some(3), false).await?;

    let pdf = Pdf {
        email_address: "reader@example.com".into(),
        bytes: b"the text inside the pdf".to_vec(),
    };
    tasque.queue(pdf, Priority::High, None, false).await?;

    // The first send fails and is retried a second later.
    sleep(Duration::from_secs(3)).await;
    Ok(())
}
