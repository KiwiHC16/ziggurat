//! What a router must answer by itself: the ZDO requests addressed to it, and a Basic
//! cluster read, so that a coordinator can interview it. The stack only answers
//! Mgmt_Lqi_req and Mgmt_Rtg_req.

use ziggurat_ieee_802154::types::Eui64;

pub const ENDPOINT: u8 = 1;
pub const PROFILE_ZDO: u16 = 0x0000;
pub const PROFILE_HA: u16 = 0x0104;
pub const CLUSTER_BASIC: u16 = 0x0000;
/// Range Extender
const DEVICE_ID: u16 = 0x0008;
const IN_CLUSTERS: &[u16] = &[CLUSTER_BASIC];
const MANUFACTURER_CODE: u16 = 0x0000;
pub const MANUFACTURER: &str = "Ziggurat";
pub const MODEL: &str = "ziggurat-router";

const NWK_ADDR_REQ: u16 = 0x0000;
const IEEE_ADDR_REQ: u16 = 0x0001;
const NODE_DESC_REQ: u16 = 0x0002;
const POWER_DESC_REQ: u16 = 0x0003;
const SIMPLE_DESC_REQ: u16 = 0x0004;
const ACTIVE_EP_REQ: u16 = 0x0005;
const MATCH_DESC_REQ: u16 = 0x0006;

const STATUS_SUCCESS: u8 = 0x00;
const STATUS_INVALID_EP: u8 = 0x82;
const BROADCAST_MIN: u16 = 0xFFF8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub to: u16,
    pub cluster: u16,
    pub asdu: Vec<u8>,
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

/// Router, 2.4 GHz, mains-powered FFD, stack compliance revision 22, not a trust center
fn node_descriptor() -> Vec<u8> {
    const SERVER_MASK: u16 = 22 << 9;
    let mut desc = vec![0x01, 0x40, ziggurat_driver::join::ROUTER_CAPABILITY];
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
    desc.push(u8::try_from(IN_CLUSTERS.len()).expect("cluster list is short"));
    desc.extend(IN_CLUSTERS.iter().flat_map(|c| c.to_le_bytes()));
    desc.push(0x00);
    desc
}

/// The response to a ZDO request, or `None` when it is not ours to answer.
#[must_use]
pub fn answer(
    source: u16,
    destination: u16,
    cluster: u16,
    data: &[u8],
    ieee: Eui64,
    address: u16,
) -> Option<Response> {
    let to_us = destination == address;
    if !to_us && destination < BROADCAST_MIN {
        return None;
    }
    let (&seq, body) = data.split_first()?;
    let mut asdu = vec![seq, STATUS_SUCCESS];

    match cluster {
        NWK_ADDR_REQ | IEEE_ADDR_REQ => {
            let (about_us, rest) = if cluster == NWK_ADDR_REQ {
                (body.get(..8)? == ieee.0, body.get(8..)?)
            } else {
                (le16(body, 0)? == address, body.get(2..)?)
            };
            if !about_us {
                return None;
            }
            asdu.extend(ieee.0);
            asdu.extend(address.to_le_bytes());
            // Extended request: no associated devices, a router without children
            if let [0x01, start, ..] = *rest {
                asdu.extend([0x00, start]);
            }
        }
        NODE_DESC_REQ | POWER_DESC_REQ | ACTIVE_EP_REQ if to_us => {
            if le16(body, 0)? != address {
                return None;
            }
            asdu.extend(address.to_le_bytes());
            match cluster {
                NODE_DESC_REQ => asdu.extend(node_descriptor()),
                // Receiver always on, mains available and in use, 100 %
                POWER_DESC_REQ => asdu.extend([0x10, 0xC1]),
                _ => asdu.extend([0x01, ENDPOINT]),
            }
        }
        SIMPLE_DESC_REQ if to_us => {
            if le16(body, 0)? != address {
                return None;
            }
            if *body.get(2)? == ENDPOINT {
                let desc = simple_descriptor();
                asdu.extend(address.to_le_bytes());
                asdu.push(u8::try_from(desc.len()).ok()?);
                asdu.extend(desc);
            } else {
                asdu = vec![seq, STATUS_INVALID_EP];
                asdu.extend(address.to_le_bytes());
                asdu.push(0);
            }
        }
        MATCH_DESC_REQ if to_us => {
            // Nothing to match: a range extender offers no application cluster
            asdu.extend(address.to_le_bytes());
            asdu.push(0x00);
        }
        _ => return None,
    }

    Some(Response {
        to: source,
        cluster: cluster | 0x8000,
        asdu,
    })
}

/// Read Attributes on Basic: the attributes that identify the device. Other attributes
/// are reported unsupported.
#[must_use]
pub fn read_basic(data: &[u8]) -> Option<Vec<u8>> {
    const READ_ATTRIBUTES: u8 = 0x00;
    const READ_ATTRIBUTES_RESPONSE: u8 = 0x01;
    const UNSUPPORTED_ATTRIBUTE: u8 = 0x86;
    const SERVER_TO_CLIENT: u8 = 0x18;
    const UINT8: u8 = 0x20;
    const ENUM8: u8 = 0x30;
    const STRING: u8 = 0x42;

    let (&control, rest) = data.split_first()?;
    // Global, client to server, not manufacturer specific
    if control & 0x0F != 0 || control & 0x04 != 0 {
        return None;
    }
    let (&sequence, rest) = rest.split_first()?;
    let (&command, attributes) = rest.split_first()?;
    if command != READ_ATTRIBUTES {
        return None;
    }

    let mut out = vec![SERVER_TO_CLIENT, sequence, READ_ATTRIBUTES_RESPONSE];
    for id in attributes.as_chunks::<2>().0 {
        out.extend(id);
        let attribute = u16::from_le_bytes(*id);
        let mut string = |text: &str| {
            out.extend([
                STRING_SUCCESS,
                STRING,
                u8::try_from(text.len()).expect("short"),
            ]);
            out.extend(text.as_bytes());
        };
        const STRING_SUCCESS: u8 = 0x00;
        match attribute {
            0x0004 => string(MANUFACTURER),
            0x0005 => string(MODEL),
            // ZCL version
            0x0000 => out.extend([0x00, UINT8, 0x08]),
            // Mains, single phase
            0x0007 => out.extend([0x00, ENUM8, 0x01]),
            _ => out.push(UNSUPPORTED_ATTRIBUTE),
        }
    }
    Some(out)
}
