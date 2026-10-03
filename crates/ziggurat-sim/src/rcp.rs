//! A simulated OpenThread RCP: speaks Spinel over HDLC-lite on any byte stream and
//! puts frames on the simulated [`Medium`].
//!
//! It implements what `ziggurat-phy-spinel` uses: `CMD_RESET`, the core properties
//! (`PROTOCOL_VERSION`, `NCP_VERSION`, `HWADDR`), radio programming, raw stream
//! transmit/receive and energy scans. Anything else is answered with
//! `LAST_STATUS = PROP_NOT_FOUND`, as a stock RCP does for unsupported properties.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use ziggurat_ieee_802154::types::{Eui64, Nwk, PanId};
use ziggurat_ieee_802154::{FrameBytes, Ieee802154Frame};
use ziggurat_phy::{RadioConfig, RxFrame, TxFrame, TxResult};
use ziggurat_spinel::{
    HdlcSpecial, SpinelCommandId, SpinelFrame, SpinelFramePropValueIs, SpinelHeader,
    SpinelPropertyId, SpinelStatus, eui64_from_spinel_bytes, eui64_to_spinel_bytes,
    hdlc_escape_into, hdlc_unescape_into, packed_uint21_deserialize, packed_uint21_to_bytes,
};

use crate::medium::{Medium, NodeId};

pub const NCP_VERSION: &str = "ZIGGURAT-SIM/0.1.0; SIMULATED-RCP";
const PROTOCOL_VERSION: (u32, u32) = (4, 3);
const NOISE_FLOOR: i8 = -100;
const RESET_TIME: Duration = Duration::from_millis(5);

/// The radio programming accumulated from individual property sets.
#[derive(Debug, Clone)]
struct Programming {
    enabled: bool,
    raw_stream: bool,
    config: RadioConfig,
    scan_channel: u8,
    scan_period_ms: u16,
}

impl Programming {
    const fn new() -> Self {
        Self {
            enabled: false,
            raw_stream: false,
            config: RadioConfig {
                channel: 11,
                tx_power: 0,
                short_address: Nwk(0xFFFE),
                extended_address: Eui64([0; 8]),
                pan_id: PanId(0xFFFF),
                promiscuous: false,
                rx_on_when_idle: false,
                frame_pending_short: Vec::new(),
                frame_pending_extended: Vec::new(),
            },
            scan_channel: 11,
            scan_period_ms: 0,
        }
    }

    /// The radio listens once its PHY is enabled
    fn effective(&self) -> Option<RadioConfig> {
        self.enabled.then(|| self.config.clone())
    }
}

/// Serve one host connection. Returns when the host closes the stream.
pub async fn serve<S>(stream: S, medium: Arc<Medium>, node: NodeId) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (out_tx, mut out_rx) = mpsc::channel::<SpinelFrame>(256);

    // Single writer: responses and unsolicited frames are serialized here
    let writer_task = tokio::spawn(async move {
        let mut wire = Vec::new();
        while let Some(frame) = out_rx.recv().await {
            wire.clear();
            wire.push(HdlcSpecial::Flag as u8);
            hdlc_escape_into(&frame.to_bytes(), &mut wire);
            wire.push(HdlcSpecial::Flag as u8);
            if writer.write_all(&wire).await.is_err() {
                break;
            }
        }
    });

    // Received frames from the medium become unsolicited STREAM_RAW updates
    let (rx_tx, mut rx_rx) = mpsc::channel::<RxFrame>(64);
    medium.set_receiver(node, rx_tx);
    let rx_out = out_tx.clone();
    let rx_task = tokio::spawn(async move {
        while let Some(frame) = rx_rx.recv().await {
            let value = rx_frame_to_spinel(&frame);
            if rx_out
                .send(unsolicited(SpinelPropertyId::StreamRaw, value))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut rcp = Rcp {
        medium: Arc::clone(&medium),
        node,
        programming: Programming::new(),
        out: out_tx,
    };
    let mut buffer = Vec::new();
    let mut scratch = Vec::new();
    let mut chunk = [0u8; 2048];

    let result = loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) => break Err(e),
        };
        buffer.extend_from_slice(&chunk[..n]);

        while let Some(flag) = buffer.iter().position(|&b| b == HdlcSpecial::Flag as u8) {
            let raw: Vec<u8> = buffer.drain(..=flag).collect();
            let body = &raw[..raw.len() - 1];
            if body.is_empty() || hdlc_unescape_into(body, &mut scratch).is_err() {
                continue;
            }
            match SpinelFrame::from_bytes(&scratch) {
                Ok(frame) => rcp.handle(frame).await,
                Err(e) => tracing::warn!("Simulated RCP: bad Spinel frame: {e}"),
            }
        }
    };

    medium.configure(node, None);
    rx_task.abort();
    writer_task.abort();
    result
}

