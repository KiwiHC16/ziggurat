//! ZiGate message types used by Abeille, and builders for the gateway → host replies.
//!
//! Layouts follow `AbeilleParser.class.php` (decoders) and `AbeilleCmdProcess.class.php`
//! (commands). Native fields are big-endian; raw ZDO/ZCL inside 0x0530/0x8002 are
//! little-endian, as on the air.

use crate::frame::Message;

// Gateway-only commands sent by `configureZigate()` and the cron
pub const SET_CHANNEL_MASK: u16 = 0x0021;
pub const SET_TIME: u16 = 0x0016;
pub const GET_TIME: u16 = 0x0017;
pub const SET_MODE: u16 = 0x0002;
pub const SET_CERTIFICATION: u16 = 0x0019;
pub const SET_DEVICE_TYPE: u16 = 0x0023;
pub const START_NETWORK: u16 = 0x0024;
pub const GET_VERSION: u16 = 0x0010;
pub const GET_NETWORK_STATE: u16 = 0x0009;
pub const RESET: u16 = 0x0011;
pub const ERASE_PDM: u16 = 0x0012;
pub const GET_PERMIT_JOIN: u16 = 0x0014;
pub const GET_DEVICE_LIST: u16 = 0x0015;
pub const SET_LED: u16 = 0x0018;
pub const PERMIT_JOIN: u16 = 0x0049;
pub const SET_TX_POWER: u16 = 0x0806;
pub const GET_TX_POWER: u16 = 0x0807;

// Over-the-air commands
pub const RAW_APS_DATA: u16 = 0x0530;
pub const READ_ATTRIBUTE: u16 = 0x0100;
pub const WRITE_ATTRIBUTE: u16 = 0x0110;
pub const ACTIVE_ENDPOINTS: u16 = 0x0045;
pub const SIMPLE_DESCRIPTOR: u16 = 0x0043;
pub const NODE_DESCRIPTOR: u16 = 0x0042;
pub const IEEE_ADDRESS: u16 = 0x0041;
pub const NWK_ADDRESS: u16 = 0x0040;
pub const MGMT_LQI: u16 = 0x004E;
pub const BIND: u16 = 0x0030;
pub const UNBIND: u16 = 0x0031;
pub const MGMT_LEAVE: u16 = 0x0047;
pub const LEAVE_REQUEST: u16 = 0x004C;
pub const ADD_GROUP: u16 = 0x0060;
pub const GET_GROUP_MEMBERSHIP: u16 = 0x0062;
pub const REMOVE_GROUP: u16 = 0x0063;
pub const REMOVE_ALL_GROUPS: u16 = 0x0064;
pub const IDENTIFY: u16 = 0x0070;
pub const MOVE_TO_LEVEL_ONOFF: u16 = 0x0081;
pub const LEVEL_STEP: u16 = 0x0082;
pub const LEVEL_STOP: u16 = 0x0084;
pub const ON_OFF: u16 = 0x0092;
pub const ON_OFF_TIMED: u16 = 0x0093;
pub const MOVE_TO_COLOUR: u16 = 0x00B7;
pub const MOVE_TO_COLOUR_TEMPERATURE: u16 = 0x00C0;
pub const WINDOW_COVERING: u16 = 0x00FA;
pub const READ_REPORTING_CONFIG: u16 = 0x0122;
pub const DISCOVER_ATTRIBUTES: u16 = 0x0140;

// Gateway → host
pub const STATUS: u16 = 0x8000;
pub const DATA_INDICATION: u16 = 0x8002;
pub const FACTORY_NEW_RESTART: u16 = 0x8007;
pub const NETWORK_STATE: u16 = 0x8009;
pub const VERSION: u16 = 0x8010;
pub const APS_ACK: u16 = 0x8011;
pub const APS_DATA_CONFIRM: u16 = 0x8012;
pub const PERMIT_JOIN_STATUS: u16 = 0x8014;
pub const TIME: u16 = 0x8017;
pub const NETWORK_FORMED: u16 = 0x8024;
pub const LEAVE_INDICATION: u16 = 0x8048;
pub const APS_DATA_FAILURE: u16 = 0x8702;
pub const TX_POWER_SET: u16 = 0x8806;
pub const TX_POWER: u16 = 0x8807;
pub const DEVICE_ANNOUNCE: u16 = 0x004D;

