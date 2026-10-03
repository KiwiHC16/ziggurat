//! A simulated OpenThread RCP dongle on TCP, optionally with simulated devices around it.
//!
//! ```text
//! ziggurat-sim --listen 127.0.0.1:9000
//! ziggurat --device tcp://127.0.0.1:9000
//!
//! # With two simulated BlitzWolf plugs on the network of a ziggurat-zigate bridge
//! ziggurat-sim --network-state network.json --plug A4C1380000000001:1A01 --plug A4C1380000000002:1A02
//!
//! # A plug and an IKEA on/off remote driving group 7A01; buttons through the control port
//! ziggurat-sim --network-state network.json --plug A4C1380000000001:1A01 \\
//!     --remote 5C0272FFFE000001:1A02:7A01
//! echo "1A02 on" | nc -q1 127.0.0.1 9001
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use ziggurat_driver::zigbee_stack::{NetworkConfig, NwkDeviceType, WELL_KNOWN_LINK_KEY};
use ziggurat_ieee_802154::types::{Eui64, Key, Nwk, PanId};
use ziggurat_sim::device::{Model, SimDevice};
use ziggurat_sim::medium::Medium;
use ziggurat_sim::phy::SimPhy;
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

    /// Network state of a ziggurat-zigate bridge: simulated devices are provisioned on
    /// that network (channel, PAN, network key)
    #[arg(long)]
    network_state: Option<PathBuf>,

    /// A simulated BlitzWolf BW-SHP13 plug, as `IEEE:NWK` in hex (repeatable)
    #[arg(long, value_name = "IEEE:NWK")]
    plug: Vec<String>,

    /// A simulated IKEA TRADFRI on/off remote, as `IEEE:NWK:GROUP` in hex: its buttons
    /// send On/Off to that group (repeatable)
    #[arg(long, value_name = "IEEE:NWK:GROUP")]
    remote: Vec<String>,

    /// Control port: `list`, `<nwk> on|off|toggle` (remote buttons, or the plug's own
    /// button), `<nwk> announce`
    #[arg(long, default_value = "127.0.0.1:9001")]
    control: SocketAddr,

    /// Live capture of the air for Wireshark: `wireshark -k -i TCP@<host>:<port>`
    #[arg(long, default_value = "0.0.0.0:17760")]
    capture: SocketAddr,

    /// Announce the devices this many seconds after start, without waiting for the
    /// coordinator to open joins
    #[arg(long)]
    announce_after: Option<u64>,

    /// Interval of the plugs' periodic power reports, in seconds (0: none)
    #[arg(long, default_value_t = 300)]
    report_interval: u64,
}

fn hex_field<'a>(state: &'a serde_json::Value, name: &str) -> Result<&'a str> {
    state[name]
        .as_str()
        .with_context(|| format!("network state: missing {name}"))
}

/// The bridge's network, as a router configuration for the simulated devices
fn network_from_state(path: &PathBuf) -> Result<NetworkConfig> {
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )?;
    let key = hex_field(&state, "network_key")?;
    let key: Vec<u8> = (0..key.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&key[i..i + 2], 16))
        .collect::<Result<_, _>>()?;
    Ok(NetworkConfig {
        role: NwkDeviceType::Router,
        channel: u8::try_from(state["channel"].as_u64().context("channel")?)?,
        update_id: u8::try_from(state["update_id"].as_u64().unwrap_or(0))?,
        pan_id: PanId(u16::from_str_radix(hex_field(&state, "pan_id")?, 16)?),
        extended_pan_id: Eui64::try_from_hex(hex_field(&state, "extended_pan_id")?)
            .map_err(|e| anyhow::anyhow!("extended PAN ID: {e:?}"))?,
        network_address: Nwk(0xFFFE),
        ieee_address: Eui64([0; 8]),
        network_key: Key(key
            .try_into()
            .map_err(|_| anyhow::anyhow!("network key must be 16 bytes"))?),
        network_key_seq_number: u8::try_from(state["network_key_seq"].as_u64().unwrap_or(0))?,
        network_key_tx_counter: 0,
        tc_link_key: WELL_KNOWN_LINK_KEY,
        tclk_seed: None,
        tx_power: 8,
        source_routing: false,
    })
}