struct Rcp {
    medium: Arc<Medium>,
    node: NodeId,
    programming: Programming,
    out: mpsc::Sender<SpinelFrame>,
}

impl Rcp {
    async fn send(&self, frame: SpinelFrame) {
        let _ = self.out.send(frame).await;
    }

    async fn reply(&self, request: &SpinelFrame, property: SpinelPropertyId, value: Vec<u8>) {
        self.send(response(request.header.transaction_id, property, value))
            .await;
    }

    async fn reply_status(&self, request: &SpinelFrame, status: SpinelStatus) {
        self.reply(
            request,
            SpinelPropertyId::LastStatus,
            packed_uint21_to_bytes(status as u32),
        )
        .await;
    }

    fn apply(&self) {
        self.medium
            .configure(self.node, self.programming.effective());
    }

    async fn handle(&mut self, frame: SpinelFrame) {
        match frame.command_id {
            SpinelCommandId::Reset => self.reset().await,
            SpinelCommandId::Noop => self.reply_status(&frame, SpinelStatus::Ok).await,
            SpinelCommandId::PropValueGet => self.get(&frame).await,
            SpinelCommandId::PropValueSet => self.set(&frame).await,
            other => {
                tracing::debug!("Simulated RCP: unsupported command {other:?}");
                self.reply_status(&frame, SpinelStatus::InvalidCommand)
                    .await;
            }
        }
    }

    async fn reset(&mut self) {
        self.programming = Programming::new();
        self.medium.configure(self.node, None);
        tokio::time::sleep(RESET_TIME).await;
        self.send(unsolicited(
            SpinelPropertyId::LastStatus,
            packed_uint21_to_bytes(SpinelStatus::ResetSoftware as u32),
        ))
        .await;
    }

    async fn get(&self, frame: &SpinelFrame) {
        let Some(property) = property_of(&frame.payload) else {
            return self.reply_status(frame, SpinelStatus::ParseError).await;
        };
        let value = match property {
            SpinelPropertyId::ProtocolVersion => {
                let mut v = packed_uint21_to_bytes(PROTOCOL_VERSION.0);
                v.extend(packed_uint21_to_bytes(PROTOCOL_VERSION.1));
                v
            }
            SpinelPropertyId::NcpVersion => {
                let mut v = NCP_VERSION.as_bytes().to_vec();
                v.push(0);
                v
            }
            // SPINEL_PROTOCOL_TYPE_THREAD, like OpenThread RCP
            SpinelPropertyId::InterfaceType => packed_uint21_to_bytes(3),
            SpinelPropertyId::Hwaddr => {
                eui64_to_spinel_bytes(self.medium.hw_address(self.node)).to_vec()
            }
            SpinelPropertyId::PhyChan => vec![self.programming.config.channel],
            SpinelPropertyId::PhyTxPower => vec![self.programming.config.tx_power.cast_unsigned()],
            SpinelPropertyId::PhyEnabled => vec![u8::from(self.programming.enabled)],
            SpinelPropertyId::PhyRxSensitivity => vec![(-100i8).cast_unsigned()],
            _ => return self.reply_status(frame, SpinelStatus::PropNotFound).await,
        };
        self.reply(frame, property, value).await;
    }

