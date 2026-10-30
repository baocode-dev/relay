//! The relay in dev mode with real iroh endpoints that have no IP
//! transport: everything they say goes through it.

use std::time::{Duration, Instant};

use baocode_relay::{
    Relay,
    config::{AccessFiles, Config, Limits},
};
use iroh::{
    Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey, TransportAddr,
    endpoint::{Connection, presets},
};
use tokio::time::timeout;

const ALPN: &[u8] = b"baocode/test/1";

fn unlimited() -> Limits {
    Limits {
        bytes_per_second: 0,
        burst_bytes: 0,
    }
}

async fn relay(access: AccessFiles, limits: Limits) -> (Relay, RelayUrl) {
    let mut config = Config::dev(0);
    config.access = access;
    config.limits = limits;
    let relay = Relay::spawn(config, None).await.unwrap();
    let url = format!("http://{}", relay.http_addr().unwrap()).parse().unwrap();
    (relay, url)
}

async fn endpoint(key: SecretKey, relays: &[RelayUrl]) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Custom(relays.iter().cloned().collect()))
        .clear_ip_transports()
        .bind()
        .await
        .unwrap()
}

fn key(n: u8) -> SecretKey {
    SecretKey::from_bytes(&[n; 32])
}

/// [server]'s address as the phone would have it: its key and every relay,
/// no IPs.
fn addr(server: &Endpoint, relays: &[RelayUrl]) -> EndpointAddr {
    EndpointAddr::from_parts(
        server.id(),
        relays.iter().cloned().map(TransportAddr::Relay),
    )
}

/// Echoes every stream back, until the endpoint closes.
fn echo(server: &Endpoint) {
    let server = server.clone();
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            tokio::spawn(async move {
                let Ok(connection) = incoming.await else { return };
                while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                    tokio::spawn(async move {
                        let _ = tokio::io::copy(&mut recv, &mut send).await;
                        let _ = send.finish();
                    });
                }
            });
        }
    });
}

async fn round_trip(connection: &Connection, bytes: usize) -> anyhow::Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    let data = vec![7u8; bytes];
    let writing = async {
        send.write_all(&data).await?;
        send.finish()?;
        anyhow::Ok(())
    };
    let reading = async { anyhow::Ok(recv.read_to_end(bytes + 1).await?) };
    let ((), got) = tokio::try_join!(writing, reading)?;
    anyhow::ensure!(got.len() == bytes, "got {} of {bytes}", got.len());
    Ok(())
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < Duration::from_secs(20), "waited for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn two_devices_talk_through_it() {
    let (relay, url) = relay(AccessFiles::default(), unlimited()).await;
    let relays = [url];
    let server = endpoint(key(1), &relays).await;
    let client = endpoint(key(2), &relays).await;
    echo(&server);
    timeout(Duration::from_secs(10), server.online()).await.unwrap();

    let connection = timeout(Duration::from_secs(15), client.connect(addr(&server, &relays), ALPN))
        .await
        .unwrap()
        .unwrap();
    round_trip(&connection, 3 << 20).await.unwrap();
    assert!(connection.paths().iter().any(|p| p.is_relay() && p.is_selected()));
    assert_eq!(relay.gate().connected(), 2);
    assert_eq!(relay.gate().metrics().admitted.get(), 2);
}

#[tokio::test]
async fn a_ban_shuts_a_device_out_and_keeps_it_out() {
    let dir = tempfile::tempdir().unwrap();
    let banned = dir.path().join("banned.txt");
    std::fs::write(&banned, "# nobody yet\n").unwrap();
    let access = AccessFiles {
        banned: Some(banned.clone()),
        ..AccessFiles::default()
    };
    let (relay, url) = relay(access, unlimited()).await;
    let relays = [url];
    let server = endpoint(key(1), &relays).await;
    let client = endpoint(key(2), &relays).await;
    echo(&server);
    timeout(Duration::from_secs(10), server.online()).await.unwrap();
    let connection = client.connect(addr(&server, &relays), ALPN).await.unwrap();
    round_trip(&connection, 1000).await.unwrap();

    std::fs::write(&banned, format!("{} abuse\n", key(2).public())).unwrap();
    assert_eq!(relay.reload().unwrap(), 1);
    assert_eq!(relay.gate().metrics().kicked.get(), 1);

    // Its relay is gone, and saying so when it asks again.
    let status = client.home_relay_status();
    wait_for("the ban to reach the client", || {
        use iroh::Watcher;
        status
            .clone()
            .get()
            .iter()
            .any(|s| s.auth_denied_reason() == Some("banned"))
    })
    .await;
    assert!(relay.gate().metrics().denied_banned.get() >= 1);
    // Nothing more passes.
    let after = timeout(Duration::from_secs(5), round_trip(&connection, 1000)).await;
    assert!(!matches!(after, Ok(Ok(()))), "a banned device still got through");
    // The other device is still welcome.
    assert_eq!(relay.gate().connected(), 1);
}

#[tokio::test]
async fn sending_is_held_to_the_rate_limit() {
    let limits = Limits {
        bytes_per_second: 256 << 10,
        burst_bytes: 64 << 10,
    };
    let (_relay, url) = relay(AccessFiles::default(), limits).await;
    let relays = [url];
    let server = endpoint(key(1), &relays).await;
    let client = endpoint(key(2), &relays).await;
    echo(&server);
    timeout(Duration::from_secs(10), server.online()).await.unwrap();
    let connection = client.connect(addr(&server, &relays), ALPN).await.unwrap();

    // 1 MB there and back: each way through a 256 KB/s bucket, about 4 s.
    let start = Instant::now();
    round_trip(&connection, 1 << 20).await.unwrap();
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(3000), "took only {took:?}");
    assert!(took < Duration::from_secs(30), "took {took:?}");
}