/// Commands Abeille treats as gateway-only (`zgIsZigateOnly`): nothing goes on the air.
#[must_use]
pub const fn is_gateway_only(msg_type: u16) -> bool {
    matches!(
        msg_type,
        SET_MODE
            | GET_NETWORK_STATE
            | GET_VERSION
            | RESET
            | ERASE_PDM
            | GET_PERMIT_JOIN
            | GET_DEVICE_LIST
            | SET_TIME
            | GET_TIME
            | SET_LED
            | SET_CERTIFICATION
            | SET_CHANNEL_MASK
            | SET_DEVICE_TYPE
            | START_NETWORK
            | PERMIT_JOIN
            | SET_TX_POWER
            | GET_TX_POWER
    )
}

/// The firmware version we advertise in 0x8010: official v1 OPDM 3.23. Abeille warns
/// below 0x0321 and ignores device announces until it has seen one 0x8010.
pub const FW_MAJOR: u16 = 0x0004;
pub const FW_MINOR: u16 = 0x0323;

/// 0x8000 status reply, in the 8-byte form so Abeille also reads the APS counter and
/// the NPDU/APDU counters (`AbeilleParser::decode8000`).
///
/// Abeille matches it strictly FIFO against its oldest in-flight command, by
/// `packet_type`: send exactly one per received command, in order.
#[must_use]
pub fn status(status: u8, sqn: u8, packet_type: u16, sent: bool, sqn_aps: u8) -> Message {
    let [p0, p1] = packet_type.to_be_bytes();
    Message {
        msg_type: STATUS,
        // NPDU and APDU stay 0: Ziggurat has no fixed buffer pool, and a stuck non-zero
        // NPDU makes Abeille throttle (> 7) or soft-reset the gateway after 4 min
        payload: vec![status, sqn, p0, p1, u8::from(sent), sqn_aps, 0x00, 0x00],
    }
}

#[must_use]
pub fn version() -> Message {
    let mut payload = FW_MAJOR.to_be_bytes().to_vec();
    payload.extend(FW_MINOR.to_be_bytes());
    Message {
        msg_type: VERSION,
        payload,
    }
}

/// 0x8024: `status addr(2) ieee(8) channel`. Status 0x01 = formed, 0x04 = already formed.
#[must_use]
pub fn network_formed(status: u8, nwk: u16, ieee: u64, channel: u8) -> Message {
    let mut payload = vec![status];
    payload.extend(nwk.to_be_bytes());
    payload.extend(ieee.to_be_bytes());
    payload.push(channel);
    Message {
        msg_type: NETWORK_FORMED,
        payload,
    }
}

/// 0x8009 network state: `addr(2) ieee(8) pan(2) ext_pan(8) channel`.
#[must_use]
pub fn network_state(nwk: u16, ieee: u64, pan: u16, ext_pan: u64, channel: u8) -> Message {
    let mut payload = nwk.to_be_bytes().to_vec();
    payload.extend(ieee.to_be_bytes());
    payload.extend(pan.to_be_bytes());
    payload.extend(ext_pan.to_be_bytes());
    payload.push(channel);
    Message {
        msg_type: NETWORK_STATE,
        payload,
    }
}

/// 0x8017: seconds since 2000-01-01.
#[must_use]
pub fn time(seconds_since_2000: u32) -> Message {
    Message {
        msg_type: TIME,
        payload: seconds_since_2000.to_be_bytes().to_vec(),
    }
}

/// 0x8806/0x8807: TX power.
#[must_use]
pub fn tx_power(msg_type: u16, dbm: i8) -> Message {
    Message {
        msg_type,
        payload: vec![dbm.cast_unsigned()],
    }
}

/// 0x8702 APS data failure: `status src_ep dst_ep dst_mode(02) addr(2) sqn_aps npdu apdu`.
/// Abeille only logs it.
#[must_use]
pub fn aps_failure(status: u8, src_ep: u8, dst_ep: u8, dst: u16, sqn_aps: u8) -> Message {
    let mut payload = vec![status, src_ep, dst_ep, 0x02];
    payload.extend(dst.to_be_bytes());
    payload.extend([sqn_aps, 0x00, 0x00]);
    Message {
        msg_type: APS_DATA_FAILURE,
        payload,
    }
}

