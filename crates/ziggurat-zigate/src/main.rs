//! ZiGate v1 emulator for the Jeedom Abeille plugin, backed by the Ziggurat stack.
//!
//! Abeille connects to it like to a ZiGate WiFi: `ab::gtwSubType = WIFI`,
//! `ab::gtwIpAddr = <host>:20108`. See README.md for the contract and the plan.

mod frame;
#[allow(dead_code)] // Message types are wired in as the bridge grows
mod msg;

use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing_subscriber::EnvFilter;

use frame::HostDecoder;

/// Coordinator IEEE used by `observe` mode. Abeille pins the IEEE it first sees in
/// `ab::zgIeeeAddrX` and ignores everything but 0x8000 if it later changes.
const OBSERVE_IEEE: u64 = 0x00ED_C0DE_0000_0001;
const OBSERVE_CHANNEL: u8 = 25;
const LQI: u8 = 0xFF;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address Abeille connects to (its `ab::gtwIpAddr`)
    #[arg(long, default_value = "0.0.0.0:20108", global = true)]
    listen: SocketAddr,

    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand, Debug)]
enum Mode {
    /// No radio: decode and log what Abeille sends, and answer the gateway-level
    /// handshake (8000/8010/8024/8014) so its startup sequence can be checked
    Observe,
    /// Full bridge over an OpenThread RCP (not implemented yet)
    Run {
        /// RCP serial device, or `tcp://host:port` over Ethernet
        #[arg(long)]
        device: String,
        /// The Sonoff Dongle Plus MG24 OpenThread RCP firmware has no hardware flow control
        #[arg(long, default_value_t = 460_800)]
        baudrate: u32,
        /// Network state file (keys, frame counters, devices): the ZiGate PDM equivalent
        #[arg(long, default_value = "ziggurat-zigate.json")]
        state: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();
    match args.mode {
        Mode::Observe => observe(args.listen).await,
        Mode::Run { device, .. } => {
            anyhow::bail!("run mode is not implemented yet (device {device}); use `observe`")
        }
    }
}

async fn observe(listen: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(listen).await?;
    tracing::info!("Observe mode, waiting for Abeille on {listen}");
    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::info!("Abeille connected from {peer}");
        // Abeille's socat holds a single connection; a new one replaces the old
        if let Err(e) = observe_connection(stream).await {
            tracing::warn!("Connection from {peer} ended: {e}");
        }
    }
}

async fn observe_connection(mut stream: TcpStream) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut decoder = HostDecoder::new();
    let mut buf = [0u8; 1024];
    let mut sqn: u8 = 0;
    let mut permit_join = false;

    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        for result in decoder.extend(&buf[..n]) {
            let request = match result {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("Bad frame: {e}");
                    continue;
                }
            };
            tracing::info!("<- {:04X} {}", request.msg_type, hex(&request.payload));

            sqn = sqn.wrapping_add(1);
            let mut replies = vec![msg::status(
                0x00,
                sqn,
                request.msg_type,
                !msg::is_gateway_only(request.msg_type),
                sqn,
            )];
            match request.msg_type {
                msg::GET_VERSION => replies.push(msg::version()),
                msg::START_NETWORK => {
                    replies.push(msg::network_formed(
                        0x01,
                        0x0000,
                        OBSERVE_IEEE,
                        OBSERVE_CHANNEL,
                    ));
                }
                msg::PERMIT_JOIN => permit_join = request.payload.get(2).is_some_and(|&d| d != 0),
                msg::GET_PERMIT_JOIN => replies.push(msg::permit_join_status(permit_join)),
                _ => {}
            }

            for reply in replies {
                tracing::info!("-> {:04X} {}", reply.msg_type, hex(&reply.payload));
                stream
                    .write_all(&frame::encode_to_host(&reply, LQI))
                    .await?;
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02X}");
        s
    })
}
