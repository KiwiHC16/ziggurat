//! Simulated Zigbee devices: a Ziggurat router stack provisioned on the network, plus
//! an application layer answering ZDO and ZCL like the real device.
//!
//! Joining is not simulated (Ziggurat has no joiner side): devices start already on the
//! network and send a Device_annce when the coordinator opens joins, which starts the
//! same interrogation a fresh join would.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ziggurat_driver::runtime::TokioSpawner;
use ziggurat_driver::zigbee_stack::{
    ApsAck, NetworkConfig, NwkDeviceType, RouteDirective, Tunables, TxPriority, ZigbeeNotification,
    ZigbeeStack,
};
use ziggurat_ieee_802154::types::{Eui64, Nwk};

use crate::phy::SimPhy;

const PROFILE_ZDO: u16 = 0x0000;
const PROFILE_HA: u16 = 0x0104;
const COORDINATOR: Nwk = Nwk(0x0000);
const RADIUS: u8 = 30;

// ZCL data types used by the models
pub const BOOL: u8 = 0x10;
pub const BITMAP8: u8 = 0x18;
pub const UINT8: u8 = 0x20;
pub const UINT16: u8 = 0x21;
pub const UINT48: u8 = 0x25;
pub const INT16: u8 = 0x29;
pub const ENUM8: u8 = 0x30;
pub const STRING: u8 = 0x42;

// ZCL status
const SUCCESS: u8 = 0x00;
const UNSUP_CLUSTER_COMMAND: u8 = 0x81;
const UNSUP_GENERAL_COMMAND: u8 = 0x82;
const UNSUPPORTED_ATTRIBUTE: u8 = 0x86;

/// A ZCL attribute in its over-the-air encoding (little-endian, strings length-prefixed)
#[derive(Debug, Clone)]
pub struct Attribute {
    pub data_type: u8,
    pub value: Vec<u8>,
}

impl Attribute {
    #[must_use]
    pub fn uint(data_type: u8, value: u64, size: usize) -> Self {
        Self {
            data_type,
            value: value.to_le_bytes()[..size].to_vec(),
        }
    }