    async fn set(&mut self, frame: &SpinelFrame) {
        let Ok((id, value)) = packed_uint21_deserialize(&frame.payload) else {
            return self.reply_status(frame, SpinelStatus::ParseError).await;
        };
        let Ok(property) = SpinelPropertyId::try_from(id) else {
            return self.reply_status(frame, SpinelStatus::PropNotFound).await;
        };
        let value = value.to_vec();

        if property == SpinelPropertyId::StreamRaw {
            return self.transmit(frame, &value).await;
        }
        if property == SpinelPropertyId::MacScanState {
            return self.scan(frame, &value).await;
        }

        let p = &mut self.programming;
        let ok = match (property, value.as_slice()) {
            (SpinelPropertyId::PhyEnabled, [v]) => {
                p.enabled = *v != 0;
                true
            }
            (SpinelPropertyId::PhyChan, [ch]) => {
                p.config.channel = *ch;
                true
            }
            (SpinelPropertyId::PhyTxPower, [dbm]) => {
                p.config.tx_power = dbm.cast_signed();
                true
            }
            (SpinelPropertyId::MacPromiscuousMode, [mode]) => {
                p.config.promiscuous = *mode != 0;
                true
            }
            (SpinelPropertyId::Mac154Laddr, bytes) => match <[u8; 8]>::try_from(bytes) {
                Ok(b) => {
                    p.config.extended_address = eui64_from_spinel_bytes(b);
                    true
                }
                Err(_) => false,
            },
            (SpinelPropertyId::Mac154Saddr, bytes) => Nwk::deserialize(bytes)
                .map(|(nwk, _)| p.config.short_address = nwk)
                .is_ok(),
            (SpinelPropertyId::Mac154Panid, bytes) => PanId::deserialize(bytes)
                .map(|(pan, _)| p.config.pan_id = pan)
                .is_ok(),
            (SpinelPropertyId::MacRxOnWhenIdleMode, [v]) => {
                p.config.rx_on_when_idle = *v != 0;
                true
            }
            (SpinelPropertyId::MacRawStreamEnabled, [v]) => {
                p.raw_stream = *v != 0;
                true
            }
            (SpinelPropertyId::MacSrcMatchEnabled, [_]) => true,
            (SpinelPropertyId::MacSrcMatchShortAddresses, bytes) => {
                p.config.frame_pending_short = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| Nwk(u16::from_le_bytes(*c)))
                    .collect();
                true
            }
            (SpinelPropertyId::MacSrcMatchExtendedAddresses, bytes) => {
                p.config.frame_pending_extended = bytes
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| eui64_from_spinel_bytes(*c))
                    .collect();
                true
            }
            (SpinelPropertyId::MacScanMask, [ch, ..]) => {
                p.scan_channel = *ch;
                true
            }
            (SpinelPropertyId::MacScanPeriod, [lo, hi]) => {
                p.scan_period_ms = u16::from_le_bytes([*lo, *hi]);
                true
            }
            _ => false,
        };

        if ok {
            self.apply();
            self.reply(frame, property, value).await;
        } else {
            tracing::debug!("Simulated RCP: unsupported set {property:?}={value:02X?}");
            self.reply_status(frame, SpinelStatus::InvalidArgument)
                .await;
        }
    }

    async fn transmit(&self, frame: &SpinelFrame, value: &[u8]) {
        let Some(tx) = spinel_to_tx_frame(value) else {
            return self.reply_status(frame, SpinelStatus::ParseError).await;
        };
        if !self.programming.enabled {
            return self.reply_status(frame, SpinelStatus::InvalidState).await;
        }
        let status = match self.medium.transmit(self.node, &tx).await {
            TxResult::Acked => SpinelStatus::Ok,
            TxResult::NoAck => SpinelStatus::NoAck,
            TxResult::ChannelAccessFailure => SpinelStatus::CcaFailure,
            TxResult::Aborted | TxResult::Failed => SpinelStatus::Failure,
        };
        self.reply_status(frame, status).await;
    }

    async fn scan(&self, frame: &SpinelFrame, value: &[u8]) {
        const ENERGY: u8 = 2;
        if value != [ENERGY] {
            return self
                .reply_status(frame, SpinelStatus::InvalidArgument)
                .await;
        }
        self.reply(frame, SpinelPropertyId::MacScanState, vec![ENERGY])
            .await;

        let channel = self.programming.scan_channel;
        let rssi = self.medium.noise(channel);
        let period = Duration::from_millis(u64::from(self.programming.scan_period_ms));
        let out = self.out.clone();
        tokio::spawn(async move {
            tokio::time::sleep(period).await;
            let _ = out
                .send(unsolicited(
                    SpinelPropertyId::MacEnergyScanResult,
                    vec![channel, rssi.cast_unsigned()],
                ))
                .await;
            let _ = out
                .send(unsolicited(SpinelPropertyId::MacScanState, vec![0]))
                .await;
        });
    }
}

fn property_of(payload: &[u8]) -> Option<SpinelPropertyId> {
    let (id, _) = packed_uint21_deserialize(payload).ok()?;
    SpinelPropertyId::try_from(id).ok()
}