#[tokio::test]
async fn the_limit_changes_while_running() {
    let limits = Limits {
        bytes_per_second: 64 << 10,
        burst_bytes: 64 << 10,
    };
    let (relay, url) = relay(AccessFiles::default(), limits).await;
    let relays = [url];
    let server = endpoint(key(1), &relays).await;
    let client = endpoint(key(2), &relays).await;
    echo(&server);
    timeout(Duration::from_secs(10), server.online()).await.unwrap();
    let connection = client.connect(addr(&server, &relays), ALPN).await.unwrap();

    relay.service().set_client_rate_limit(None);
    let start = Instant::now();
    round_trip(&connection, 2 << 20).await.unwrap();
    // 2 MB at 64 KB/s would be half a minute.
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

#[tokio::test]
async fn when_one_relay_goes_the_other_carries_on() {
    let (first, a) = relay(AccessFiles::default(), unlimited()).await;
    let (_second, b) = relay(AccessFiles::default(), unlimited()).await;
    let relays = [a, b];
    let server = endpoint(key(1), &relays).await;
    let client = endpoint(key(2), &relays).await;
    echo(&server);
    timeout(Duration::from_secs(10), server.online()).await.unwrap();
    let connection = client.connect(addr(&server, &relays), ALPN).await.unwrap();
    round_trip(&connection, 1000).await.unwrap();

    first.shutdown().await;
    // What the app's endpoints do when their home relay stays down 3 s
    // (bao_iroh's relay_watchdog): look again now, rather than at iroh's
    // next network report up to 26 s on.
    tokio::time::sleep(Duration::from_secs(3)).await;
    server.network_change().await;
    client.network_change().await;

    // A new connection finds the way through the second.
    let again = timeout(
        Duration::from_secs(30),
        client.connect(addr(&server, &relays), ALPN),
    )
    .await
    .expect("no connection after the first relay went")
    .unwrap();
    timeout(Duration::from_secs(20), round_trip(&again, 100_000))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn entitlements_stand_on_the_cache_when_the_url_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("entitlements.cache.json");
    std::fs::write(
        &cache,
        format!(
            r#"{{"endpoints": {{"{}": "standard", "{}": "standard"}}}}"#,
            key(1).public(),
            key(2).public()
        ),
    )
    .unwrap();
    let access = AccessFiles {
        default_tier: String::new(),
        // Nothing listens there.
        entitlements_url: Some("http://127.0.0.1:9/entitlements".into()),
        cache: Some(cache.clone()),
        ..AccessFiles::default()
    };
    let (relay, url) = relay(access, unlimited()).await;
    assert!(relay.fetch().await.is_err());
    // The cache was not touched by the failure.
    assert!(std::fs::read_to_string(&cache).unwrap().contains(&key(1).public().to_string()));

    let relays = [url];
    let server = endpoint(key(1), &relays).await;
    let client = endpoint(key(2), &relays).await;
    let stranger = endpoint(key(3), &relays).await;
    echo(&server);
    timeout(Duration::from_secs(10), server.online()).await.unwrap();
    let connection = client.connect(addr(&server, &relays), ALPN).await.unwrap();
    round_trip(&connection, 1000).await.unwrap();

    let status = stranger.home_relay_status();
    wait_for("the stranger to be refused", || {
        use iroh::Watcher;
        status
            .clone()
            .get()
            .iter()
            .any(|s| s.auth_denied_reason() == Some("not entitled"))
    })
    .await;
    assert_eq!(relay.gate().connected(), 2);
}

#[tokio::test]
async fn over_tls_with_metrics_as_deployed() {
    let dir = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
    let config = Config::parse(&format!(
        r#"
        hostname = "127.0.0.1"
        [bind]
        http = "127.0.0.1:0"
        https = "127.0.0.1:0"
        quic = "127.0.0.1:0"
        metrics = "127.0.0.1:0"
        [tls]
        mode = "files"
        cert = "{}"
        key = "{}"
        "#,
        cert_path.display(),
        key_path.display()
    ))
    .unwrap();
    let relay = Relay::spawn(config, None).await.unwrap();
    assert!(relay.quic_addr().is_some());
    let url: RelayUrl = format!("https://{}", relay.https_addr().unwrap()).parse().unwrap();
    let relays = [url];
    let roots = iroh_relay::tls::CaTlsConfig::custom_roots([cert.cert.der().clone()]);
    let bind = |n| {
        Endpoint::builder(presets::Minimal)
            .secret_key(key(n))
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Custom(relays.iter().cloned().collect()))
            .ca_tls_config(roots.clone())
            .clear_ip_transports()
            .bind()
    };
    let server = bind(1).await.unwrap();
    let client = bind(2).await.unwrap();
    echo(&server);
    timeout(Duration::from_secs(10), server.online()).await.unwrap();
    let connection = client.connect(addr(&server, &relays), ALPN).await.unwrap();
    round_trip(&connection, 100_000).await.unwrap();

    let text = reqwest::get(format!("http://{}/metrics", relay.metrics_addr().unwrap()))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("baocode_relay_admitted_total 2"), "{text}");
    assert!(text.contains("relayserver_bytes_recv"), "{text}");
    // No device ids in there.
    assert!(!text.contains(&key(1).public().to_string()));
}