/// Line protocol for pressing buttons from a shell (`nc`)
async fn control(listen: SocketAddr, devices: Vec<Arc<SimDevice>>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let listener = match TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::warn!("Control port {listen} unavailable: {e}");
            return;
        }
    };
    tracing::info!("Control port on {listen}");
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let devices = devices.clone();
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let reply = control_command(&devices, line.trim());
                if writer
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
}

fn control_command(devices: &[Arc<SimDevice>], line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["list"] => devices
            .iter()
            .map(|d| {
                let state = d
                    .attribute(1, 0x0006, 0x0000)
                    .map(|a| if a.value == [0] { " off" } else { " on" })
                    .unwrap_or_default();
                format!(
                    "{:04X} {} groups={:04X?}{state}",
                    d.nwk.0,
                    d.model_name(),
                    d.groups()
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        [nwk, action] => {
            let Ok(nwk) = u16::from_str_radix(nwk, 16) else {
                return format!("bad address {nwk}");
            };
            let Some(device) = devices.iter().find(|d| d.nwk.0 == nwk) else {
                return format!("no device {nwk:04X}");
            };
            let command = match *action {
                "off" => 0x00,
                "on" => 0x01,
                "toggle" => 0x02,
                "announce" => {
                    device.announce();
                    return "ok".into();
                }
                other => return format!("unknown action {other}"),
            };
            let result = if device.model_name() == "TRADFRIonoffswitch" {
                device.press(command)
            } else {
                // The plug's own button
                let on = match command {
                    0x00 => false,
                    0x01 => true,
                    _ => device
                        .attribute(1, 0x0006, 0x0000)
                        .is_some_and(|a| a.value == [0]),
                };
                device.set_on_off(1, on);
                Ok(())
            };
            result.map_or_else(|e| format!("error: {e}"), |()| "ok".into())
        }
        _ => "commands: list | <nwk> on|off|toggle|announce".into(),
    }
}

fn parse_plug(spec: &str) -> Result<(Eui64, Nwk)> {
    let (ieee, nwk) = spec
        .split_once(':')
        .with_context(|| format!("--plug {spec}: expected IEEE:NWK"))?;
    let ieee = Eui64::try_from_hex(ieee).map_err(|e| anyhow::anyhow!("--plug {spec}: {e:?}"))?;
    let nwk = u16::from_str_radix(nwk, 16).with_context(|| format!("--plug {spec}"))?;
    Ok((ieee, Nwk(nwk)))
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

    let mut devices = Vec::new();
    if !args.plug.is_empty() || !args.remote.is_empty() {
        let path = args
            .network_state
            .as_ref()
            .context("--plug/--remote need --network-state")?;
        let network = network_from_state(path)?;
        let mut specs: Vec<(String, Eui64, Nwk, Model)> = Vec::new();
        for spec in &args.plug {
            let (ieee, nwk) = parse_plug(spec)?;
            specs.push((format!("plug {spec}"), ieee, nwk, Model::blitzwolf_plug()));
        }
        for spec in &args.remote {
            let (device, group) = spec
                .rsplit_once(':')
                .with_context(|| format!("--remote {spec}: expected IEEE:NWK:GROUP"))?;
            let (ieee, nwk) = parse_plug(device)?;
            let group =
                u16::from_str_radix(group, 16).with_context(|| format!("--remote {spec}"))?;
            specs.push((
                format!("remote {spec}"),
                ieee,
                nwk,
                Model::ikea_remote(group),
            ));
        }
        for (name, ieee, nwk, model) in specs {
            let node = medium.add_node(name.clone(), ieee);
            let phy = Arc::new(SimPhy::new(medium.clone(), node));
            let template = NetworkConfig {
                network_key: network.network_key.clone(),
                tc_link_key: network.tc_link_key.clone(),
                tclk_seed: None,
                ..network
            };
            let is_plug = model.target_group.is_none();
            let device = SimDevice::start(phy, template, ieee, nwk, model).await?;
            if is_plug && args.report_interval > 0 {
                device.spawn_periodic_reports(Duration::from_secs(args.report_interval));
            }
            tracing::info!("Simulated {name} on channel {}", network.channel);
            devices.push(device);
        }
    }
    tokio::spawn(control(args.control, devices.clone()));
    tokio::spawn(ziggurat_sim::pcap::serve(args.capture, medium.sniffer_feed()));
    if let Some(delay) = args.announce_after {
        let devices: Vec<Arc<SimDevice>> = devices.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            for device in devices {
                device.announce();
            }
        });
    }

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
