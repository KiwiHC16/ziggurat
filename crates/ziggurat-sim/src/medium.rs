//! A simulated 802.15.4 medium shared by every simulated radio.
//!
//! The medium does what the radio hardware and the air do between two RCPs: address
//! filtering, automatic MAC acks and MAC retries. Each directed link has its own
//! delivery probability and signal quality, so asymmetric links (a device that hears
//! the coordinator badly but is heard well) can be reproduced.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Duration, Instant};
use ziggurat_ieee_802154::types::{Eui64, Nwk, PanId};
use ziggurat_ieee_802154::{Ieee802154Address, Ieee802154Frame, Ieee802154FrameHeader};
use ziggurat_phy::{RadioConfig, RxFrame, TxFrame, TxResult};

const BROADCAST_PAN: PanId = PanId(0xFFFF);
const BROADCAST_NWK: Nwk = Nwk(0xFFFF);

/// 250 kbit/s: 32 µs per byte, plus the 6-byte PHY header
const US_PER_BYTE: u64 = 32;
const PHY_OVERHEAD_BYTES: u64 = 6;
/// Turnaround plus ack frame airtime
const ACK_TIME: Duration = Duration::from_micros(192 + 11 * US_PER_BYTE);

pub type NodeId = usize;

/// One direction of a radio link.
#[derive(Debug, Clone, Copy)]
pub struct Link {
    /// Probability that a frame sent over this link is received, in `0.0..=1.0`
    pub delivery: f64,
    pub rssi: i8,
    pub lqi: u8,
}

impl Link {
    pub const PERFECT: Self = Self {
        delivery: 1.0,
        rssi: -40,
        lqi: 255,
    };
    pub const DOWN: Self = Self {
        delivery: 0.0,
        rssi: -100,
        lqi: 0,
    };
}

struct Node {
    name: String,
    hw_address: Eui64,
    /// `None` while the radio is reset/unconfigured: it neither sends nor receives
    config: Option<RadioConfig>,
    rx: Option<mpsc::Sender<RxFrame>>,
}

struct Inner {
    nodes: Vec<Node>,
    links: HashMap<(NodeId, NodeId), Link>,
    default_link: Link,
    /// Peak RSSI reported by energy scans, per channel
    noise: HashMap<u8, i8>,
    rng: u64,
}

/// A frame seen on the air, for sniffers. `psdu` has no FCS.
#[derive(Debug, Clone)]
pub struct AirFrame {
    pub psdu: Vec<u8>,
    pub channel: u8,
    pub rssi: i8,
    pub lqi: u8,
    pub time: std::time::SystemTime,
}

pub struct Medium {
    inner: Mutex<Inner>,
    epoch: Instant,
    sniffer: broadcast::Sender<AirFrame>,
}

