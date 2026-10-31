//! The coordinator's own ZDO server: what the ZiGate firmware answered by itself.
//!
//! Ziggurat only answers Mgmt_Lqi_req and Mgmt_Rtg_req; every other request addressed to
//! the coordinator (address discovery, descriptors, Match_Desc from IAS or OTA devices)
//! is answered here. Devices need these during and right after joining.

use ziggurat_driver::zigbee_stack::{NwkDeviceType, ZigbeeStack, neighbors};
use ziggurat_ieee_802154::types::Eui64;
use ziggurat_phy::RadioPhy;
use ziggurat_zigbee::zdp::{
    MgmtLqiRsp, NeighborDescriptor, ZdpAffinity, ZdpCommand, ZdpDeviceType, ZdpPermitJoining,
    ZdpRxOnWhenIdle, ZdpStatus,
};

pub const ENDPOINT: u8 = 0x01;
pub const PROFILE_HA: u16 = 0x0104;
/// NXP, as reported by the ZiGate
pub const MANUFACTURER_CODE: u16 = 0x1037;
/// Configuration tool
const DEVICE_ID: u16 = 0x0005;

/// Server clusters of the coordinator endpoint: Basic, Identify, Time (answered by
/// Abeille), OTA (served by Abeille), IAS ACE
const IN_CLUSTERS: &[u16] = &[0x0000, 0x0003, 0x000A, 0x0019, 0x0501];
/// Client clusters: what Abeille controls and receives reports from
const OUT_CLUSTERS: &[u16] = &[
    0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0020, 0x0101, 0x0102, 0x0201, 0x0300, 0x0400,
    0x0402, 0x0403, 0x0405, 0x0406, 0x0500, 0x0502, 0x0702, 0x0B04,
];

const NWK_ADDR_REQ: u16 = 0x0000;
const IEEE_ADDR_REQ: u16 = 0x0001;
const NODE_DESC_REQ: u16 = 0x0002;
const POWER_DESC_REQ: u16 = 0x0003;
const SIMPLE_DESC_REQ: u16 = 0x0004;
const ACTIVE_EP_REQ: u16 = 0x0005;
const MATCH_DESC_REQ: u16 = 0x0006;

const STATUS_SUCCESS: u8 = 0x00;
const STATUS_INVALID_EP: u8 = 0x82;

const COORDINATOR: u16 = 0x0000;
const BROADCAST_MIN: u16 = 0xFFF8;

/// A ZDO response to send back to the requester
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub to: u16,
    pub cluster: u16,
    pub asdu: Vec<u8>,
}

/// Node descriptor: coordinator, 2.4 GHz, mains powered FFD that allocates addresses,
/// primary trust center and network manager, stack compliance revision 22
fn node_descriptor() -> Vec<u8> {
    const SERVER_MASK: u16 = 0x0001 | 0x0040 | (22 << 9);
    let mut desc = vec![0x00, 0x40, 0x8F];
    desc.extend(MANUFACTURER_CODE.to_le_bytes());
    desc.push(0x52);
    desc.extend(0x0080u16.to_le_bytes());
    desc.extend(SERVER_MASK.to_le_bytes());
    desc.extend(0x0080u16.to_le_bytes());
    desc.push(0x00);
    desc
}

fn simple_descriptor() -> Vec<u8> {
    let mut desc = vec![ENDPOINT];
    desc.extend(PROFILE_HA.to_le_bytes());
    desc.extend(DEVICE_ID.to_le_bytes());
    desc.push(0x00);
    for list in [IN_CLUSTERS, OUT_CLUSTERS] {
        desc.push(u8::try_from(list.len()).expect("cluster lists are short"));
        desc.extend(list.iter().flat_map(|c| c.to_le_bytes()));
    }
    desc
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]))
}

fn le16_list(bytes: &[u8], count: usize) -> Option<Vec<u16>> {
    (0..count).map(|i| le16(bytes, 2 * i)).collect()
}