    #[must_use]
    pub fn string(text: &str) -> Self {
        let mut value = vec![u8::try_from(text.len()).expect("short string")];
        value.extend(text.as_bytes());
        Self {
            data_type: STRING,
            value,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub id: u8,
    pub profile: u16,
    pub device_id: u16,
    pub servers: Vec<u16>,
    pub clients: Vec<u16>,
}

/// What a device looks like on the network
#[derive(Debug, Clone)]
pub struct Model {
    pub name: &'static str,
    pub manufacturer_code: u16,
    /// MAC capability flags of the Device_annce
    pub mac_capability: u8,
    /// For remotes: the group its buttons control
    pub target_group: Option<u16>,
    pub endpoints: Vec<Endpoint>,
    /// `(endpoint, cluster, attribute)`
    pub attributes: BTreeMap<(u8, u16, u16), Attribute>,
}

impl Model {
    /// BlitzWolf BW-SHP13 (Tuya TS011F `_TZ3000_amdymr7l`), as discovered by Abeille on
    /// the real "Pompe Eau Chaude Boucle" plug
    #[must_use]
    pub fn blitzwolf_plug() -> Self {
        let mut attributes = BTreeMap::new();
        let mut set = |cluster: u16, attribute: u16, value: Attribute| {
            attributes.insert((1, cluster, attribute), value);
        };
        set(0x0000, 0x0000, Attribute::uint(UINT8, 3, 1));
        set(0x0000, 0x0001, Attribute::uint(UINT8, 74, 1));
        set(0x0000, 0x0002, Attribute::uint(UINT8, 0, 1));
        set(0x0000, 0x0003, Attribute::uint(UINT8, 1, 1));
        set(0x0000, 0x0004, Attribute::string("_TZ3000_amdymr7l"));
        set(0x0000, 0x0005, Attribute::string("TS011F"));
        set(0x0000, 0x0006, Attribute::string(""));
        set(0x0000, 0x0007, Attribute::uint(ENUM8, 1, 1));
        set(0x0000, 0xFFFD, Attribute::uint(UINT16, 2, 2));
        set(0x0006, 0x0000, Attribute::uint(BOOL, 0, 1));
        set(0x0006, 0x4001, Attribute::uint(UINT16, 0, 2));
        set(0x0006, 0x4002, Attribute::uint(UINT16, 0, 2));
        set(0x0006, 0x8001, Attribute::uint(ENUM8, 1, 1));
        set(0x0006, 0x8002, Attribute::uint(ENUM8, 2, 1));
        set(0x0702, 0x0000, Attribute::uint(UINT48, 0, 6));
        set(0x0702, 0x0200, Attribute::uint(BITMAP8, 0, 1));
        set(0x0702, 0x0300, Attribute::uint(ENUM8, 0, 1));
        set(0x0B04, 0x0505, Attribute::uint(UINT16, 227, 2));
        set(0x0B04, 0x0508, Attribute::uint(UINT16, 0, 2));
        set(0x0B04, 0x050B, Attribute::uint(INT16, 0, 2));
        Self {
            name: "TS011F__TZ3000_amdymr7l",
            manufacturer_code: 0x1002,
            // Router, mains powered, receiver on, allocate address
            mac_capability: 0x8E,
            target_group: None,
            endpoints: vec![
                Endpoint {
                    id: 0x01,
                    profile: PROFILE_HA,
                    device_id: 0x0051,
                    servers: vec![
                        0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0702, 0x0B04, 0xE000, 0xE001,
                    ],
                    clients: vec![0x0019, 0x000A],
                },
                Endpoint {
                    id: 0xF2,
                    profile: 0xA1E0,
                    device_id: 0x0061,
                    servers: vec![],
                    clients: vec![0x0021],
                },
            ],
            attributes,
        }
    }

    /// IKEA TRADFRI on/off switch (E1743), as discovered by Abeille: On/Off buttons that
    /// send to a group (firmware before 2.3.075 binds to groups only).
    ///
    /// The real remote is a sleepy end device; the simulated one runs on a router stack
    /// (Ziggurat has no end device role) and announces itself as a router.
    #[must_use]
    pub fn ikea_remote(group: u16) -> Self {
        let mut attributes = BTreeMap::new();
        let mut set = |cluster: u16, attribute: u16, value: Attribute| {
            attributes.insert((1, cluster, attribute), value);
        };
        set(0x0000, 0x0000, Attribute::uint(UINT8, 3, 1));
        set(0x0000, 0x0001, Attribute::uint(UINT8, 33, 1));
        set(0x0000, 0x0002, Attribute::uint(UINT8, 98, 1));
        set(0x0000, 0x0003, Attribute::uint(UINT8, 1, 1));
        set(0x0000, 0x0004, Attribute::string("IKEA of Sweden"));
        set(0x0000, 0x0005, Attribute::string("TRADFRI on/off switch"));
        set(0x0000, 0x0006, Attribute::string("20210722"));
        // Battery (0x03)
        set(0x0000, 0x0007, Attribute::uint(ENUM8, 3, 1));
        set(0x0000, 0x4000, Attribute::string("2.3.070"));
        set(0x0000, 0xFFFD, Attribute::uint(UINT16, 1, 2));
        // 2.9 V, 100 % (in half percent)
        set(0x0001, 0x0020, Attribute::uint(UINT8, 29, 1));
        set(0x0001, 0x0021, Attribute::uint(UINT8, 200, 1));
        set(0x0003, 0x0000, Attribute::uint(UINT16, 0, 2));
        Self {
            name: "TRADFRIonoffswitch",
            manufacturer_code: 0x117C,
            mac_capability: 0x8E,
            target_group: Some(group),
            endpoints: vec![Endpoint {
                id: 0x01,
                profile: PROFILE_HA,
                device_id: 0x0820,
                servers: vec![0x0000, 0x0001, 0x0003, 0x0009, 0x0020, 0x1000, 0xFC7C],
                clients: vec![0x0003, 0x0004, 0x0006, 0x0008, 0x0019, 0x0102, 0x1000],
            }],
            attributes,
        }
    }

    fn simple_descriptor(&self, ep: u8) -> Option<Vec<u8>> {
        let endpoint = self.endpoints.iter().find(|e| e.id == ep)?;
        let mut desc = vec![endpoint.id];
        desc.extend(endpoint.profile.to_le_bytes());
        desc.extend(endpoint.device_id.to_le_bytes());
        desc.push(0x01);
        for list in [&endpoint.servers, &endpoint.clients] {
            desc.push(u8::try_from(list.len()).ok()?);
            desc.extend(list.iter().flat_map(|c| c.to_le_bytes()));
        }
        Some(desc)
    }
}

/// Simulated power draw of the load behind the plug when it is on
const LOAD_WATTS: u64 = 44;
const LOAD_MILLIAMPS: u64 = 190;

/// Device NWK frame counters must keep growing across simulator restarts, or the
/// coordinator drops their frames as replays: start from the clock.
fn initial_frame_counter() -> u32 {
    const EPOCH_2025: u64 = 1_735_689_600;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    u32::try_from(now.saturating_sub(EPOCH_2025).saturating_mul(16)).unwrap_or(u32::MAX / 2)
}

pub struct SimDevice {
    pub stack: Arc<ZigbeeStack<SimPhy>>,
    pub ieee: Eui64,
    pub nwk: Nwk,
    model: Mutex<Model>,
    seq: Mutex<u8>,
    /// Groups the device's endpoints are members of
    groups: Mutex<BTreeSet<u16>>,
}

impl SimDevice {
    /// Start a device as a router already on the network described by `network`
    pub async fn start(
        phy: Arc<SimPhy>,
        mut network: NetworkConfig,
        ieee: Eui64,
        nwk: Nwk,
        model: Model,
    ) -> anyhow::Result<Arc<Self>> {
        network.role = NwkDeviceType::Router;
        network.network_address = nwk;
        network.ieee_address = ieee;
        network.network_key_tx_counter = initial_frame_counter();
        network.source_routing = false;
        let stack = ZigbeeStack::new(phy, network, Tunables::default(), TokioSpawner::default());
        stack
            .start_network()
            .await
            .map_err(|e| anyhow::anyhow!("device {nwk:?} start: {e}"))?;
        stack.spawn_tracked(|s| async move { s.run().await });

        let device = Arc::new(Self {
            stack,
            ieee,
            nwk,
            model: Mutex::new(model),
            // Away from the client's own counter: Abeille drops a frame whose FCF and
            // sequence number match one received from the same device in the last 2 s
            seq: Mutex::new(0x7F),
            groups: Mutex::new(BTreeSet::new()),
        });
        let worker = Arc::clone(&device);
        tokio::spawn(async move {
            loop {
                for notification in worker.stack.next_notifications().await {
                    worker.on_notification(notification);
                }
            }
        });
        Ok(device)
    }

    fn next_seq(&self) -> u8 {
        let mut seq = self.seq.lock().expect("seq lock");
        *seq = seq.wrapping_add(1);
        *seq
    }

    #[must_use]
    pub fn attribute(&self, ep: u8, cluster: u16, attribute: u16) -> Option<Attribute> {
        self.model
            .lock()
            .expect("model lock")
            .attributes
            .get(&(ep, cluster, attribute))
            .cloned()
    }

    #[allow(clippy::too_many_arguments)]
    fn send(&self, to: Nwk, profile: u16, cluster: u16, src_ep: u8, dst_ep: u8, data: Vec<u8>) {
        if let Err(e) = self.stack.send_aps_unicast(
            to,
            profile,
            cluster,
            src_ep,
            dst_ep,
            ApsAck::None,
            RADIUS,
            data,
            None,
            false,
            TxPriority::UserNormal,
            RouteDirective::StackDecides,
        ) {
            tracing::warn!("Device {:?}: send rejected: {e:?}", self.nwk);
        }
    }

    #[must_use]
    pub fn model_name(&self) -> &'static str {
        self.model.lock().expect("model lock").name
    }

    #[must_use]
    pub fn groups(&self) -> Vec<u16> {
        self.groups
            .lock()
            .expect("groups lock")
            .iter()
            .copied()
            .collect()
    }

    /// Device_annce
    pub fn announce(&self) {
        let capability = self.model.lock().expect("model lock").mac_capability;
        let mut data = vec![self.next_seq()];
        data.extend(self.nwk.to_bytes());
        data.extend(self.ieee.to_bytes());
        data.push(capability);
        tracing::info!("Device {:?} announces itself", self.nwk);
        if let Err(e) = self.stack.send_aps_broadcast(
            Nwk(0xFFFD),
            PROFILE_ZDO,
            0x0013,
            0,
            0,
            RADIUS,
            data,
            TxPriority::UserNormal,
        ) {
            tracing::warn!("Device {:?}: announce rejected: {e:?}", self.nwk);
        }
    }

    fn on_notification(&self, notification: ZigbeeNotification) {
        let ZigbeeNotification::ReceivedApsCommand {
            source,
            profile_id,
            cluster_id,
            src_ep,
            dst_ep,
            data,
            group,
            ..
        } = notification
        else {
            return;
        };
        if profile_id == PROFILE_ZDO && dst_ep == 0 {
            self.zdo(source, cluster_id, &data);
        } else if let Some(group) = group {
            // Group frames carry no endpoint: deliver to the one serving the cluster
            if self.groups.lock().expect("groups lock").contains(&group) {
                let ep = self.endpoint_serving(cluster_id);
                self.zcl(source, profile_id, cluster_id, src_ep, ep, &data, true);
            }
        } else {
            self.zcl(source, profile_id, cluster_id, src_ep, dst_ep, &data, false);
        }
    }

    fn endpoint_serving(&self, cluster: u16) -> u8 {
        let model = self.model.lock().expect("model lock");
        model
            .endpoints
            .iter()
            .find(|e| e.servers.contains(&cluster))
            .or_else(|| model.endpoints.first())
            .map_or(1, |e| e.id)
    }

    /// A button press on a remote: On (01), Off (00) or Toggle (02) to its group
    pub fn press(&self, command: u8) -> anyhow::Result<()> {
        let group = self
            .model
            .lock()
            .expect("model lock")
            .target_group
            .ok_or_else(|| anyhow::anyhow!("{:?} is not a remote", self.nwk))?;
        // Cluster-specific, client to server, no default response
        let frame = vec![0x11, self.next_seq(), command];
        tracing::info!(
            "Remote {:?}: button {command:02X} to group {group:04X}",
            self.nwk
        );
        self.stack
            .send_aps_groupcast(
                group,
                PROFILE_HA,
                0x0006,
                0x01,
                RADIUS,
                frame,
                TxPriority::UserNormal,
            )
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("groupcast rejected: {e:?}"))
    }