impl Medium {
    #[must_use]
    pub fn new(seed: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                nodes: Vec::new(),
                links: HashMap::new(),
                default_link: Link::PERFECT,
                noise: HashMap::new(),
                // xorshift must not start at zero
                rng: seed | 1,
            }),
            epoch: Instant::now(),
            sniffer: broadcast::channel(4096).0,
        })
    }

    /// Add a radio to the medium. It stays deaf until configured.
    pub fn add_node(&self, name: impl Into<String>, hw_address: Eui64) -> NodeId {
        let mut inner = self.inner.lock();
        inner.nodes.push(Node {
            name: name.into(),
            hw_address,
            config: None,
            rx: None,
        });
        inner.nodes.len() - 1
    }

    #[must_use]
    pub fn hw_address(&self, node: NodeId) -> Eui64 {
        self.inner.lock().nodes[node].hw_address
    }

    #[must_use]
    pub fn name(&self, node: NodeId) -> String {
        self.inner.lock().nodes[node].name.clone()
    }

    /// The link used between nodes without an explicit [`set_link`](Self::set_link)
    pub fn set_default_link(&self, link: Link) {
        self.inner.lock().default_link = link;
    }

    /// Set the link from `from` to `to` (one direction only)
    pub fn set_link(&self, from: NodeId, to: NodeId, link: Link) {
        self.inner.lock().links.insert((from, to), link);
    }

    /// Set both directions of a link
    pub fn set_link_both(&self, a: NodeId, b: NodeId, link: Link) {
        self.set_link(a, b, link);
        self.set_link(b, a, link);
    }

    pub fn set_noise(&self, channel: u8, rssi: i8) {
        self.inner.lock().noise.insert(channel, rssi);
    }

    #[must_use]
    pub fn noise(&self, channel: u8) -> i8 {
        self.inner
            .lock()
            .noise
            .get(&channel)
            .copied()
            .unwrap_or(-100)
    }

    /// Program a radio, or `None` to model a reset
    pub fn configure(&self, node: NodeId, config: Option<RadioConfig>) {
        self.inner.lock().nodes[node].config = config;
    }

    #[must_use]
    pub fn config(&self, node: NodeId) -> Option<RadioConfig> {
        self.inner.lock().nodes[node].config.clone()
    }

    /// Change only the channel of a configured radio
    pub fn set_channel(&self, node: NodeId, channel: u8) {
        if let Some(config) = &mut self.inner.lock().nodes[node].config {
            config.channel = channel;
        }
    }

    pub fn set_promiscuous(&self, node: NodeId, promiscuous: bool) {
        if let Some(config) = &mut self.inner.lock().nodes[node].config {
            config.promiscuous = promiscuous;
        }
    }

    pub fn set_frame_pending_table(&self, node: NodeId, short: &[Nwk], extended: &[Eui64]) {
        if let Some(config) = &mut self.inner.lock().nodes[node].config {
            config.frame_pending_short = short.to_vec();
            config.frame_pending_extended = extended.to_vec();
        }
    }

    /// Redirect received frames of `node` to `tx`, replacing the previous receiver
    pub fn set_receiver(&self, node: NodeId, tx: mpsc::Sender<RxFrame>) {
        self.inner.lock().nodes[node].rx = Some(tx);
    }

    /// Put a frame on the air, with the MAC acks and retries a real radio performs.
    pub async fn transmit(&self, from: NodeId, frame: &TxFrame) -> TxResult {
        let Some(channel) = self.inner.lock().nodes[from]
            .config
            .as_ref()
            .map(|c| frame.channel.unwrap_or(c.channel))
        else {
            return TxResult::Failed;
        };

        let header = Ieee802154Frame::from_bytes_without_fcs(&frame.psdu)
            .ok()
            .map(|f| f.header().clone());
        let wants_ack = header.as_ref().is_some_and(|h| {
            h.frame_control.ack_request
                && !matches!(
                    h.dest_address,
                    None | Some(Ieee802154Address::Nwk(BROADCAST_NWK))
                )
        });
        let attempts = if wants_ack {
            1 + u32::from(frame.max_frame_retries)
        } else {
            1
        };
        let airtime =
            Duration::from_micros((frame.psdu.len() as u64 + 2 + PHY_OVERHEAD_BYTES) * US_PER_BYTE);

        for _ in 0..attempts {
            tokio::time::sleep(airtime).await;
            self.sniff(frame.psdu.clone(), channel);
            let acked = self.deliver(from, channel, frame, header.as_ref());
            if !wants_ack {
                return TxResult::Acked;
            }
            tokio::time::sleep(ACK_TIME).await;
            if acked {
                // Imm-Ack: frame control 0x0002, then the acknowledged sequence number
                if let Some(seq) = header.as_ref().and_then(|h| h.sequence_number) {
                    self.sniff(vec![0x02, 0x00, seq], channel);
                }
                return TxResult::Acked;
            }
        }
        TxResult::NoAck
    }

    /// Every frame put on the air from now on, as a sniffer sees it
    #[must_use]
    pub fn sniffer(&self) -> broadcast::Receiver<AirFrame> {
        self.sniffer.subscribe()
    }

    /// The sniffer feed itself, for a capture server
    #[must_use]
    pub fn sniffer_feed(&self) -> broadcast::Sender<AirFrame> {
        self.sniffer.clone()
    }

    fn sniff(&self, psdu: Vec<u8>, channel: u8) {
        // No sniffer connected is the normal case
        let _ = self.sniffer.send(AirFrame {
            psdu,
            channel,
            rssi: Link::PERFECT.rssi,
            lqi: Link::PERFECT.lqi,
            time: std::time::SystemTime::now(),
        });
    }

    /// Deliver one attempt to every listening radio; returns whether the addressed
    /// receiver got it and its ack made it back.
    fn deliver(
        &self,
        from: NodeId,
        channel: u8,
        frame: &TxFrame,
        header: Option<&Ieee802154FrameHeader>,
    ) -> bool {
        let timestamp_us = u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX);
        let mut inner = self.inner.lock();
        let mut acked = false;

        for to in 0..inner.nodes.len() {
            if to == from {
                continue;
            }
            let Some(config) = &inner.nodes[to].config else {
                continue;
            };
            if config.channel != channel || !config.rx_on_when_idle {
                continue;
            }
            let addressed = header.is_some_and(|h| is_addressed_to(h, config));
            if !addressed && !config.promiscuous {
                continue;
            }

            let link = inner.link(from, to);
            if !inner.roll(link.delivery) {
                continue;
            }
            if let Some(rx) = &inner.nodes[to].rx {
                // A full receive queue is a radio overrun: the frame is lost
                let _ = rx.try_send(RxFrame {
                    psdu: frame.psdu.clone(),
                    channel,
                    rssi: link.rssi,
                    lqi: link.lqi,
                    timestamp_us,
                });
            }

            let unicast = header.is_some_and(|h| {
                h.frame_control.ack_request
                    && !matches!(
                        h.dest_address,
                        None | Some(Ieee802154Address::Nwk(BROADCAST_NWK))
                    )
            });
            if addressed && unicast {
                let back = inner.link(to, from);
                acked |= inner.roll(back.delivery);
            }
        }
        acked
    }
}