/// Answer a ZDO request (`profile 0`, `endpoint 0`) if it is the coordinator's to answer.
/// `destination` is the NWK destination of the request; broadcasts are only answered
/// when they concern the coordinator.
#[must_use]
pub fn answer(
    source: u16,
    destination: u16,
    cluster: u16,
    data: &[u8],
    coordinator: Eui64,
) -> Option<Response> {
    let to_us = destination == COORDINATOR;
    if !to_us && destination < BROADCAST_MIN {
        return None;
    }
    let (&seq, body) = data.split_first()?;
    let ieee = coordinator.to_bytes();
    let mut asdu = vec![seq, STATUS_SUCCESS];

    match cluster {
        NWK_ADDR_REQ | IEEE_ADDR_REQ => {
            // NWK_addr_req: ieee(8) type index; IEEE_addr_req: nwk(2) type index
            let (about_us, rest) = if cluster == NWK_ADDR_REQ {
                (body.get(..8)? == ieee, body.get(8..)?)
            } else {
                (le16(body, 0)? == COORDINATOR, body.get(2..)?)
            };
            if !about_us {
                return None;
            }
            asdu.extend(ieee);
            asdu.extend(COORDINATOR.to_le_bytes());
            // Extended request: no associated devices listed
            if let [0x01, start, ..] = *rest {
                asdu.extend([0x00, start]);
            }
        }
        NODE_DESC_REQ | POWER_DESC_REQ | ACTIVE_EP_REQ if to_us => {
            if le16(body, 0)? != COORDINATOR {
                return None;
            }
            asdu.extend(COORDINATOR.to_le_bytes());
            match cluster {
                NODE_DESC_REQ => asdu.extend(node_descriptor()),
                // Receiver always on, mains available and in use, 100 %
                POWER_DESC_REQ => asdu.extend([0x10, 0xC1]),
                _ => asdu.extend([0x01, ENDPOINT]),
            }
        }
        SIMPLE_DESC_REQ if to_us => {
            if le16(body, 0)? != COORDINATOR {
                return None;
            }
            if *body.get(2)? == ENDPOINT {
                let desc = simple_descriptor();
                asdu.extend(COORDINATOR.to_le_bytes());
                asdu.push(u8::try_from(desc.len()).ok()?);
                asdu.extend(desc);
            } else {
                asdu = vec![seq, STATUS_INVALID_EP];
                asdu.extend(COORDINATOR.to_le_bytes());
                asdu.push(0);
            }
        }
        MATCH_DESC_REQ => {
            // nwk(2) profile(2) in_count in(2 each) out_count out(2 each)
            let nwk = le16(body, 0)?;
            if nwk != COORDINATOR && nwk < BROADCAST_MIN {
                return None;
            }
            let profile = le16(body, 2)?;
            let in_count = usize::from(*body.get(4)?);
            let their_in = le16_list(body.get(5..)?, in_count)?;
            let out_at = 5 + 2 * in_count;
            let out_count = usize::from(*body.get(out_at)?);
            let their_out = le16_list(body.get(out_at + 1..)?, out_count)?;

            // Spec 2.4.3.1.7: the request's input list is matched against our input
            // (server) clusters, its output list against our output (client) clusters
            let matches = (profile == PROFILE_HA || profile == 0xFFFF)
                && (their_in.iter().any(|c| IN_CLUSTERS.contains(c))
                    || their_out.iter().any(|c| OUT_CLUSTERS.contains(c)));
            if !matches && !to_us {
                return None;
            }
            asdu.extend(COORDINATOR.to_le_bytes());
            if matches {
                asdu.extend([0x01, ENDPOINT]);
            } else {
                asdu.push(0x00);
            }
        }
        _ => return None,
    }

    Some(Response {
        to: source,
        cluster: cluster | 0x8000,
        asdu,
    })
}

/// Mgmt_Lqi_rsp for the coordinator itself, as the stack builds it for remote queries
#[allow(clippy::significant_drop_tightening)]
pub fn local_neighbor_table<P: RadioPhy>(stack: &ZigbeeStack<P>, seq: u8, start: u8) -> Vec<u8> {
    const PER_FRAME: usize = 2;
    let extended_pan_id = stack.config.extended_pan_id;
    let core = stack.state.core.lock();
    let mut entries: Vec<&neighbors::TableEntry> = core.nib.neighbors.entries().collect();
    entries.sort_by_key(|entry| entry.network_address.as_u16());

    let list = entries
        .iter()
        .skip(usize::from(start))
        .take(PER_FRAME)
        .map(|entry| NeighborDescriptor {
            extended_pan_id,
            extended_address: entry.extended_address,
            network_address: entry.network_address,
            device_type: match entry.device_type {
                NwkDeviceType::Coordinator => ZdpDeviceType::Coordinator,
                NwkDeviceType::Router => ZdpDeviceType::Router,
                NwkDeviceType::EndDevice => ZdpDeviceType::EndDevice,
            },
            rx_on_when_idle: if entry.rx_on_when_idle {
                ZdpRxOnWhenIdle::On
            } else {
                ZdpRxOnWhenIdle::Off
            },
            affinity: match entry.relationship {
                neighbors::Relationship::Parent => ZdpAffinity::Parent,
                neighbors::Relationship::Child => ZdpAffinity::Child,
                neighbors::Relationship::Sibling => ZdpAffinity::Sibling,
                _ => ZdpAffinity::NoneOfTheAbove,
            },
            permit_joining: ZdpPermitJoining::Unknown,
            depth: 1,
            lqa: entry.lqa().unwrap_or(0),
        })
        .collect();

    MgmtLqiRsp {
        status: ZdpStatus::Success,
        neighbor_table_entries: u8::try_from(entries.len()).unwrap_or(u8::MAX),
        start_index: start,
        neighbor_table_list: list,
    }
    .serialize(seq)
    .unwrap_or_default()
}