    fn zdo(&self, source: Nwk, cluster: u16, data: &[u8]) {
        let Some((&seq, body)) = data.split_first() else {
            return;
        };
        let nwk = self.nwk.to_bytes();
        let ours = |at: usize| body.get(at..at + 2) == Some(&nwk[..]);
        let mut rsp = vec![seq, SUCCESS];
        match cluster {
            // Mgmt_Permit_Joining_req: the coordinator opened joins, show up
            0x0036 => {
                if body.first().is_some_and(|&duration| duration > 0) {
                    self.announce();
                }
                return;
            }
            0x0000 if body.get(..8) == Some(&self.ieee.to_bytes()[..]) => {
                rsp.extend(self.ieee.to_bytes());
                rsp.extend(nwk);
            }
            0x0001 if ours(0) => {
                rsp.extend(self.ieee.to_bytes());
                rsp.extend(nwk);
            }
            0x0002 if ours(0) => {
                rsp.extend(nwk);
                let code = self.model.lock().expect("model lock").manufacturer_code;
                rsp.extend([0x01, 0x40, 0x8E]);
                rsp.extend(code.to_le_bytes());
                rsp.push(0x52);
                rsp.extend(0x0052u16.to_le_bytes());
                rsp.extend(0x2C00u16.to_le_bytes());
                rsp.extend(0x0052u16.to_le_bytes());
                rsp.push(0x00);
            }
            0x0003 if ours(0) => {
                rsp.extend(nwk);
                rsp.extend([0x10, 0xC1]);
            }
            0x0004 if ours(0) => {
                let ep = body.get(2).copied().unwrap_or(0);
                let descriptor = self.model.lock().expect("model lock").simple_descriptor(ep);
                match descriptor {
                    Some(desc) => {
                        rsp.extend(nwk);
                        rsp.push(u8::try_from(desc.len()).unwrap_or(0));
                        rsp.extend(desc);
                    }
                    None => {
                        rsp = vec![seq, 0x83];
                        rsp.extend(nwk);
                        rsp.push(0);
                    }
                }
            }
            0x0005 if ours(0) => {
                rsp.extend(nwk);
                let model = self.model.lock().expect("model lock");
                rsp.push(u8::try_from(model.endpoints.len()).unwrap_or(0));
                rsp.extend(model.endpoints.iter().map(|e| e.id));
            }
            // Bind, unbind, leave: accepted
            0x0021 | 0x0022 | 0x0034 => {}
            _ => return,
        }
        tracing::debug!("Device {:?}: ZDO {cluster:04X} answered", self.nwk);
        self.send(source, PROFILE_ZDO, cluster | 0x8000, 0, 0, rsp);
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn zcl(
        &self,
        source: Nwk,
        profile: u16,
        cluster: u16,
        src_ep: u8,
        ep: u8,
        data: &[u8],
        from_group: bool,
    ) {
        let Some(&control) = data.first() else {
            return;
        };
        let manufacturer = (control & 0x04 != 0).then(|| data.get(1..3)).flatten();
        let at = if manufacturer.is_some() { 3 } else { 1 };
        let (Some(&seq), Some(&command)) = (data.get(at), data.get(at + 1)) else {
            return;
        };
        let payload = &data[at + 2..];
        let cluster_specific = control & 0x03 == 0x01;
        let disable_default_response = control & 0x10 != 0;

        // Response headers: server to client, no default response, same manufacturer;
        // global (0x18) or cluster-specific (0x19) frame type
        let response_header = |frame_type: u8, command: u8| {
            let mut h = vec![0x18 | frame_type | (control & 0x04)];
            if let Some(code) = manufacturer {
                h.extend(code);
            }
            h.extend([seq, command]);
            h
        };
        let header = |command: u8| response_header(0x00, command);
        let cluster_header = |command: u8| response_header(0x01, command);
        // Group-addressed commands are executed but never answered
        let reply = |frame: Vec<u8>| {
            if !from_group {
                self.send(source, profile, cluster, ep, src_ep, frame);
            }
        };
        let default_response = |status: u8| {
            if !disable_default_response || status != SUCCESS {
                let mut frame = header(0x0B);
                frame.extend([command, status]);
                reply(frame);
            }
        };

        if !cluster_specific {
            match command {
                // Read attributes
                0x00 => {
                    let mut frame = header(0x01);
                    let model = self.model.lock().expect("model lock");
                    for &id in payload.as_chunks::<2>().0 {
                        let id = u16::from_le_bytes(id);
                        frame.extend(id.to_le_bytes());
                        match model.attributes.get(&(ep, cluster, id)) {
                            Some(attr) => {
                                frame.extend([SUCCESS, attr.data_type]);
                                frame.extend(&attr.value);
                            }
                            None => frame.push(UNSUPPORTED_ATTRIBUTE),
                        }
                    }
                    drop(model);
                    reply(frame);
                }
                // Write attributes: accepted as long as the attribute exists
                0x02 | 0x05 => {
                    let mut model = self.model.lock().expect("model lock");
                    let mut rest = payload;
                    while let [lo, hi, data_type, value @ ..] = rest {
                        let id = u16::from_le_bytes([*lo, *hi]);
                        let size = match *data_type {
                            BOOL | BITMAP8 | UINT8 | ENUM8 => 1,
                            UINT16 | INT16 => 2,
                            UINT48 => 6,
                            STRING => 1 + usize::from(*value.first().unwrap_or(&0)),
                            _ => break,
                        };
                        let Some(bytes) = value.get(..size) else {
                            break;
                        };
                        if let Some(attr) = model.attributes.get_mut(&(ep, cluster, id)) {
                            attr.value = bytes.to_vec();
                        }
                        rest = &value[size..];
                    }
                    drop(model);
                    if command == 0x02 {
                        let mut frame = header(0x04);
                        frame.push(SUCCESS);
                        reply(frame);
                    }
                }
                // Configure reporting: accepted (reports are sent on every change)
                0x06 => {
                    let mut frame = header(0x07);
                    frame.push(SUCCESS);
                    reply(frame);
                }
                // Discover attributes: start(2) max
                0x0C => {
                    let start = payload
                        .get(..2)
                        .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
                    let max = usize::from(payload.get(2).copied().unwrap_or(0xFF));
                    let model = self.model.lock().expect("model lock");
                    let found: Vec<_> = model
                        .attributes
                        .range((ep, cluster, start)..=(ep, cluster, 0xFFFF))
                        .map(|((_, _, id), attr)| (*id, attr.data_type))
                        .collect();
                    drop(model);
                    let mut frame = header(0x0D);
                    frame.push(u8::from(found.len() <= max));
                    for (id, data_type) in found.into_iter().take(max) {
                        frame.extend(id.to_le_bytes());
                        frame.push(data_type);
                    }
                    reply(frame);
                }
                // Responses and reports from others: nothing to say
                0x01 | 0x04 | 0x07 | 0x0A | 0x0B | 0x0D => {}
                _ => default_response(UNSUP_GENERAL_COMMAND),
            }
            return;
        }

        match (cluster, command) {
            // On/Off: Off, On, Toggle
            (0x0006, 0x00..=0x02) => {
                let on = match command {
                    0x00 => false,
                    0x01 => true,
                    _ => !self
                        .attribute(ep, 0x0006, 0x0000)
                        .is_some_and(|a| a.value != [0]),
                };
                default_response(SUCCESS);
                self.set_on_off(ep, on);
            }
            // Identify
            (0x0003, 0x00) => default_response(SUCCESS),
            // Groups: add (00), remove (03): group(2, LE) [name]
            (0x0004, 0x00 | 0x03) => {
                let Some(&[lo, hi]) = payload.first_chunk::<2>() else {
                    return default_response(0x80);
                };
                let group = u16::from_le_bytes([lo, hi]);
                let changed = {
                    let mut groups = self.groups.lock().expect("groups lock");
                    if command == 0x00 {
                        groups.insert(group)
                    } else {
                        groups.remove(&group)
                    }
                };
                tracing::info!(
                    "Device {:?}: {} group {group:04X}",
                    self.nwk,
                    if command == 0x00 {
                        "added to"
                    } else {
                        "removed from"
                    }
                );
                let mut frame = cluster_header(command);
                // NOT_FOUND (0x8B) when removing a group we are not in
                frame.push(if changed || command == 0x00 {
                    SUCCESS
                } else {
                    0x8B
                });
                frame.extend([lo, hi]);
                reply(frame);
            }
            // Get group membership: count, groups(2 each); an empty list asks for all
            (0x0004, 0x02) => {
                let asked: Vec<u16> = payload
                    .get(1..)
                    .unwrap_or_default()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&g| u16::from_le_bytes(g))
                    .collect();
                let groups: Vec<u16> = self
                    .groups()
                    .into_iter()
                    .filter(|g| asked.is_empty() || asked.contains(g))
                    .collect();
                let mut frame = cluster_header(0x02);
                // Remaining capacity, then the list
                frame.push(0xFE);
                frame.push(u8::try_from(groups.len()).unwrap_or(0));
                frame.extend(groups.iter().flat_map(|g| g.to_le_bytes()));
                reply(frame);
            }
            (0x0004, 0x04) => {
                self.groups.lock().expect("groups lock").clear();
                default_response(SUCCESS);
            }
            _ => default_response(UNSUP_CLUSTER_COMMAND),
        }
    }

