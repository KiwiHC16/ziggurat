//! The ZiGate personality: turns Abeille's commands into Ziggurat sends and the stack's
//! notifications into ZiGate messages.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use ziggurat_driver::zigbee_stack::{
    ApsAck, DeliveryError, RouteDirective, SendHandle, TxPriority, ZigbeeNotification, ZigbeeStack,
};
use ziggurat_ieee_802154::types::{Eui64, Nwk};
use ziggurat_phy::RadioPhy;

use crate::frame::Message;
use crate::msg;
use crate::state::{NetworkState, eui_to_u64};

/// LQI byte appended to messages that do not come from the air
const LOCAL_LQI: u8 = 0xFF;
/// Seconds between 1970-01-01 and 2000-01-01, the ZiGate time epoch
const ZIGATE_EPOCH: u64 = 946_684_800;
const SAVE_INTERVAL: Duration = Duration::from_secs(60);

const PROFILE_ZDO: u16 = 0x0000;
const ZDO_DEVICE_ANNCE: u16 = 0x0013;
const ZDO_MGMT_PERMIT_JOINING_REQ: u16 = 0x0036;
const BROADCAST_ROUTERS: Nwk = Nwk(0xFFFC);
const DEFAULT_RADIUS: u8 = 30;

// 0x8000 statuses (ZiGate firmware `teSL_MsgStatus`)
const STATUS_OK: u8 = 0x00;
const STATUS_BAD_PARAMETER: u8 = 0x01;
const STATUS_UNHANDLED_COMMAND: u8 = 0x02;
const STATUS_COMMAND_FAILED: u8 = 0x03;

// ZiGate addressing modes in 0x0530
const ADDR_GROUP: u8 = 0x01;
const ADDR_SHORT: u8 = 0x02;
const ADDR_BROADCAST: u8 = 0x04;
const ADDR_SHORT_NO_ACK: u8 = 0x07;

/// One outbound message for Abeille, with the LQI byte the frame carries
pub type Outbound = (Message, u8);

/// ZiGate-style status for a failed send, as reported in 0x8011/0x8702
const fn zigate_status(error: &DeliveryError) -> u8 {
    match error {
        DeliveryError::ApsAckTimeout => 0xA7,
        DeliveryError::NwkNoAck { .. } => 0xE9,
        DeliveryError::CcaFailure => 0xE1,
        DeliveryError::IndirectExpired { .. } => 0xF0,
        DeliveryError::RouteDiscoveryTimeout(_)
        | DeliveryError::RouteDiscoveryNoEntry
        | DeliveryError::RouteInactiveAfterDiscovery => 0xD0,
        _ => 0x01,
    }
}

fn seconds_since_2000() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(now.saturating_sub(ZIGATE_EPOCH)).unwrap_or(i64::MAX)
}

pub struct Bridge<P: RadioPhy> {
    stack: Arc<ZigbeeStack<P>>,
    state: Mutex<NetworkState>,
    /// `None` keeps the state in memory only (tests)
    state_path: Option<PathBuf>,
    /// The connected Abeille, if any
    out: Mutex<Option<UnboundedSender<Outbound>>>,
    /// Sequence number of 0x8000 replies
    sqn: AtomicU8,
    /// Tag matching a 0x8000 to its later 0x8011 (Abeille's `sqnAps`)
    aps_tag: AtomicU8,
    zdo_seq: AtomicU8,
    permit_join_until: Mutex<Option<Instant>>,
    /// Abeille's clock (0x0016) minus ours, in seconds
    clock_offset: Mutex<i64>,
    tx_power: Mutex<i8>,
    /// Whether this process formed a new network (0x8024 status)
    newly_formed: bool,
    allow_erase: bool,
}

impl<P: RadioPhy> Bridge<P> {
    pub fn new(
        stack: Arc<ZigbeeStack<P>>,
        state: NetworkState,
        state_path: Option<PathBuf>,
        newly_formed: bool,
        allow_erase: bool,
    ) -> Arc<Self> {
        let tx_power = state.tx_power;
        Arc::new(Self {
            stack,
            state: Mutex::new(state),
            state_path,
            out: Mutex::new(None),
            sqn: AtomicU8::new(0),
            aps_tag: AtomicU8::new(0),
            zdo_seq: AtomicU8::new(0),
            permit_join_until: Mutex::new(None),
            clock_offset: Mutex::new(0),
            tx_power: Mutex::new(tx_power),
            newly_formed,
            allow_erase,
        })
    }

