//! ZiGate "native" commands translated to the ZDO/ZCL frames the ZiGate firmware would
//! have built (`app_Znc_cmds.c` in the official 3.23 sources).
//!
//! Abeille runs the ZiGate in RAW mode but still sends these for discovery, polling and
//! simple actuators. Their answers come back as ordinary 0x8002 indications.
//! Native fields are big-endian; the frames built here are little-endian, as on the air.

use crate::frame::Message;
use crate::msg;

pub const PROFILE_ZDO: u16 = 0x0000;

/// Where a translated frame goes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Unicast { nwk: u16, aps_ack: bool },
    Group(u16),
    Broadcast(u16),
}

/// An over-the-air frame to send for a native command
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub target: Target,
    pub profile: u16,
    pub cluster: u16,
    pub src_ep: u8,
    pub dst_ep: u8,
    pub asdu: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Translation {
    Send(Frame),
    /// Mgmt_Lqi_req for the coordinator itself: answered from the stack's tables
    LocalNeighborTable {
        start_index: u8,
    },
    /// Binding on the coordinator itself: accepted, nothing to send (the coordinator
    /// receives everything addressed to it anyway)
    LocalBind,
    /// The command needs the NWK address of this IEEE, unknown to the address map
    UnknownDevice(u64),
    Malformed,
    Unsupported,
}

/// Sequence numbers and address lookups the translation needs from the bridge
pub trait Context {
    fn next_zdo_seq(&mut self) -> u8;
    fn next_zcl_seq(&mut self) -> u8;
    /// NWK address of a device, by its IEEE in the ZiGate big-endian form
    fn nwk_of(&self, ieee: u64) -> Option<u16>;
    fn coordinator_ieee(&self) -> u64;
}

// ZDO request clusters
const NWK_ADDR_REQ: u16 = 0x0000;
const IEEE_ADDR_REQ: u16 = 0x0001;
const NODE_DESC_REQ: u16 = 0x0002;
const SIMPLE_DESC_REQ: u16 = 0x0004;
const ACTIVE_EP_REQ: u16 = 0x0005;
const BIND_REQ: u16 = 0x0021;
const UNBIND_REQ: u16 = 0x0022;
const MGMT_LQI_REQ: u16 = 0x0031;
const MGMT_LEAVE_REQ: u16 = 0x0034;

// ZCL frame control
const FC_CLUSTER_SPECIFIC: u8 = 0x01;
const FC_MANUFACTURER_SPECIFIC: u8 = 0x04;
const FC_SERVER_TO_CLIENT: u8 = 0x08;
const FC_DISABLE_DEFAULT_RESPONSE: u8 = 0x10;

// ZCL global commands
const ZCL_READ_ATTRIBUTES: u8 = 0x00;
const ZCL_WRITE_ATTRIBUTES: u8 = 0x02;
const ZCL_READ_REPORTING_CONFIGURATION: u8 = 0x08;
const ZCL_DISCOVER_ATTRIBUTES: u8 = 0x0C;

// Clusters
const CLUSTER_IDENTIFY: u16 = 0x0003;
const CLUSTER_GROUPS: u16 = 0x0004;
const CLUSTER_ON_OFF: u16 = 0x0006;
const CLUSTER_LEVEL: u16 = 0x0008;
const CLUSTER_WINDOW_COVERING: u16 = 0x0102;
const CLUSTER_COLOR: u16 = 0x0300;

const BROADCAST_MIN: u16 = 0xFFF8;

fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]))
}

fn be64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// Size of a fixed-length ZCL attribute value, `None` for strings and unknown types
const fn zcl_type_size(data_type: u8) -> Option<usize> {
    Some(match data_type {
        0x08 | 0x10 | 0x18 | 0x20 | 0x28 | 0x30 => 1,
        0x09 | 0x19 | 0x21 | 0x29 | 0x31 | 0x38 | 0xE8 | 0xE9 => 2,
        0x0A | 0x1A | 0x22 | 0x2A => 3,
        0x0B | 0x1B | 0x23 | 0x2B | 0x39 | 0xE0 | 0xE1 | 0xE2 | 0xEA => 4,
        0x0C | 0x1C | 0x24 | 0x2C => 5,
        0x0D | 0x1D | 0x25 | 0x2D => 6,
        0x0E | 0x1E | 0x26 | 0x2E => 7,
        0x0F | 0x1F | 0x27 | 0x2F | 0x3A | 0xF0 => 8,
        0xF1 => 16,
        _ => return None,
    })
}