fn response(tid: u8, property: SpinelPropertyId, value: Vec<u8>) -> SpinelFrame {
    SpinelFrame {
        header: SpinelHeader {
            flag: 0b10,
            network_link_id: 0,
            transaction_id: tid,
        },
        command_id: SpinelCommandId::PropValueIs,
        payload: SpinelFramePropValueIs {
            property_id: property,
            value,
        }
        .to_bytes(),
    }
}

fn unsolicited(property: SpinelPropertyId, value: Vec<u8>) -> SpinelFrame {
    response(0, property, value)
}

/// Parse a `STREAM_RAW` transmit: `len:u16 psdu(with FCS) channel backoffs retries
/// csma ...`. Optional trailing fields are taken when present.
fn spinel_to_tx_frame(value: &[u8]) -> Option<TxFrame> {
    let [lo, hi, rest @ ..] = value else {
        return None;
    };
    let len = usize::from(u16::from_le_bytes([*lo, *hi]));
    if len < 2 || rest.len() < len {
        return None;
    }
    let (psdu, meta) = rest.split_at(len);
    Some(TxFrame {
        psdu: psdu[..len - 2].to_vec(),
        channel: meta.first().copied(),
        max_csma_backoffs: meta.get(1).copied().unwrap_or(4),
        max_frame_retries: meta.get(2).copied().unwrap_or(3),
        csma_ca: meta.get(3).is_none_or(|&v| v != 0),
        security_processed: meta.get(6).is_some_and(|&v| v != 0),
    })
}

/// Build a `STREAM_RAW` receive: `len:u16 psdu(with FCS) rssi noise flags:u32 channel
/// lqi timestamp:u64 rx_error`.
fn rx_frame_to_spinel(frame: &RxFrame) -> Vec<u8> {
    let fcs = Ieee802154Frame::<FrameBytes>::compute_fcs(&frame.psdu).to_le_bytes();
    let len = u16::try_from(frame.psdu.len() + 2).unwrap_or(u16::MAX);
    let mut value = len.to_le_bytes().to_vec();
    value.extend_from_slice(&frame.psdu);
    value.extend(fcs);
    value.push(frame.rssi.cast_unsigned());
    value.push(NOISE_FLOOR.cast_unsigned());
    value.extend(0u32.to_le_bytes());
    value.push(frame.channel);
    value.push(frame.lqi);
    value.extend(frame.timestamp_us.to_le_bytes());
    value.push(0);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use ziggurat_spinel::client::SpinelRxFrame;

    #[test]
    fn rx_frame_matches_the_client_parser() {
        let rx = RxFrame {
            psdu: vec![0x41, 0x88, 0x01, 0x62, 0x1A, 0xFF, 0xFF, 0x00, 0x00, 0x09],
            channel: 25,
            rssi: -42,
            lqi: 200,
            timestamp_us: 123_456,
        };
        let parsed = SpinelRxFrame::from_bytes(&rx_frame_to_spinel(&rx)).unwrap();
        assert_eq!(&parsed.psdu[..parsed.psdu.len() - 2], rx.psdu.as_slice());
        assert_eq!(parsed.channel, 25);
        assert_eq!(parsed.rssi, -42);
        assert_eq!(parsed.lqi, 200);
        assert_eq!(parsed.timestamp_us, 123_456);
    }

    #[test]
    fn tx_frame_round_trip_from_the_client_encoder() {
        use ziggurat_spinel::client::SpinelTxFrame;
        let psdu = vec![
            0x41, 0x88, 0x01, 0x62, 0x1A, 0x34, 0x12, 0x00, 0x00, 0xAA, 0xBB, 0xCC,
        ];
        let spinel = SpinelTxFrame {
            psdu: psdu.clone(),
            channel: Some(25),
            max_csma_backoffs: Some(4),
            max_frame_retries: Some(5),
            enable_csma_ca: Some(true),
            is_header_updated: Some(true),
            is_a_retransmit: Some(false),
            is_security_processed: Some(true),
            tx_delay: None,
            tx_delay_base_time: None,
            rx_channel_after_tx: None,
            tx_power: None,
        };
        let tx = spinel_to_tx_frame(&spinel.to_bytes()).unwrap();
        // The client sends the PSDU with its FCS; the simulated radio strips it
        assert_eq!(tx.psdu, psdu[..psdu.len() - 2]);
        assert_eq!(tx.channel, Some(25));
        assert_eq!(tx.max_frame_retries, 5);
        assert!(tx.security_processed);
    }
}