/// Mgmt_Rtg_rsp for the coordinator itself, as the stack builds it for remote queries
pub fn local_routing_table<P: RadioPhy>(stack: &ZigbeeStack<P>, seq: u8, start: u8) -> Vec<u8> {
    stack
        .routing_table_slice(start)
        .serialize(seq)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: Eui64 = Eui64([0x01, 0x00, 0xED, 0x00, 0x00, 0x4B, 0x12, 0x00]);

    #[test]
    fn ieee_and_nwk_address_of_the_coordinator() {
        let r = answer(
            0x2AAE,
            0x0000,
            IEEE_ADDR_REQ,
            &[0x07, 0x00, 0x00, 0x00, 0x00],
            US,
        )
        .unwrap();
        assert_eq!((r.to, r.cluster), (0x2AAE, 0x8001));
        let mut expected = vec![0x07, 0x00];
        expected.extend(US.to_bytes());
        expected.extend([0x00, 0x00]);
        assert_eq!(r.asdu, expected);

        // NWK_addr_req broadcast about us is answered, about someone else is not
        let mut req = vec![0x08];
        req.extend(US.to_bytes());
        req.extend([0x00, 0x00]);
        assert!(answer(0x2AAE, 0xFFFD, NWK_ADDR_REQ, &req, US).is_some());
        req[1] ^= 0xFF;
        assert!(answer(0x2AAE, 0xFFFD, NWK_ADDR_REQ, &req, US).is_none());
    }

    #[test]
    fn descriptors() {
        let r = answer(0x2AAE, 0x0000, NODE_DESC_REQ, &[0x01, 0x00, 0x00], US).unwrap();
        assert_eq!(r.cluster, 0x8002);
        assert_eq!(r.asdu.len(), 2 + 2 + 13);
        assert_eq!(&r.asdu[4..9], &[0x00, 0x40, 0x8F, 0x37, 0x10]);

        let r = answer(0x2AAE, 0x0000, ACTIVE_EP_REQ, &[0x02, 0x00, 0x00], US).unwrap();
        assert_eq!(r.asdu, [0x02, 0x00, 0x00, 0x00, 0x01, 0x01]);

        let r = answer(
            0x2AAE,
            0x0000,
            SIMPLE_DESC_REQ,
            &[0x03, 0x00, 0x00, 0x01],
            US,
        )
        .unwrap();
        assert_eq!(
            &r.asdu[..5],
            &[
                0x03,
                0x00,
                0x00,
                0x00,
                u8::try_from(r.asdu.len() - 5).unwrap()
            ]
        );
        assert_eq!(&r.asdu[5..10], &[0x01, 0x04, 0x01, 0x05, 0x00]);

        let r = answer(
            0x2AAE,
            0x0000,
            SIMPLE_DESC_REQ,
            &[0x04, 0x00, 0x00, 0x09],
            US,
        )
        .unwrap();
        assert_eq!(r.asdu, [0x04, STATUS_INVALID_EP, 0x00, 0x00, 0x00]);

        // Not for us: another device's descriptor
        assert!(answer(0x2AAE, 0x0000, NODE_DESC_REQ, &[0x01, 0x34, 0x12], US).is_none());
        // Unicast to someone else
        assert!(answer(0x2AAE, 0x1234, NODE_DESC_REQ, &[0x01, 0x00, 0x00], US).is_none());
    }

    #[test]
    fn match_desc_matching() {
        // An IAS zone looking for its CIE, which has IAS Zone as an output (client)
        // cluster: seq, nwk FFFD, profile 0104, no input cluster, output 0500
        let req = [0x09, 0xFD, 0xFF, 0x04, 0x01, 0x00, 0x01, 0x00, 0x05];
        let r = answer(0x5678, 0xFFFD, MATCH_DESC_REQ, &req, US).unwrap();
        assert_eq!((r.to, r.cluster), (0x5678, 0x8006));
        assert_eq!(r.asdu, [0x09, 0x00, 0x00, 0x00, 0x01, 0x01]);

        // The Aqara plug looking for an OTA server (input cluster 0019), as captured
        let req = [0x2E, 0xFD, 0xFF, 0x04, 0x01, 0x01, 0x19, 0x00, 0x00];
        let r = answer(0x9481, 0xFFFD, MATCH_DESC_REQ, &req, US).unwrap();
        assert_eq!(r.asdu, [0x2E, 0x00, 0x00, 0x00, 0x01, 0x01]);
        // Server 0500 is not ours: not matched
        let req = [0x0B, 0xFD, 0xFF, 0x04, 0x01, 0x01, 0x00, 0x05, 0x00];
        assert!(answer(0x5678, 0xFFFD, MATCH_DESC_REQ, &req, US).is_none());

        // Nothing in common, broadcast: no answer
        let req = [0x0A, 0xFD, 0xFF, 0x04, 0x01, 0x01, 0x34, 0x12, 0x00];
        assert!(answer(0x5678, 0xFFFD, MATCH_DESC_REQ, &req, US).is_none());
    }
}
