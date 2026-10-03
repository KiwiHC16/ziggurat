//! ZiGate v1 emulator for the Jeedom Abeille plugin, backed by the Ziggurat stack.
//!
//! Abeille connects to it like to a ZiGate WiFi: `ab::gtwSubType = WIFI`,
//! `ab::gtwIpAddr = <host>:20108`. See README.md for the contract and the plan.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing_subscriber::EnvFilter;

use ziggurat_zigate::frame::{self, HostDecoder};
use ziggurat_zigate::server::{self, FlowControlMode, RunConfig};
use ziggurat_zigate::{hex, msg};

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
    /// The bridge over an OpenThread RCP (or `ziggurat-sim`)
    Run {
        /// RCP serial device, or `tcp://host:port` over Ethernet
        #[arg(long)]
        device: String,
        #[arg(long, default_value_t = 460_800)]
        baudrate: u32,
        /// The Sonoff Dongle Plus MG24 OpenThread RCP firmware has no hardware flow control
        #[arg(long, value_enum, default_value_t = FlowControlMode::None)]
        flow_control: FlowControlMode,
        /// Network state file (keys, frame counters, devices): the ZiGate PDM equivalent
        #[arg(long, default_value = "ziggurat-zigate.json")]
        state: PathBuf,
        /// Channel when forming a new network; an existing one keeps its channel
        #[arg(long, default_value_t = 25)]
        channel: u8,
        #[arg(long, default_value_t = 8)]
        tx_power: i8,
        /// Let Abeille's "erase PDM" (0x0012) wipe the network
        #[arg(long)]
        allow_erase: bool,
        /// Seconds to wait after the network starts before accepting Abeille: two link
        /// status periods, so routers know their link cost to the coordinator
        #[arg(long, default_value_t = 35)]
        warm_up: u64,
        /// Live capture of the radio's traffic for Wireshark (`-i TCP@<host>:<port>`)
        #[arg(long)]
        capture: Option<SocketAddr>,
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
        Mode::Run {
            device,
            baudrate,
            flow_control,
            state,
            channel,
            tx_power,
            allow_erase,
            warm_up,
            capture,
        } => {
            server::run(RunConfig {
                device,
                baudrate,
                flow_control,
                state_path: state,
                channel,
                tx_power,
                allow_erase,
                listen: args.listen,
                warm_up: std::time::Duration::from_secs(warm_up),
                capture,
            })
            .await
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
