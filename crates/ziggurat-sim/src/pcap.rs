//! Live capture of the simulated air for Wireshark.
//!
//! Each TCP client gets a pcap stream (link type 283, `IEEE802_15_4_TAP`, which carries
//! the channel, RSSI and LQI of every frame). Wireshark reads it directly with
//! `-i TCP@<host>:<port>`, no capture privileges needed.

use std::net::SocketAddr;
use std::time::UNIX_EPOCH;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::error::RecvError;
use ziggurat_ieee_802154::{FrameBytes, Ieee802154Frame};

use tokio::sync::broadcast;

use crate::medium::AirFrame;

/// `LINKTYPE_IEEE802_15_4_TAP`
pub const LINKTYPE_IEEE802_15_4_TAP: u32 = 283;

/// pcap global header, little-endian, microsecond timestamps
#[must_use]
pub fn global_header() -> Vec<u8> {
    let mut h = Vec::with_capacity(24);
    h.extend(0xA1B2_C3D4u32.to_le_bytes());
    h.extend(2u16.to_le_bytes());
    h.extend(4u16.to_le_bytes());
    h.extend(0i32.to_le_bytes());
    h.extend(0u32.to_le_bytes());
    h.extend(65_535u32.to_le_bytes());
    h.extend(LINKTYPE_IEEE802_15_4_TAP.to_le_bytes());
    h
}

/// One TLV of the TAP header, padded to 4 bytes
fn tlv(out: &mut Vec<u8>, kind: u16, value: &[u8]) {
    out.extend(kind.to_le_bytes());
    out.extend(u16::try_from(value.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(value);
    out.resize(out.len().next_multiple_of(4), 0);
}

/// One pcap record: TAP header (FCS type, RSS, channel, LQI) then the PSDU with its FCS
#[must_use]
pub fn record(frame: &AirFrame) -> Vec<u8> {
    const FCS_TYPE: u16 = 0;
    const RSS: u16 = 1;
    const CHANNEL_ASSIGNMENT: u16 = 3;
    const LQI: u16 = 10;
    const FCS_16_BIT: u8 = 1;
    const PAGE_0: u8 = 0;

    let mut tlvs = Vec::new();
    tlv(&mut tlvs, FCS_TYPE, &[FCS_16_BIT]);
    tlv(&mut tlvs, RSS, &f32::from(frame.rssi).to_le_bytes());
    let mut channel = u16::from(frame.channel).to_le_bytes().to_vec();
    channel.push(PAGE_0);
    tlv(&mut tlvs, CHANNEL_ASSIGNMENT, &channel);
    tlv(&mut tlvs, LQI, &[frame.lqi]);

    let mut packet = vec![0u8, 0u8];
    packet.extend(u16::try_from(4 + tlvs.len()).unwrap_or(0).to_le_bytes());
    packet.extend(tlvs);
    packet.extend_from_slice(&frame.psdu);
    packet.extend(Ieee802154Frame::<FrameBytes>::compute_fcs(&frame.psdu).to_le_bytes());

    let since_epoch = frame.time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let len = u32::try_from(packet.len()).unwrap_or(0);
    let mut out = Vec::with_capacity(16 + packet.len());
    out.extend(
        u32::try_from(since_epoch.as_secs())
            .unwrap_or(0)
            .to_le_bytes(),
    );
    out.extend(since_epoch.subsec_micros().to_le_bytes());
    out.extend(len.to_le_bytes());
    out.extend(len.to_le_bytes());
    out.extend(packet);
    out
}

/// Serve the live capture to any number of Wireshark instances
/// `frames` is any source of air frames: the simulated medium, or a tap on a real radio
pub async fn serve(listen: SocketAddr, frames: broadcast::Sender<AirFrame>) {
    let listener = match TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::warn!("Capture port {listen} unavailable: {e}");
            return;
        }
    };
    tracing::info!(
        "Live capture on {listen} (wireshark -k -i TCP@<host>:{})",
        listen.port()
    );
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        tracing::info!("Capture client {peer} connected");
        tokio::spawn(stream_to(stream, frames.subscribe()));
    }
}

async fn stream_to(mut stream: TcpStream, mut frames: broadcast::Receiver<AirFrame>) {
    let _ = stream.set_nodelay(true);
    if stream.write_all(&global_header()).await.is_err() {
        return;
    }
    loop {
        match frames.recv().await {
            Ok(frame) => {
                if stream.write_all(&record(&frame)).await.is_err() {
                    return;
                }
            }
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!("Capture client too slow, {missed} frames dropped");
            }
            Err(RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_layout() {
        let frame = AirFrame {
            psdu: vec![0x02, 0x00, 0x42],
            channel: 25,
            rssi: -40,
            lqi: 255,
            time: UNIX_EPOCH + std::time::Duration::from_micros(1_500_000),
        };
        let r = record(&frame);
        // ts 1 s 500000 µs, lengths: 4 + 32 TAP + 3 PSDU + 2 FCS = 41
        assert_eq!(
            &r[..16],
            &[1, 0, 0, 0, 0x20, 0xA1, 0x07, 0, 41, 0, 0, 0, 41, 0, 0, 0]
        );
        let tap = &r[16..];
        assert_eq!(&tap[..4], &[0, 0, 36, 0]);
        // FCS type TLV, then RSS = -40.0 f32
        assert_eq!(&tap[4..12], &[0, 0, 1, 0, 1, 0, 0, 0]);
        assert_eq!(&tap[12..16], &[1, 0, 4, 0]);
        assert_eq!(&tap[16..20], &(-40.0f32).to_le_bytes());
        // Channel 25, page 0; LQI 255
        assert_eq!(&tap[20..28], &[3, 0, 3, 0, 25, 0, 0, 0]);
        assert_eq!(&tap[28..36], &[10, 0, 1, 0, 255, 0, 0, 0]);
        assert_eq!(&tap[36..39], &[0x02, 0x00, 0x42]);
        assert_eq!(tap.len(), 41);
    }
}