/// 0x8014: permit-join state, 0x01 = open.
#[must_use]
pub fn permit_join_status(open: bool) -> Message {
    Message {
        msg_type: PERMIT_JOIN_STATUS,
        payload: vec![u8::from(open)],
    }
}

/// 0x8011: `status dst_addr(2) dst_ep cluster(2) sqn_aps`. Abeille looks the command up
/// by `sqn_aps` only, so it must echo the value given in the matching 0x8000.
#[must_use]
pub fn aps_ack(status: u8, dst: u16, dst_ep: u8, cluster: u16, sqn_aps: u8) -> Message {
    let mut payload = vec![status];
    payload.extend(dst.to_be_bytes());
    payload.push(dst_ep);
    payload.extend(cluster.to_be_bytes());
    payload.push(sqn_aps);
    Message {
        msg_type: APS_ACK,
        payload,
    }
}

/// 0x004D device announce: `addr(2) ieee(8) mac_capabilities [rejoin]`.
#[must_use]
pub fn device_announce(nwk: u16, ieee: u64, mac_capabilities: u8) -> Message {
    let mut payload = nwk.to_be_bytes().to_vec();
    payload.extend(ieee.to_be_bytes());
    payload.push(mac_capabilities);
    Message {
        msg_type: DEVICE_ANNOUNCE,
        payload,
    }
}

/// 0x8048 leave indication: `ieee(8) rejoin`.
#[must_use]
pub fn leave_indication(ieee: u64, rejoin: bool) -> Message {
    let mut payload = ieee.to_be_bytes().to_vec();
    payload.push(u8::from(rejoin));
    Message {
        msg_type: LEAVE_INDICATION,
        payload,
    }
}

/// Source of a 0x8002 data indication. Abeille always parses the addresses as 2 bytes.
pub struct Indication<'a> {
    pub profile: u16,
    pub cluster: u16,
    pub src_ep: u8,
    pub dst_ep: u8,
    pub src_nwk: u16,
    pub dst_nwk: u16,
    /// Set for group-addressed frames: reported with destination mode 0x01
    pub dst_group: Option<u16>,
    /// The raw APS payload (ZDO seq + body, or a full ZCL frame), little-endian
    pub asdu: &'a [u8],
}

/// 0x8002 data indication, the RAW-mode path for every ZDO and ZCL frame:
/// `status profile(2) cluster(2) src_ep dst_ep src_mode src(2) dst_mode dst(2) asdu`.
#[must_use]
pub fn data_indication(ind: &Indication<'_>) -> Message {
    const GROUP: u8 = 0x01;
    const SHORT: u8 = 0x02;
    let mut payload = vec![0x00];
    payload.extend(ind.profile.to_be_bytes());
    payload.extend(ind.cluster.to_be_bytes());
    payload.push(ind.src_ep);
    payload.push(ind.dst_ep);
    payload.push(SHORT);
    payload.extend(ind.src_nwk.to_be_bytes());
    match ind.dst_group {
        Some(group) => {
            payload.push(GROUP);
            payload.extend(group.to_be_bytes());
        }
        None => {
            payload.push(SHORT);
            payload.extend(ind.dst_nwk.to_be_bytes());
        }
    }
    payload.extend_from_slice(ind.asdu);
    Message {
        msg_type: DATA_INDICATION,
        payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_the_eight_byte_form() {
        let m = status(0x00, 0x12, RAW_APS_DATA, true, 0x34);
        assert_eq!(m.msg_type, STATUS);
        assert_eq!(m.payload, [0x00, 0x12, 0x05, 0x30, 0x01, 0x34, 0x00, 0x00]);
    }

    #[test]
    fn version_is_official_3_23() {
        assert_eq!(version().payload, [0x00, 0x04, 0x03, 0x23]);
    }

    #[test]
    fn data_indication_layout() {
        let m = data_indication(&Indication {
            profile: 0x0104,
            cluster: 0x0B04,
            src_ep: 0x01,
            dst_ep: 0x01,
            src_nwk: 0x2AAE,
            dst_nwk: 0x0000,
            dst_group: None,
            asdu: &[0x18, 0x30, 0x0A],
        });
        assert_eq!(
            m.payload,
            [
                0x00, 0x01, 0x04, 0x0B, 0x04, 0x01, 0x01, 0x02, 0x2A, 0xAE, 0x02, 0x00, 0x00, 0x18,
                0x30, 0x0A
            ]
        );
    }
}
