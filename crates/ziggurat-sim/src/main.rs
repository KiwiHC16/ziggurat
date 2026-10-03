//! A simulated OpenThread RCP dongle on TCP.
//!
//! ```text
//! ziggurat-sim --listen 127.0.0.1:9000
//! ziggurat --device tcp://127.0.0.1:9000
//! ```

use std::net::SocketAddr;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use ziggurat_ieee_802154::types::Eui64;
use ziggurat_sim::medium::Medium;
use ziggurat_sim::rcp;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address the host stack connects to (`--device tcp://<listen>`)
    #[arg(long, default_value = "127.0.0.1:9000")]
    listen: SocketAddr,

    /// Factory EUI64 of the simulated dongle, as 16 hex digits (big-endian, as printed)
    #[arg(long, default_value = "00124B0000ED0001")]
    hw_address: String,

    /// Seed for the link-loss draws
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    // try_from_hex takes the printed (big-endian) form and stores over-the-air order
    let hw = Eui64::try_from_hex(&args.hw_address)
        .map_err(|e| anyhow::anyhow!("bad --hw-address: {e:?}"))?;

    let medium = Medium::new(args.seed);
    let dongle = medium.add_node("dongle", hw);

    let listener = TcpListener::bind(args.listen).await?;
    tracing::info!("Simulated RCP listening on {}", args.listen);
    loop {
        let (stream, peer) = listener.accept().await?;
        stream.set_nodelay(true)?;
        tracing::info!("Host connected from {peer}");
        // One host at a time, like a serial port
        if let Err(e) = rcp::serve(stream, medium.clone(), dongle).await {
            tracing::warn!("Host {peer} disconnected: {e}");
        } else {
            tracing::info!("Host {peer} disconnected");
        }
    }
}