    /// Switch the relay and report the new state and power, as the plug does
    pub fn set_on_off(&self, ep: u8, on: bool) {
        for (cluster, id, value) in [
            (0x0006, 0x0000, u64::from(on)),
            (0x0B04, 0x050B, if on { LOAD_WATTS } else { 0 }),
            (0x0B04, 0x0508, if on { LOAD_MILLIAMPS } else { 0 }),
        ] {
            if let Some(attr) = self
                .model
                .lock()
                .expect("model lock")
                .attributes
                .get_mut(&(ep, cluster, id))
            {
                let size = attr.value.len();
                attr.value = value.to_le_bytes()[..size].to_vec();
            }
        }
        tracing::info!(
            "Device {:?}: relay {}",
            self.nwk,
            if on { "on" } else { "off" }
        );
        self.report(ep, 0x0006, &[0x0000]);
        self.report(ep, 0x0B04, &[0x050B, 0x0508]);
    }

    /// Attribute report (0x0A) to the coordinator
    pub fn report(&self, ep: u8, cluster: u16, ids: &[u16]) {
        let mut frame = vec![0x18, self.next_seq(), 0x0A];
        for &id in ids {
            if let Some(attr) = self.attribute(ep, cluster, id) {
                frame.extend(id.to_le_bytes());
                frame.push(attr.data_type);
                frame.extend(attr.value);
            }
        }
        self.send(COORDINATOR, PROFILE_HA, cluster, ep, 0x01, frame);
    }

    /// Periodic reports, like the real plug's energy metering
    pub fn spawn_periodic_reports(self: &Arc<Self>, every: Duration) {
        let device = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                device.report(0x01, 0x0B04, &[0x0505, 0x0508, 0x050B]);
            }
        });
    }
}