/// A ZCL frame: `frame_control [manufacturer] seq command payload`
fn zcl(control: u8, manufacturer: Option<u16>, seq: u8, command: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![control | manufacturer.map_or(0, |_| FC_MANUFACTURER_SPECIFIC)];
    if let Some(code) = manufacturer {
        frame.extend(code.to_le_bytes());
    }
    frame.push(seq);
    frame.push(command);
    frame.extend_from_slice(payload);
    frame
}

/// The ZCL address prefix of native commands: `mode addr(2) src_ep dst_ep`
fn zcl_target(payload: &[u8]) -> Option<(Target, u8, u8)> {
    let mode = *payload.first()?;
    let addr = be16(payload, 1)?;
    let target = match mode {
        0x01 => Target::Group(addr),
        0x02 | 0x03 => Target::Unicast {
            nwk: addr,
            aps_ack: true,
        },
        0x04 => Target::Broadcast(addr),
        0x07 | 0x08 => Target::Unicast {
            nwk: addr,
            aps_ack: false,
        },
        _ => return None,
    };
    Some((target, *payload.get(3)?, *payload.get(4)?))
}

const fn zdo_unicast(nwk: u16, aps_ack: bool) -> Target {
    if nwk >= BROADCAST_MIN {
        Target::Broadcast(nwk)
    } else {
        Target::Unicast { nwk, aps_ack }
    }
}

const fn zdo(target: Target, cluster: u16, asdu: Vec<u8>) -> Translation {
    Translation::Send(Frame {
        target,
        profile: PROFILE_ZDO,
        cluster,
        src_ep: 0,
        dst_ep: 0,
        asdu,
    })
}

fn zcl_frame(
    payload: &[u8],
    cluster: u16,
    control: u8,
    manufacturer: Option<u16>,
    seq: u8,
    command: u8,
    body: &[u8],
) -> Translation {
    let Some((target, src_ep, dst_ep)) = zcl_target(payload) else {
        return Translation::Malformed;
    };
    Translation::Send(Frame {
        target,
        profile: 0x0104,
        cluster,
        src_ep,
        dst_ep,
        asdu: zcl(control, manufacturer, seq, command, body),
    })
}

