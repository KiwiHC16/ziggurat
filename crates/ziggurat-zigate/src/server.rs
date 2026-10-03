//! Radio bring-up and the TCP side Abeille connects to.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_serial::{FlowControl, SerialPortBuilderExt};
use ziggurat_driver::runtime::TokioSpawner;
use ziggurat_driver::zigbee_stack::{Tunables, ZigbeeStack};
use ziggurat_phy::RadioPhy;
use ziggurat_phy_spinel::SpinelPhy;
use ziggurat_spinel::client::{RcpTransport, SpinelClient};

use crate::bridge::{Bridge, Outbound};
use crate::frame::{self, HostDecoder};
use crate::state::{NetworkState, eui_to_hex};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FlowControlMode {
    Hardware,
    Software,
    None,
}

impl From<FlowControlMode> for FlowControl {
    fn from(mode: FlowControlMode) -> Self {
        match mode {
            FlowControlMode::Hardware => Self::Hardware,
            FlowControlMode::Software => Self::Software,
            FlowControlMode::None => Self::None,
        }
    }
}

pub struct RunConfig {
    pub device: String,
    pub baudrate: u32,
    pub flow_control: FlowControlMode,
    pub state_path: PathBuf,
    /// Channel of a newly formed network; an existing network keeps its own
    pub channel: u8,
    pub tx_power: i8,
    pub allow_erase: bool,
    pub listen: SocketAddr,
}

/// `tcp://host:port` (wired network RCP or ser2net), or a serial device path. Same
/// semantics as `ziggurat-server`.
async fn open_transport(config: &RunConfig) -> Result<Box<dyn RcpTransport>> {
    if let Some(addr) = config.device.strip_prefix("tcp://") {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .context("RCP connect timed out")??;
        stream.set_nodelay(true)?;
        return Ok(Box::new(stream));
    }
    let port = tokio_serial::new(&config.device, config.baudrate)
        .flow_control(config.flow_control.into())
        .open_native_async()
        .with_context(|| format!("opening {}", config.device))?;
    Ok(Box::new(port))
}

/// Bring the network up on the radio and serve Abeille until the process stops
pub async fn run(config: RunConfig) -> Result<()> {
    let transport = open_transport(&config).await?;
    let phy = Arc::new(SpinelPhy::new(Arc::new(SpinelClient::new(transport))));
    let hw = phy
        .hw_address()
        .await
        .map_err(|e| anyhow::anyhow!("reading the radio EUI64: {e}"))?;
    tracing::info!("Radio EUI64 {}", eui_to_hex(hw));

    let bridge = start_bridge(phy, hw, &config).await?;
    serve(bridge, config.listen).await
}

/// Load or create the network, start the stack and the bridge on any radio
pub async fn start_bridge<P: RadioPhy>(
    phy: Arc<P>,
    hw: ziggurat_ieee_802154::types::Eui64,
    config: &RunConfig,
) -> Result<Arc<Bridge<P>>> {
    let (mut state, newly_formed) = match NetworkState::load(&config.state_path)? {
        Some(state) => {
            tracing::info!(
                "Restoring network PAN {} on channel {} ({} devices)",
                state.pan_id,
                state.channel,
                state.devices.len()
            );
            (state, false)
        }
        None => {
            let state = NetworkState::generate(hw, config.channel, config.tx_power)?;
            tracing::info!(
                "Forming a new network: PAN {} on channel {}",
                state.pan_id,
                state.channel
            );
            (state, true)
        }
    };
    if state.coordinator()? != hw {
        // Abeille pins the coordinator IEEE: keep the saved one with a new radio
        tracing::warn!(
            "Radio EUI64 {} differs from the network's coordinator {}: keeping the latter",
            eui_to_hex(hw),
            state.coordinator_ieee
        );
    }

    let network = state.network_config()?;
    // The counters now start past the margin: save that as the new floor
    state.nwk_frame_counter = network.network_key_tx_counter;
    let stack = ZigbeeStack::new(phy, network, Tunables::default(), TokioSpawner::default());
    state.restore_into(&stack)?;
    state.aps_frame_counter = state
        .aps_frame_counter
        .saturating_add(crate::state::FRAME_COUNTER_MARGIN);
    state.save(&config.state_path)?;

    stack
        .start_network()
        .await
        .map_err(|e| anyhow::anyhow!("starting the network: {e}"))?;
    stack.spawn_tracked(|s| async move { s.run().await });
    tracing::info!("Zigbee network up");

    let bridge = Bridge::new(
        stack,
        state,
        Some(config.state_path.clone()),
        newly_formed,
        config.allow_erase,
    );
    bridge.spawn_tasks();
    Ok(bridge)
}

pub async fn serve<P: RadioPhy>(bridge: Arc<Bridge<P>>, listen: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(listen).await?;
    tracing::info!("Waiting for Abeille on {listen}");
    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::info!("Abeille connected from {peer}");
        let bridge = Arc::clone(&bridge);
        tokio::spawn(async move {
            if let Err(e) = connection(bridge, stream).await {
                tracing::warn!("Connection from {peer} ended: {e}");
            } else {
                tracing::info!("Abeille {peer} disconnected");
            }
        });
    }
}

/// One Abeille connection. A newer connection takes over the outbound messages.
async fn connection<P: RadioPhy>(bridge: Arc<Bridge<P>>, stream: TcpStream) -> Result<()> {
    stream.set_nodelay(true)?;
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Outbound>();
    bridge.attach(tx);

    let writer_task = tokio::spawn(async move {
        while let Some((message, lqi)) = rx.recv().await {
            if writer
                .write_all(&frame::encode_to_host(&message, lqi))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut decoder = HostDecoder::new();
    let mut buf = [0u8; 1024];
    let result = loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) => break Err(e.into()),
        };
        for frame in decoder.extend(&buf[..n]) {
            match frame {
                Ok(message) => bridge.handle(&message),
                Err(e) => tracing::warn!("Bad frame from Abeille: {e}"),
            }
        }
    };
    writer_task.abort();
    result
}