impl Inner {
    fn link(&self, from: NodeId, to: NodeId) -> Link {
        self.links
            .get(&(from, to))
            .copied()
            .unwrap_or(self.default_link)
    }

    /// Bernoulli draw with xorshift64*, deterministic for a given seed
    fn roll(&mut self, probability: f64) -> bool {
        if probability >= 1.0 {
            return true;
        }
        if probability <= 0.0 {
            return false;
        }
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let value = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        #[allow(clippy::cast_precision_loss)]
        let unit = value as f64 / (1u64 << 53) as f64;
        unit < probability
    }
}

/// The 802.15.4 receive filter (PAN ID and destination address) of a non-promiscuous
/// radio. Frames without a destination address (beacons) are accepted.
fn is_addressed_to(header: &Ieee802154FrameHeader, config: &RadioConfig) -> bool {
    if let Some(pan) = header.dest_pan_id
        && pan != BROADCAST_PAN
        && pan != config.pan_id
    {
        return false;
    }
    match header.dest_address {
        None => true,
        Some(Ieee802154Address::Nwk(nwk)) => nwk == BROADCAST_NWK || nwk == config.short_address,
        Some(Ieee802154Address::Eui64(eui)) => eui == config.extended_address,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(short: u16, ext: u8) -> RadioConfig {
        RadioConfig {
            channel: 25,
            tx_power: 8,
            short_address: Nwk(short),
            extended_address: Eui64([ext; 8]),
            pan_id: PanId(0x1A62),
            promiscuous: false,
            rx_on_when_idle: true,
            frame_pending_short: Vec::new(),
            frame_pending_extended: Vec::new(),
        }
    }

    /// A data frame with short addressing and PAN ID compression
    fn data_frame(dst: u16, src: u16, ack: bool) -> TxFrame {
        let fcf: u16 = 0x0001 | (u16::from(ack) << 5) | (1 << 6) | (0b10 << 10) | (0b10 << 14);
        let mut psdu = fcf.to_le_bytes().to_vec();
        psdu.push(0x42);
        psdu.extend(0x1A62u16.to_le_bytes());
        psdu.extend(dst.to_le_bytes());
        psdu.extend(src.to_le_bytes());
        psdu.extend([0xDE, 0xAD]);
        TxFrame {
            psdu,
            channel: None,
            csma_ca: true,
            max_frame_retries: 3,
            max_csma_backoffs: 4,
            security_processed: true,
        }
    }

    fn setup() -> (Arc<Medium>, NodeId, NodeId, NodeId, mpsc::Receiver<RxFrame>) {
        let medium = Medium::new(7);
        let a = medium.add_node("a", Eui64([1; 8]));
        let b = medium.add_node("b", Eui64([2; 8]));
        let c = medium.add_node("c", Eui64([3; 8]));
        medium.configure(a, Some(config(0x0000, 1)));
        medium.configure(b, Some(config(0x1234, 2)));
        medium.configure(c, Some(config(0x5678, 3)));
        let (tx, rx) = mpsc::channel(16);
        medium.set_receiver(b, tx);
        (medium, a, b, c, rx)
    }

    #[tokio::test(start_paused = true)]
    async fn unicast_is_filtered_and_acked() {
        let (medium, a, _b, c, mut rx) = setup();
        let (tx_c, mut rx_c) = mpsc::channel(16);
        medium.set_receiver(c, tx_c);

        let result = medium.transmit(a, &data_frame(0x1234, 0x0000, true)).await;
        assert_eq!(result, TxResult::Acked);
        assert_eq!(rx.try_recv().unwrap().lqi, 255);
        // Not addressed to c, and c is not promiscuous
        assert!(rx_c.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn lost_acks_cause_retries_then_no_ack() {
        let (medium, a, b, _c, mut rx) = setup();
        // b hears a perfectly but a never hears b: the "one-way link" case
        medium.set_link(b, a, Link::DOWN);

        let result = medium.transmit(a, &data_frame(0x1234, 0x0000, true)).await;
        assert_eq!(result, TxResult::NoAck);
        // 1 attempt + 3 MAC retries, all received by b as duplicates
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn unconfigured_radio_is_deaf() {
        let (medium, a, b, _c, mut rx) = setup();
        medium.configure(b, None);
        let result = medium.transmit(a, &data_frame(0x1234, 0x0000, true)).await;
        assert_eq!(result, TxResult::NoAck);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn roll_is_deterministic_and_roughly_fair() {
        let medium = Medium::new(42);
        let hits = (0..10_000)
            .filter(|_| medium.inner.lock().roll(0.3))
            .count();
        assert!((2_700..3_300).contains(&hits), "{hits}");
    }
}
