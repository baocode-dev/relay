//! Two ends for trying the relay across real networks (tests/nat):
//!
//!   peer listen <relay-url> [ca.pem]          prints its id, echoes streams
//!   peer dial <relay-url> <id> [ca.pem]       sends 4 MB, says which path
//!   peer cert <ip> <dir>                      a self-signed cert.pem, key.pem
//!
//! The dialler waits up to 20 s for a direct path, then prints
//! `path: direct <addr>` or `path: relay`, and the round trip's speed.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint, EndpointAddr, PublicKey, RelayMode, RelayUrl, TransportAddr, endpoint::presets,
};
use iroh_relay::tls::CaTlsConfig;
use rustls::pki_types::{CertificateDer, pem::PemObject};

const ALPN: &[u8] = b"baocode/nat-test/1";

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["listen", relay, rest @ ..] => listen(relay.parse()?, rest.first().copied()).await,
        ["dial", relay, id, rest @ ..] => {
            dial(relay.parse()?, id.parse()?, rest.first().copied()).await
        }
        ["cert", ip, dir] => {
            let cert = rcgen::generate_simple_self_signed(vec![ip.to_string()])?;
            std::fs::write(format!("{dir}/cert.pem"), cert.cert.pem())?;
            std::fs::write(format!("{dir}/key.pem"), cert.signing_key.serialize_pem())?;
            Ok(())
        }
        _ => bail!("peer listen <relay-url> [ca.pem] | peer dial <relay-url> <id> [ca.pem]"),
    }
}

async fn bind(relay: &RelayUrl, ca: Option<&str>) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::Minimal)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Custom([relay.clone()].into_iter().collect()));
    if let Some(ca) = ca {
        let roots = CertificateDer::pem_file_iter(ca)
            .context("ca file")?
            .collect::<Result<Vec<_>, _>>()?;
        builder = builder.ca_tls_config(CaTlsConfig::custom_roots(roots));
    }
    Ok(builder.bind().await?)
}

async fn listen(relay: RelayUrl, ca: Option<&str>) -> Result<()> {
    let endpoint = bind(&relay, ca).await?;
    tokio::time::timeout(Duration::from_secs(20), endpoint.online())
        .await
        .context("relay not reached")?;
    println!("{}", endpoint.id());
    eprintln!("addr: {:?}", endpoint.addr());
    while let Some(incoming) = endpoint.accept().await {
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
    Ok(())
}

async fn dial(relay: RelayUrl, id: PublicKey, ca: Option<&str>) -> Result<()> {
    let endpoint = bind(&relay, ca).await?;
    tokio::time::timeout(Duration::from_secs(20), endpoint.online())
        .await
        .context("relay not reached")?;
    eprintln!("addr: {:?}", endpoint.addr());
    let addr = EndpointAddr::from_parts(id, [TransportAddr::Relay(relay)]);
    let connection = tokio::time::timeout(Duration::from_secs(20), endpoint.connect(addr, ALPN))
        .await
        .context("connect timed out")??;
    println!("connected");

    let start = Instant::now();
    let direct = loop {
        let direct = connection
            .paths()
            .iter()
            .find(|p| p.is_selected() && p.is_ip())
            .map(|p| format!("{:?}", p.remote_addr()));
        if direct.is_some() || start.elapsed() > Duration::from_secs(20) {
            break direct;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    for path in connection.paths().iter() {
        eprintln!(
            "path {:?} selected={} rtt={:?}",
            path.remote_addr(),
            path.is_selected(),
            path.rtt()
        );
    }
    match &direct {
        Some(addr) => println!("path: direct {addr} after {:?}", start.elapsed()),
        None => println!("path: relay"),
    }

    let size = 4 << 20;
    let start = Instant::now();
    let (mut send, mut recv) = connection.open_bi().await?;
    let data = vec![1u8; size];
    let writing = async {
        send.write_all(&data).await?;
        send.finish()?;
        anyhow::Ok(())
    };
    let reading = async { anyhow::Ok(recv.read_to_end(size + 1).await?) };
    let ((), got) = tokio::try_join!(writing, reading)?;
    if got.len() != size {
        bail!("echo short: {} of {size}", got.len());
    }
    let secs = start.elapsed().as_secs_f64();
    println!("echo: {} MB/s", (2 * size) as f64 / secs / 1e6);
    connection.close(0u32.into(), b"done");
    endpoint.close().await;
    Ok(())
}
