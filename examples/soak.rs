//! A long run through one relay, as many phones and desktops would make
//! it: pairs of ends that reach each other only through it, each pair
//! sending small messages back and forth all the time, a bigger transfer
//! now and then, and connecting again every so often.
//!
//!   soak <relay-url> [pairs] [minutes]       (20 pairs, 10 minutes)
//!
//! Every 30 s it prints what went through and what failed; at the end, the
//! totals. Watch the relay's memory and CPU beside it (docs/operations.md,
//! "Soak test").

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, TransportAddr, endpoint::presets};

const ALPN: &[u8] = b"baocode/soak/1";

/// Small messages a second, each way, in each pair.
const MESSAGES_PER_SECOND: u64 = 20;
const MESSAGE: usize = 512;
/// A bigger transfer this often, of this many bytes.
const BULK_EVERY: Duration = Duration::from_secs(30);
const BULK: usize = 1 << 20;
/// A pair connects again this often.
const RECONNECT_EVERY: Duration = Duration::from_secs(120);

#[derive(Default)]
struct Counts {
    echoed: AtomicU64,
    bytes: AtomicU64,
    bulks: AtomicU64,
    connects: AtomicU64,
    errors: AtomicU64,
    /// The slowest small round trip, in microseconds.
    slowest: AtomicU64,
    /// Round trips over 1 s.
    slow: AtomicU64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(relay) = args.first() else {
        bail!("soak <relay-url> [pairs] [minutes]");
    };
    let relay: RelayUrl = relay.parse()?;
    let pairs: usize = args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(20);
    let minutes: u64 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(10);
    let counts = Arc::new(Counts::default());
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);

    let mut tasks = Vec::new();
    for n in 0..pairs {
        let relay = relay.clone();
        let counts = counts.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(error) = pair(relay, counts.clone(), deadline).await {
                counts.errors.fetch_add(1, Relaxed);
                eprintln!("pair {n}: {error:#}");
            }
        }));
    }

    let started = Instant::now();
    let mut report = tokio::time::interval(Duration::from_secs(30));
    report.tick().await;
    while Instant::now() < deadline {
        report.tick().await;
        print(&counts, started.elapsed());
    }
    for task in tasks {
        let _ = task.await;
    }
    println!("done:");
    print(&counts, started.elapsed());
    if counts.errors.load(Relaxed) > 0 {
        bail!("{} errors", counts.errors.load(Relaxed));
    }
    Ok(())
}

fn print(counts: &Counts, elapsed: Duration) {
    println!(
        "{:>5}s  echoed {}  {:.1} MB  bulk {}  connects {}  errors {}  slowest {} ms  over 1 s {}",
        elapsed.as_secs(),
        counts.echoed.load(Relaxed),
        counts.bytes.load(Relaxed) as f64 / 1e6,
        counts.bulks.load(Relaxed),
        counts.connects.load(Relaxed),
        counts.errors.load(Relaxed),
        counts.slowest.load(Relaxed) / 1000,
        counts.slow.load(Relaxed),
    );
}

async fn bind(relay: &RelayUrl) -> Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::Minimal)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Custom([relay.clone()].into_iter().collect()))
        .clear_ip_transports()
        .bind()
        .await?;
    tokio::time::timeout(Duration::from_secs(20), endpoint.online())
        .await
        .context("relay not reached")?;
    Ok(endpoint)
}

/// One desktop and one phone, until [deadline].
async fn pair(relay: RelayUrl, counts: Arc<Counts>, deadline: Instant) -> Result<()> {
    let desktop = bind(&relay).await?;
    let phone = bind(&relay).await?;
    let echo = desktop.clone();
    tokio::spawn(async move {
        while let Some(incoming) = echo.accept().await {
            tokio::spawn(async move {
                let Ok(connection) = incoming.await else {
                    return;
                };
                while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                    tokio::spawn(async move {
                        let _ = tokio::io::copy(&mut recv, &mut send).await;
                        let _ = send.finish();
                    });
                }
            });
        }
    });
    let addr = EndpointAddr::from_parts(desktop.id(), [TransportAddr::Relay(relay)]);
    while Instant::now() < deadline {
        let connection =
            tokio::time::timeout(Duration::from_secs(20), phone.connect(addr.clone(), ALPN))
                .await
                .context("connect timed out")??;
        counts.connects.fetch_add(1, Relaxed);
        let until = deadline.min(Instant::now() + RECONNECT_EVERY);
        let result = session(&connection, &counts, until).await;
        connection.close(0u32.into(), b"again");
        if let Err(error) = result {
            counts.errors.fetch_add(1, Relaxed);
            eprintln!("session: {error:#}");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    phone.close().await;
    desktop.close().await;
    Ok(())
}

/// Small messages on one stream, a bulk transfer on its own now and then,
/// until [until].
async fn session(
    connection: &iroh::endpoint::Connection,
    counts: &Counts,
    until: Instant,
) -> Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    let message = vec![7u8; MESSAGE];
    let mut back = vec![0u8; MESSAGE];
    let mut tick = tokio::time::interval(Duration::from_millis(1000 / MESSAGES_PER_SECOND));
    let mut next_bulk = Instant::now() + BULK_EVERY;
    while Instant::now() < until {
        tick.tick().await;
        let start = Instant::now();
        send.write_all(&message).await?;
        tokio::time::timeout(Duration::from_secs(10), recv.read_exact(&mut back))
            .await
            .context("echo timed out")??;
        let took = start.elapsed().as_micros() as u64;
        counts.slowest.fetch_max(took, Relaxed);
        if took > 1_000_000 {
            counts.slow.fetch_add(1, Relaxed);
        }
        counts.echoed.fetch_add(1, Relaxed);
        counts.bytes.fetch_add(2 * MESSAGE as u64, Relaxed);
        if Instant::now() >= next_bulk {
            next_bulk += BULK_EVERY;
            bulk(connection, counts).await?;
        }
    }
    send.finish()?;
    Ok(())
}

async fn bulk(connection: &iroh::endpoint::Connection, counts: &Counts) -> Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    let data = vec![1u8; BULK];
    let writing = async {
        send.write_all(&data).await?;
        send.finish()?;
        anyhow::Ok(())
    };
    let reading = async { anyhow::Ok(recv.read_to_end(BULK + 1).await?) };
    let ((), got) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::try_join!(writing, reading)
    })
    .await
    .context("bulk timed out")??;
    if got.len() != BULK {
        bail!("bulk short: {} of {BULK}", got.len());
    }
    counts.bulks.fetch_add(1, Relaxed);
    counts.bytes.fetch_add(2 * BULK as u64, Relaxed);
    Ok(())
}