    /// Spawn the notification pump and the periodic state save
    pub fn spawn_tasks(self: &Arc<Self>) {
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                for notification in bridge.stack.next_notifications().await {
                    bridge.on_notification(notification);
                }
            }
        });
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SAVE_INTERVAL).await;
                bridge.save();
            }
        });
    }

    /// Route messages to a newly connected Abeille, replacing any previous connection
    pub fn attach(&self, out: UnboundedSender<Outbound>) {
        *self.out.lock().expect("out lock") = Some(out);
    }

    fn emit_lqi(&self, message: Message, lqi: u8) {
        tracing::debug!(
            "-> {:04X} {}",
            message.msg_type,
            crate::hex(&message.payload)
        );
        let out = self.out.lock().expect("out lock");
        if let Some(out) = out.as_ref() {
            // A closed channel just means Abeille is not connected right now
            let _ = out.send((message, lqi));
        }
    }

    fn emit(&self, message: Message) {
        self.emit_lqi(message, LOCAL_LQI);
    }

    fn reply_status(&self, status: u8, sqn: u8, request: &Message) {
        self.emit(msg::status(status, sqn, request.msg_type, false, 0));
    }

    /// Persist the state now (keys, children, address map, counters)
    pub fn save(&self) {
        let mut state = self.state.lock().expect("state lock");
        state.snapshot_from(&self.stack);
        if let Some(path) = &self.state_path
            && let Err(e) = state.save(path)
        {
            tracing::error!("Failed to save the network state: {e:#}");
        }
    }

    #[must_use]
    pub fn state(&self) -> NetworkState {
        self.state.lock().expect("state lock").clone()
    }

    fn coordinator_ieee(&self) -> u64 {
        self.state
            .lock()
            .expect("state lock")
            .coordinator()
            .map(eui_to_u64)
            .unwrap_or_default()
    }

    /// Handle one command from Abeille. Every command gets exactly one 0x8000, emitted
    /// before any other message it causes: Abeille matches them strictly FIFO.
    pub fn handle(self: &Arc<Self>, request: &Message) {
        tracing::debug!(
            "<- {:04X} {}",
            request.msg_type,
            crate::hex(&request.payload)
        );
        let sqn = self.sqn.fetch_add(1, Ordering::Relaxed).wrapping_add(1);

        match request.msg_type {
            msg::RAW_APS_DATA => self.raw_aps_data(sqn, request),
            msg::GET_VERSION => {
                self.reply_status(STATUS_OK, sqn, request);
                self.emit(msg::version());
            }
            msg::START_NETWORK => {
                self.reply_status(STATUS_OK, sqn, request);
                let channel = self.state.lock().expect("state lock").channel;
                let status = if self.newly_formed { 0x01 } else { 0x00 };
                self.emit(msg::network_formed(
                    status,
                    0x0000,
                    self.coordinator_ieee(),
                    channel,
                ));
            }
            msg::GET_NETWORK_STATE => {
                self.reply_status(STATUS_OK, sqn, request);
                let state = self.state.lock().expect("state lock").clone();
                self.emit(msg::network_state(
                    0x0000,
                    self.coordinator_ieee(),
                    state.pan().map(|p| p.0).unwrap_or_default(),
                    state.extended_pan().map(eui_to_u64).unwrap_or_default(),
                    state.channel,
                ));
            }
            msg::PERMIT_JOIN => self.permit_join(sqn, request),
            msg::GET_PERMIT_JOIN => {
                self.reply_status(STATUS_OK, sqn, request);
                let open = self
                    .permit_join_until
                    .lock()
                    .expect("permit lock")
                    .is_some_and(|until| until > Instant::now());
                self.emit(msg::permit_join_status(open));
            }
            msg::SET_TIME => {
                if let Ok(bytes) = <[u8; 4]>::try_from(request.payload.as_slice()) {
                    *self.clock_offset.lock().expect("clock lock") =
                        i64::from(u32::from_be_bytes(bytes)) - seconds_since_2000();
                    self.reply_status(STATUS_OK, sqn, request);
                } else {
                    self.reply_status(STATUS_BAD_PARAMETER, sqn, request);
                }
            }
            msg::GET_TIME => {
                self.reply_status(STATUS_OK, sqn, request);
                let now = seconds_since_2000() + *self.clock_offset.lock().expect("clock lock");
                self.emit(msg::time(u32::try_from(now).unwrap_or_default()));
            }
            msg::SET_TX_POWER => {
                // Recorded and reported; the radio keeps the power it was started with
                if let [power] = request.payload[..] {
                    *self.tx_power.lock().expect("power lock") = power.cast_signed();
                }
                self.reply_status(STATUS_OK, sqn, request);
                let power = *self.tx_power.lock().expect("power lock");
                self.emit(msg::tx_power(msg::TX_POWER_SET, power));
            }
            msg::GET_TX_POWER => {
                self.reply_status(STATUS_OK, sqn, request);
                let power = *self.tx_power.lock().expect("power lock");
                self.emit(msg::tx_power(msg::TX_POWER, power));
            }
            msg::ERASE_PDM if !self.allow_erase => {
                tracing::warn!(
                    "Abeille asked to erase the PDM (0x0012): refused, the network is kept \
                     (start with --allow-erase to permit it)"
                );
                self.reply_status(STATUS_COMMAND_FAILED, sqn, request);
            }
            msg::ERASE_PDM => {
                tracing::warn!("Erase PDM accepted: not implemented yet, the network is kept");
                self.reply_status(STATUS_COMMAND_FAILED, sqn, request);
            }
            // Accepted and meaningless for Ziggurat: the network is formed at startup on
            // the configured channel, always as coordinator, always in RAW mode
            msg::SET_MODE
            | msg::SET_CERTIFICATION
            | msg::SET_DEVICE_TYPE
            | msg::SET_CHANNEL_MASK
            | msg::SET_LED
            | msg::RESET
            | msg::GET_DEVICE_LIST => self.reply_status(STATUS_OK, sqn, request),
            other => {
                tracing::info!("Unhandled ZiGate command {other:04X}");
                self.reply_status(STATUS_UNHANDLED_COMMAND, sqn, request);
            }
        }
    }

    /// 0x0530: `addr_mode dst(2) src_ep dst_ep cluster(2) profile(2) security radius
    /// len data`
    fn raw_aps_data(self: &Arc<Self>, sqn: u8, request: &Message) {
        let Some((head, data)) = request.payload.split_at_checked(12) else {
            return self.reply_status(STATUS_BAD_PARAMETER, sqn, request);
        };
        let [
            mode,
            d0,
            d1,
            src_ep,
            dst_ep,
            c0,
            c1,
            p0,
            p1,
            _security,
            radius,
            len,
        ] = <[u8; 12]>::try_from(head).expect("split at 12");
        if data.len() != usize::from(len) {
            return self.reply_status(STATUS_BAD_PARAMETER, sqn, request);
        }
        let dst = u16::from_be_bytes([d0, d1]);
        let cluster = u16::from_be_bytes([c0, c1]);
        let profile = u16::from_be_bytes([p0, p1]);
        let radius = if radius == 0 { DEFAULT_RADIUS } else { radius };
        let data = data.to_vec();

        let (sent, aps_ack) = match mode {
            ADDR_SHORT | ADDR_SHORT_NO_ACK => {
                let ack = mode == ADDR_SHORT;
                let result = self.stack.send_aps_unicast(
                    Nwk(dst),
                    profile,
                    cluster,
                    src_ep,
                    dst_ep,
                    if ack { ApsAck::Request } else { ApsAck::None },
                    radius,
                    data,
                    None,
                    false,
                    TxPriority::UserNormal,
                    RouteDirective::StackDecides,
                );
                (result, ack)
            }
            ADDR_GROUP => (
                self.stack.send_aps_groupcast(
                    dst,
                    profile,
                    cluster,
                    src_ep,
                    radius,
                    data,
                    TxPriority::UserNormal,
                ),
                false,
            ),
            ADDR_BROADCAST => (
                self.stack.send_aps_broadcast(
                    Nwk(dst),
                    profile,
                    cluster,
                    src_ep,
                    dst_ep,
                    radius,
                    data,
                    TxPriority::UserNormal,
                ),
                false,
            ),
            other => {
                tracing::info!("0530 addressing mode {other:02X} not supported");
                return self.reply_status(STATUS_BAD_PARAMETER, sqn, request);
            }
        };

        let handle = match sent {
            Ok(handle) => handle,
            Err(e) => {
                tracing::warn!("0530 to {dst:04X} rejected: {e:?}");
                return self.reply_status(STATUS_COMMAND_FAILED, sqn, request);
            }
        };

        let tag = self.aps_tag.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        self.emit(msg::status(STATUS_OK, sqn, request.msg_type, true, tag));
        self.track(handle, aps_ack, dst, src_ep, dst_ep, cluster, tag);
    }

    /// Report the outcome of a send: 0x8011 when Abeille waits for an APS ack, 0x8702
    /// (logged only by Abeille) for a failed send without one.
    #[allow(clippy::too_many_arguments)]
    fn track(
        self: &Arc<Self>,
        handle: SendHandle,
        aps_ack: bool,
        dst: u16,
        src_ep: u8,
        dst_ep: u8,
        cluster: u16,
        tag: u8,
    ) {
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            let mut result = handle.handed_off().await;
            if aps_ack && result.is_ok() {
                result = handle.delivered().await;
            }
            match (aps_ack, result) {
                (true, Ok(())) => bridge.emit(msg::aps_ack(STATUS_OK, dst, dst_ep, cluster, tag)),
                (true, Err(e)) => {
                    tracing::info!("No APS ack from {dst:04X}: {e}");
                    bridge.emit(msg::aps_ack(zigate_status(&e), dst, dst_ep, cluster, tag));
                }
                (false, Ok(())) => {}
                (false, Err(e)) => {
                    tracing::info!("Send to {dst:04X} failed: {e}");
                    bridge.emit(msg::aps_failure(
                        zigate_status(&e),
                        src_ep,
                        dst_ep,
                        dst,
                        tag,
                    ));
                }
            }
        });
    }

    /// 0x0049: `target(2) duration tc_significance`. Opens our own join window and
    /// tells the routers with a Mgmt_Permit_Joining_req broadcast.
    fn permit_join(self: &Arc<Self>, sqn: u8, request: &Message) {
        let [_, _, duration, tc_significance] = request.payload[..] else {
            return self.reply_status(STATUS_BAD_PARAMETER, sqn, request);
        };
        self.stack.permit_joins(u64::from(duration), true);
        *self.permit_join_until.lock().expect("permit lock") =
            (duration != 0).then(|| Instant::now() + Duration::from_secs(u64::from(duration)));

        let seq = self.zdo_seq.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self.stack.send_aps_broadcast(
            BROADCAST_ROUTERS,
            PROFILE_ZDO,
            ZDO_MGMT_PERMIT_JOINING_REQ,
            0,
            0,
            DEFAULT_RADIUS,
            vec![seq, duration, tc_significance],
            TxPriority::UserNormal,
        ) {
            tracing::warn!("Mgmt_Permit_Joining_req broadcast rejected: {e:?}");
        }
        self.reply_status(STATUS_OK, sqn, request);
    }

    fn on_notification(&self, notification: ZigbeeNotification) {
        match notification {
            ZigbeeNotification::ReceivedApsCommand {
                source,
                destination,
                group,
                profile_id,
                cluster_id,
                src_ep,
                dst_ep,
                lqi,
                data,
                ..
            } => {
                self.emit_lqi(
                    msg::data_indication(&msg::Indication {
                        profile: profile_id,
                        cluster: cluster_id,
                        src_ep,
                        dst_ep,
                        src_nwk: source.0,
                        dst_nwk: destination.0,
                        dst_group: group,
                        asdu: &data,
                    }),
                    lqi,
                );
                if profile_id == PROFILE_ZDO && cluster_id == ZDO_DEVICE_ANNCE {
                    self.device_announce(&data, lqi);
                }
            }
            ZigbeeNotification::DeviceJoined { nwk, ieee, .. } => {
                tracing::info!("Device {ieee:?} joined as {nwk:?}");
                self.save();
            }
            ZigbeeNotification::DeviceLeft { nwk, ieee, reason } => {
                tracing::info!("Device {nwk:?} ({ieee:?}) left: {reason:?}");
                if let Some(ieee) = ieee {
                    self.emit(msg::leave_indication(eui_to_u64(ieee), false));
                }
                self.save();
            }
            ZigbeeNotification::FrameCounterUpdate { frame_counter } => {
                self.state.lock().expect("state lock").nwk_frame_counter = frame_counter;
                self.save();
            }
            ZigbeeNotification::ApsFrameCounterUpdate { frame_counter } => {
                self.state.lock().expect("state lock").aps_frame_counter = frame_counter;
                self.save();
            }
            ZigbeeNotification::LinkKeyUpdate { ieee, .. } => {
                tracing::info!("New link key for {ieee:?}");
                self.save();
            }
            other => tracing::debug!("Notification: {other:?}"),
        }
    }

    /// Device_annce: `seq nwk(2, LE) ieee(8, LE) capability` → 0x004D
    fn device_announce(&self, data: &[u8], lqi: u8) {
        let [_seq, n0, n1, ieee @ .., capability] = data else {
            return;
        };
        let Ok(ieee) = <[u8; 8]>::try_from(ieee) else {
            return;
        };
        let nwk = u16::from_le_bytes([*n0, *n1]);
        tracing::info!("Device announce: {nwk:04X} {:?}", Eui64(ieee));
        self.emit_lqi(
            msg::device_announce(nwk, eui_to_u64(Eui64(ieee)), *capability),
            lqi,
        );
        self.save();
    }
}
