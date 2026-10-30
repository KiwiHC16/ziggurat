//! `ziggurat-router`: joins an existing Zigbee network on an OpenThread RCP and stays
//! on it as a childless router. See README.md.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, ValueEnum};
use tokio::net::TcpStream;
use tokio_serial::{FlowControl, SerialPortBuilderExt};
use ziggurat_driver::join::JoinParams;
use ziggurat_ieee_802154::types::{Eui64, Key};
use ziggurat_phy_spinel::SpinelPhy;
use ziggurat_router::control;
use ziggurat_router::router::Router;
use ziggurat_router::state::from_hex;
use ziggurat_spinel::client::{RcpTransport, SpinelClient};
use ziggurat_zigbee::constants::WELL_KNOWN_LINK_KEY;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FlowControlMode {
    None,
    Software,
    Hardware,
}

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// RCP serial device, or `tcp://host:port`
    #[arg(long)]
    device: String,
    #[arg(long, default_value_t = 460_800)]
    baudrate: u32,
    #[arg(long, value_enum, default_value_t = FlowControlMode::None)]
    flow_control: FlowControlMode,
    /// Router state (network key, address, counters). Joins when the file is absent.
    #[arg(long)]
    state: PathBuf,
    /// Channels to scan when joining
    #[arg(long, value_delimiter = ',', default_values_t = 11..=26)]
    channels: Vec<u8>,
    /// Only join this extended PAN ID (16 hex digits, as on the air, little endian)
    #[arg(long)]
    extended_pan_id: Option<String>,
    #[arg(long, default_value_t = 8)]
    tx_power: i8,
    /// Control port. Unauthenticated: keep it on the loopback.
    #[arg(long, default_value = "127.0.0.1:20109")]
    control: SocketAddr,
}

async fn open_transport(args: &Args) -> Result<Box<dyn RcpTransport>> {
    if let Some(addr) = args.device.strip_prefix("tcp://") {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .context("RCP connect timed out")??;
        stream.set_nodelay(true)?;
        return Ok(Box::new(stream));
    }
    let flow_control = match args.flow_control {
        FlowControlMode::None => FlowControl::None,
        FlowControlMode::Software => FlowControl::Software,
        FlowControlMode::Hardware => FlowControl::Hardware,
    };
    let port = tokio_serial::new(&args.device, args.baudrate)
        .flow_control(flow_control)
        .open_native_async()
        .with_context(|| format!("opening {}", args.device))?;
    Ok(Box::new(port))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let transport = open_transport(&args).await?;
    let phy = Arc::new(SpinelPhy::new(Arc::new(SpinelClient::new(transport))));
    let hw = phy
        .hw_address()
        .await
        .map_err(|e| anyhow!("reading the radio EUI64: {e}"))?;

    let mut params = JoinParams::new(Eui64(hw.0), Key(WELL_KNOWN_LINK_KEY.0));
    params.channels.clone_from(&args.channels);
    params.tx_power = args.tx_power;
    params.extended_pan_id = args
        .extended_pan_id
        .as_deref()
        .map(|text| -> Result<Eui64> {
            let bytes: [u8; 8] = from_hex(text)?
                .try_into()
                .map_err(|_| anyhow!("the extended PAN ID takes 8 bytes"))?;
            Ok(Eui64(bytes))
        })
        .transpose()?;

    let router = Router::start(phy, params, &args.state).await?;
    tokio::select! {
        result = control::serve(Arc::clone(&router), args.control) => result?,
        result = tokio::signal::ctrl_c() => {
            result?;
            router.save()?;
        }
    }
    Ok(())
}