/// Translate one native command. `None` means it is not a native over-the-air command.
#[allow(clippy::too_many_lines)]
pub fn translate(message: &Message, ctx: &mut impl Context) -> Option<Translation> {
    let p = message.payload.as_slice();
    let malformed = || Some(Translation::Malformed);

    let translation = match message.msg_type {
        // ---- ZDO ----
        msg::NWK_ADDRESS => {
            // target(2) ieee(8) request_type start_index
            let (Some(target), Some(ieee), Some(&kind), Some(&start)) =
                (be16(p, 0), be64(p, 2), p.get(10), p.get(11))
            else {
                return malformed();
            };
            let mut asdu = vec![ctx.next_zdo_seq()];
            asdu.extend(ieee.to_le_bytes());
            asdu.extend([kind, start]);
            zdo(zdo_unicast(target, false), NWK_ADDR_REQ, asdu)
        }
        msg::IEEE_ADDRESS => {
            // target(2) nwk_of_interest(2) request_type start_index
            let (Some(target), Some(nwk), Some(&kind), Some(&start)) =
                (be16(p, 0), be16(p, 2), p.get(4), p.get(5))
            else {
                return malformed();
            };
            let mut asdu = vec![ctx.next_zdo_seq()];
            asdu.extend(nwk.to_le_bytes());
            asdu.extend([kind, start]);
            zdo(zdo_unicast(target, false), IEEE_ADDR_REQ, asdu)
        }
        msg::NODE_DESCRIPTOR | msg::ACTIVE_ENDPOINTS => {
            let Some(target) = be16(p, 0) else {
                return malformed();
            };
            let mut asdu = vec![ctx.next_zdo_seq()];
            asdu.extend(target.to_le_bytes());
            let cluster = if message.msg_type == msg::NODE_DESCRIPTOR {
                NODE_DESC_REQ
            } else {
                ACTIVE_EP_REQ
            };
            zdo(zdo_unicast(target, true), cluster, asdu)
        }
        msg::SIMPLE_DESCRIPTOR => {
            let (Some(target), Some(&ep)) = (be16(p, 0), p.get(2)) else {
                return malformed();
            };
            let mut asdu = vec![ctx.next_zdo_seq()];
            asdu.extend(target.to_le_bytes());
            asdu.push(ep);
            zdo(zdo_unicast(target, true), SIMPLE_DESC_REQ, asdu)
        }
        msg::MGMT_LQI => {
            let (Some(target), Some(&start)) = (be16(p, 0), p.get(2)) else {
                return malformed();
            };
            if target == 0x0000 {
                return Some(Translation::LocalNeighborTable { start_index: start });
            }
            zdo(
                zdo_unicast(target, true),
                MGMT_LQI_REQ,
                vec![ctx.next_zdo_seq(), start],
            )
        }
        msg::BIND | msg::UNBIND => {
            // src_ieee(8) src_ep cluster(2) dst_mode, then group(2) or ieee(8) dst_ep
            let (Some(src_ieee), Some(&src_ep), Some(cluster), Some(&mode)) =
                (be64(p, 0), p.get(8), be16(p, 9), p.get(11))
            else {
                return malformed();
            };
            let destination: Vec<u8> = match mode {
                0x01 => match be16(p, 12) {
                    Some(group) => group.to_le_bytes().to_vec(),
                    None => return malformed(),
                },
                0x03 => match (be64(p, 12), p.get(20)) {
                    (Some(ieee), Some(&ep)) => {
                        let mut d = ieee.to_le_bytes().to_vec();
                        d.push(ep);
                        d
                    }
                    _ => return malformed(),
                },
                _ => return malformed(),
            };
            if src_ieee == ctx.coordinator_ieee() {
                return Some(Translation::LocalBind);
            }
            let Some(nwk) = ctx.nwk_of(src_ieee) else {
                return Some(Translation::UnknownDevice(src_ieee));
            };
            let mut asdu = vec![ctx.next_zdo_seq()];
            asdu.extend(src_ieee.to_le_bytes());
            asdu.push(src_ep);
            asdu.extend(cluster.to_le_bytes());
            asdu.push(mode);
            asdu.extend(destination);
            let zdo_cluster = if message.msg_type == msg::BIND {
                BIND_REQ
            } else {
                UNBIND_REQ
            };
            zdo(zdo_unicast(nwk, true), zdo_cluster, asdu)
        }
        msg::MGMT_LEAVE => {
            // target(2) ieee(8) rejoin remove_children
            let (Some(target), Some(ieee), Some(&rejoin), Some(&children)) =
                (be16(p, 0), be64(p, 2), p.get(10), p.get(11))
            else {
                return malformed();
            };
            let mut asdu = vec![ctx.next_zdo_seq()];
            asdu.extend(ieee.to_le_bytes());
            asdu.push((u8::from(children != 0) << 6) | (u8::from(rejoin != 0) << 7));
            zdo(zdo_unicast(target, false), MGMT_LEAVE_REQ, asdu)
        }
        msg::LEAVE_REQUEST => {
            // ieee(8) rejoin remove_children: ask that device to leave
            let (Some(ieee), Some(&rejoin), Some(&children)) = (be64(p, 0), p.get(8), p.get(9))
            else {
                return malformed();
            };
            let Some(nwk) = ctx.nwk_of(ieee) else {
                return Some(Translation::UnknownDevice(ieee));
            };
            let mut asdu = vec![ctx.next_zdo_seq()];
            asdu.extend(ieee.to_le_bytes());
            asdu.push((u8::from(children != 0) << 6) | (u8::from(rejoin != 0) << 7));
            zdo(zdo_unicast(nwk, false), MGMT_LEAVE_REQ, asdu)
        }

        // ---- ZCL, common prefix: mode addr(2) src_ep dst_ep ----
        msg::READ_ATTRIBUTE => {
            // cluster(2)@5 direction@7 manuf_specific@8 manuf(2)@9 count@11 ids(2 each)@12
            let (Some(cluster), Some(&dir), Some(&manuf_specific), Some(manuf), Some(&count)) =
                (be16(p, 5), p.get(7), p.get(8), be16(p, 9), p.get(11))
            else {
                return malformed();
            };
            let Some(ids) = p.get(12..12 + 2 * usize::from(count)) else {
                return malformed();
            };
            let body: Vec<u8> = ids
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|&[hi, lo]| [lo, hi])
                .collect();
            let control =
                FC_DISABLE_DEFAULT_RESPONSE | if dir != 0 { FC_SERVER_TO_CLIENT } else { 0 };
            zcl_frame(
                p,
                cluster,
                control,
                (manuf_specific != 0).then_some(manuf),
                ctx.next_zcl_seq(),
                ZCL_READ_ATTRIBUTES,
                &body,
            )
        }
        msg::WRITE_ATTRIBUTE => {
            // cluster(2)@5 direction@7 manuf_specific@8 manuf(2)@9 count@11 records@12:
            // id(2) type value (numeric big-endian, strings length-prefixed)
            let (Some(cluster), Some(&dir), Some(&manuf_specific), Some(manuf)) =
                (be16(p, 5), p.get(7), p.get(8), be16(p, 9))
            else {
                return malformed();
            };
            let mut body = Vec::new();
            let mut rest = p.get(12..).unwrap_or_default();
            while !rest.is_empty() {
                let (Some(id), Some(&data_type)) = (be16(rest, 0), rest.get(2)) else {
                    return malformed();
                };
                body.extend(id.to_le_bytes());
                body.push(data_type);
                let value_len = match zcl_type_size(data_type) {
                    Some(size) => size,
                    // Octet/character strings: length byte then the bytes, as-is
                    None if matches!(data_type, 0x41 | 0x42) => match rest.get(3) {
                        Some(&len) => 1 + usize::from(len),
                        None => return malformed(),
                    },
                    None => return Some(Translation::Unsupported),
                };
                let Some(value) = rest.get(3..3 + value_len) else {
                    return malformed();
                };
                if zcl_type_size(data_type).is_some() {
                    body.extend(value.iter().rev());
                } else {
                    body.extend_from_slice(value);
                }
                rest = &rest[3 + value_len..];
            }
            let control =
                FC_DISABLE_DEFAULT_RESPONSE | if dir != 0 { FC_SERVER_TO_CLIENT } else { 0 };
            zcl_frame(
                p,
                cluster,
                control,
                (manuf_specific != 0).then_some(manuf),
                ctx.next_zcl_seq(),
                ZCL_WRITE_ATTRIBUTES,
                &body,
            )
        }
        msg::READ_REPORTING_CONFIG => {
            // cluster(2)@5 direction@7 count@8 manuf_specific@9 manuf(2)@10
            // records@12: direction id(2)
            let (Some(cluster), Some(&dir), Some(&count), Some(&manuf_specific), Some(manuf)) =
                (be16(p, 5), p.get(7), p.get(8), p.get(9), be16(p, 10))
            else {
                return malformed();
            };
            let Some(records) = p.get(12..12 + 3 * usize::from(count)) else {
                return malformed();
            };
            let body: Vec<u8> = records
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|&[dir, hi, lo]| [dir, lo, hi])
                .collect();
            let control =
                FC_DISABLE_DEFAULT_RESPONSE | if dir != 0 { FC_SERVER_TO_CLIENT } else { 0 };
            zcl_frame(
                p,
                cluster,
                control,
                (manuf_specific != 0).then_some(manuf),
                ctx.next_zcl_seq(),
                ZCL_READ_REPORTING_CONFIGURATION,
                &body,
            )
        }
        msg::DISCOVER_ATTRIBUTES => {
            // cluster(2)@5 start(2)@7 direction@9 manuf_specific@10 manuf(2)@11 max@13
            let (
                Some(cluster),
                Some(start),
                Some(&dir),
                Some(&manuf_specific),
                Some(manuf),
                Some(&max),
            ) = (
                be16(p, 5),
                be16(p, 7),
                p.get(9),
                p.get(10),
                be16(p, 11),
                p.get(13),
            )
            else {
                return malformed();
            };
            let mut body = start.to_le_bytes().to_vec();
            body.push(max);
            let control =
                FC_DISABLE_DEFAULT_RESPONSE | if dir != 0 { FC_SERVER_TO_CLIENT } else { 0 };
            zcl_frame(
                p,
                cluster,
                control,
                (manuf_specific != 0).then_some(manuf),
                ctx.next_zcl_seq(),
                ZCL_DISCOVER_ATTRIBUTES,
                &body,
            )
        }

        // Cluster commands keep the default response the ZiGate asks for
        msg::ON_OFF => {
            let Some(&command) = p.get(5) else {
                return malformed();
            };
            zcl_frame(
                p,
                CLUSTER_ON_OFF,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                command,
                &[],
            )
        }
        msg::ON_OFF_TIMED => {
            // on_off@5 on_time(2)@6 off_time(2)@8 → On With Timed Off (0x42)
            let (Some(&on_off), Some(on), Some(off)) = (p.get(5), be16(p, 6), be16(p, 8)) else {
                return malformed();
            };
            let mut body = vec![on_off];
            body.extend(on.to_le_bytes());
            body.extend(off.to_le_bytes());
            zcl_frame(
                p,
                CLUSTER_ON_OFF,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                0x42,
                &body,
            )
        }
        msg::MOVE_TO_LEVEL_ONOFF => {
            // with_on_off@5 level@6 transition(2)@7 → Move To Level (0x00 / 0x04)
            let (Some(&with_on_off), Some(&level), Some(transition)) =
                (p.get(5), p.get(6), be16(p, 7))
            else {
                return malformed();
            };
            let mut body = vec![level];
            body.extend(transition.to_le_bytes());
            let command = if with_on_off != 0 { 0x04 } else { 0x00 };
            zcl_frame(
                p,
                CLUSTER_LEVEL,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                command,
                &body,
            )
        }
        msg::LEVEL_STEP => {
            // with_on_off@5 mode@6 size@7 transition(2)@8 → Step (0x02 / 0x06)
            let (Some(&with_on_off), Some(&mode), Some(&size), Some(transition)) =
                (p.get(5), p.get(6), p.get(7), be16(p, 8))
            else {
                return malformed();
            };
            let mut body = vec![mode, size];
            body.extend(transition.to_le_bytes());
            let command = if with_on_off != 0 { 0x06 } else { 0x02 };
            zcl_frame(
                p,
                CLUSTER_LEVEL,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                command,
                &body,
            )
        }
        msg::LEVEL_STOP => {
            // with_on_off@5 [options_mask@6 options_override@7] → Stop (0x03 / 0x07)
            let Some(&with_on_off) = p.get(5) else {
                return malformed();
            };
            let body: Vec<u8> = p.get(6..8).map(<[u8]>::to_vec).unwrap_or_default();
            let command = if with_on_off != 0 { 0x07 } else { 0x03 };
            zcl_frame(
                p,
                CLUSTER_LEVEL,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                command,
                &body,
            )
        }
        msg::MOVE_TO_COLOUR => {
            // x(2)@5 y(2)@7 transition(2)@9 → Move To Color (0x07)
            let (Some(x), Some(y), Some(t)) = (be16(p, 5), be16(p, 7), be16(p, 9)) else {
                return malformed();
            };
            let body: Vec<u8> = [x, y, t].iter().flat_map(|v| v.to_le_bytes()).collect();
            zcl_frame(
                p,
                CLUSTER_COLOR,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                0x07,
                &body,
            )
        }
        msg::MOVE_TO_COLOUR_TEMPERATURE => {
            // mireds(2)@5 transition(2)@7 → Move To Color Temperature (0x0A)
            let (Some(mireds), Some(t)) = (be16(p, 5), be16(p, 7)) else {
                return malformed();
            };
            let body: Vec<u8> = [mireds, t].iter().flat_map(|v| v.to_le_bytes()).collect();
            zcl_frame(
                p,
                CLUSTER_COLOR,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                0x0A,
                &body,
            )
        }
        msg::IDENTIFY => {
            let Some(time) = be16(p, 5) else {
                return malformed();
            };
            zcl_frame(
                p,
                CLUSTER_IDENTIFY,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                0x00,
                &time.to_le_bytes(),
            )
        }
        msg::ADD_GROUP | msg::REMOVE_GROUP => {
            let Some(group) = be16(p, 5) else {
                return malformed();
            };
            let (command, body) = if message.msg_type == msg::ADD_GROUP {
                // Group name: empty string
                let mut body = group.to_le_bytes().to_vec();
                body.push(0);
                (0x00, body)
            } else {
                (0x03, group.to_le_bytes().to_vec())
            };
            zcl_frame(
                p,
                CLUSTER_GROUPS,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                command,
                &body,
            )
        }
        msg::GET_GROUP_MEMBERSHIP => {
            // count@5 groups(2 each)@6
            let Some(&count) = p.get(5) else {
                return malformed();
            };
            let Some(groups) = p.get(6..6 + 2 * usize::from(count)) else {
                return malformed();
            };
            let mut body = vec![count];
            body.extend(
                groups
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|&[hi, lo]| [lo, hi]),
            );
            zcl_frame(
                p,
                CLUSTER_GROUPS,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                0x02,
                &body,
            )
        }
        msg::REMOVE_ALL_GROUPS => zcl_frame(
            p,
            CLUSTER_GROUPS,
            FC_CLUSTER_SPECIFIC,
            None,
            ctx.next_zcl_seq(),
            0x04,
            &[],
        ),
        msg::WINDOW_COVERING => {
            // command@5, then lift/tilt value(2)@6 (0x04/0x07) or percentage@6 (0x05/0x08)
            let Some(&command) = p.get(5) else {
                return malformed();
            };
            let body = match command {
                0x00..=0x02 => Vec::new(),
                0x04 | 0x07 => match be16(p, 6) {
                    Some(v) => v.to_le_bytes().to_vec(),
                    None => return malformed(),
                },
                0x05 | 0x08 => match p.get(6) {
                    Some(&pct) => vec![pct],
                    None => return malformed(),
                },
                _ => return Some(Translation::Unsupported),
            };
            zcl_frame(
                p,
                CLUSTER_WINDOW_COVERING,
                FC_CLUSTER_SPECIFIC,
                None,
                ctx.next_zcl_seq(),
                command,
                &body,
            )
        }
        _ => return None,
    };
    Some(translation)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ctx {
        seq: u8,
    }

    impl Context for Ctx {
        fn next_zdo_seq(&mut self) -> u8 {
            self.seq += 1;
            self.seq
        }
        fn next_zcl_seq(&mut self) -> u8 {
            self.seq += 1;
            self.seq
        }
        fn nwk_of(&self, ieee: u64) -> Option<u16> {
            (ieee == 0xA4C1_388F_3FCA_7281).then_some(0x2AAE)
        }
        fn coordinator_ieee(&self) -> u64 {
            0x0012_4B00_00ED_0001
        }
    }

    fn translate_hex(msg_type: u16, payload: &str) -> Translation {
        let payload = (0..payload.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&payload[i..i + 2], 16).unwrap())
            .collect();
        translate(&Message { msg_type, payload }, &mut Ctx { seq: 0x10 }).expect("native command")
    }

    fn sent(t: Translation) -> Frame {
        match t {
            Translation::Send(frame) => frame,
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn read_attribute_as_abeille_builds_it() {
        // Abeille readAttribute: mode 02, 2AAE, ep 01→01, cluster 0000, dir 00, no manuf,
        // 3 attributes 0004 0005 4000
        let f = sent(translate_hex(
            0x0100,
            "022AAE0101000000000000030004000540 00"
                .replace(' ', "")
                .as_str(),
        ));
        assert_eq!(
            f.target,
            Target::Unicast {
                nwk: 0x2AAE,
                aps_ack: true
            }
        );
        assert_eq!(
            (f.profile, f.cluster, f.src_ep, f.dst_ep),
            (0x0104, 0x0000, 1, 1)
        );
        assert_eq!(
            f.asdu,
            [0x10, 0x11, 0x00, 0x04, 0x00, 0x05, 0x00, 0x00, 0x40]
        );
    }

    #[test]
    fn manufacturer_specific_read() {
        let f = sent(translate_hex(0x0100, "022AAE0101FC0000011037010001"));
        // FCF 0x14, manufacturer 0x1037 LE, seq, command 00, attribute 0x0001 LE
        assert_eq!(f.cluster, 0xFC00);
        assert_eq!(f.asdu, [0x14, 0x37, 0x10, 0x11, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn write_attribute_swaps_numeric_values_only() {
        // uint16 (0x21) 0x1234 → LE; then character string (0x42) "AB" as-is
        let f = sent(translate_hex(
            0x0110,
            "022AAE01010000000000000200102112340011420241 42"
                .replace(' ', "")
                .as_str(),
        ));
        assert_eq!(
            f.asdu,
            [
                0x10, 0x11, 0x02, 0x10, 0x00, 0x21, 0x34, 0x12, 0x11, 0x00, 0x42, 0x02, 0x41, 0x42
            ]
        );
    }

    #[test]
    fn zdo_requests() {
        let f = sent(translate_hex(0x0045, "2AAE"));
        assert_eq!((f.profile, f.cluster), (0x0000, 0x0005));
        assert_eq!(f.asdu, [0x11, 0xAE, 0x2A]);
        assert_eq!(
            f.target,
            Target::Unicast {
                nwk: 0x2AAE,
                aps_ack: true
            }
        );

        let f = sent(translate_hex(0x0043, "2AAE01"));
        assert_eq!(
            (f.cluster, f.asdu.as_slice()),
            (0x0004, &[0x11, 0xAE, 0x2A, 0x01][..])
        );

        // NWK_addr_req broadcast to FFFD
        let f = sent(translate_hex(0x0040, "FFFDA4C1388F3FCA72810000"));
        assert_eq!(f.target, Target::Broadcast(0xFFFD));
        assert_eq!(
            f.asdu,
            [
                0x11, 0x81, 0x72, 0xCA, 0x3F, 0x8F, 0x38, 0xC1, 0xA4, 0x00, 0x00
            ]
        );

        assert_eq!(
            translate_hex(0x004E, "000000"),
            Translation::LocalNeighborTable { start_index: 0 }
        );
    }

    #[test]
    fn bind_goes_to_the_source_device() {
        // Bind 0006 of A4C1…7281 ep 01 to the coordinator ep 01
        let f = sent(translate_hex(
            0x0030,
            "A4C1388F3FCA728101000603 00124B0000ED0001 01"
                .replace(' ', "")
                .as_str(),
        ));
        assert_eq!(
            f.target,
            Target::Unicast {
                nwk: 0x2AAE,
                aps_ack: true
            }
        );
        assert_eq!(f.cluster, 0x0021);
        let mut expected = vec![
            0x11, 0x81, 0x72, 0xCA, 0x3F, 0x8F, 0x38, 0xC1, 0xA4, 0x01, 0x06, 0x00, 0x03,
        ];
        expected.extend([0x01, 0x00, 0xED, 0x00, 0x00, 0x4B, 0x12, 0x00, 0x01]);
        assert_eq!(f.asdu, expected);

        assert_eq!(
            translate_hex(
                0x0030,
                "0000000000000099010006030000000000000001 01"
                    .replace(' ', "")
                    .as_str()
            ),
            Translation::UnknownDevice(0x99)
        );
    }

    #[test]
    fn cluster_commands() {
        // On/Off toggle to a group
        let f = sent(translate_hex(0x0092, "010003010102"));
        assert_eq!(f.target, Target::Group(0x0003));
        assert_eq!(
            (f.cluster, f.asdu.as_slice()),
            (0x0006, &[0x01, 0x11, 0x02][..])
        );

        // Move to level with on/off, level 0x80, transition 0x000A
        let f = sent(translate_hex(
            0x0081,
            "022AAE010101 80000A".replace(' ', "").as_str(),
        ));
        assert_eq!(
            (f.cluster, f.asdu.as_slice()),
            (0x0008, &[0x01, 0x11, 0x04, 0x80, 0x0A, 0x00][..])
        );

        // Color temperature 370 mireds, transition 0
        let f = sent(translate_hex(0x00C0, "022AAE010101720000"));
        assert_eq!(f.asdu, [0x01, 0x11, 0x0A, 0x72, 0x01, 0x00, 0x00]);

        // Window covering go to lift percentage 50
        let f = sent(translate_hex(0x00FA, "022AAE01010532"));
        assert_eq!(
            (f.cluster, f.asdu.as_slice()),
            (0x0102, &[0x01, 0x11, 0x05, 0x32][..])
        );
    }

    #[test]
    fn unknown_commands_are_not_native() {
        assert!(
            translate(
                &Message {
                    msg_type: 0x0010,
                    payload: vec![]
                },
                &mut Ctx { seq: 0 }
            )
            .is_none()
        );
    }
}
