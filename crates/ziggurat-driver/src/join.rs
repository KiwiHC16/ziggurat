//! Joining an existing network as a childless router.
//!
//! The stack itself only starts on a network it already knows, so this client runs
//! first and owns the radio alone: active scan, MAC association, then the network key
//! delivered by the trust center (spec 4.6.3.2). The result is a [`NetworkConfig`] for
//! a regular [`NwkDeviceType::Router`] stack. Such a router relays and sends frames but
//! does not accept children: see the beacon built in `zigbee_stack/mac.rs`.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use abstract_bits::AbstractBits;
use ziggurat_ieee_802154::commands::{
    AssociationRequestDeviceType, AssociationRequestPowerSource,
    Ieee802154AssociationRequestCommand, Ieee802154BeaconRequestCommand,
    Ieee802154DataRequestCommand,
};
use ziggurat_ieee_802154::types::{Eui64, Key, Nwk, PanId};
use ziggurat_ieee_802154::{
    Ieee802154Address, Ieee802154AddressingMode, Ieee802154AssociationStatus,
    Ieee802154CommandFrame, Ieee802154CommandPayload, Ieee802154Frame, Ieee802154FrameControl,
    Ieee802154FrameHeader, Ieee802154FrameType, Ieee802154FrameVersion,
};
use ziggurat_phy::{
    ExclusiveRadio, RadioConfig, RadioError, RadioPhy, Receiver, RxFrame, TxFrame, TxResult,
};
use ziggurat_zigbee::aps::frame::{
    ApsCommandFrameCommand, ApsFrame, ApsTransportKeyDescriptor, parse_aps_frame,
};
use ziggurat_zigbee::aps::security::ApsSecurity;
use ziggurat_zigbee::beacon::ZigbeeBeacon;
use ziggurat_zigbee::nwk::frame::{EncryptedNwkFrame, NwkPayload};

use crate::runtime::Runtime;
use crate::zigbee_stack::{NetworkConfig, NwkDeviceType};

/// The only stack profile this stack speaks (Zigbee PRO)
const STACK_PROFILE_PRO: u8 = 2;
const BROADCAST_PAN: PanId = PanId(0xFFFF);
const UNASSIGNED_NWK: Nwk = Nwk(0xFFFF);
/// Mains-powered, always-on router, address allocated by the parent
pub const ROUTER_CAPABILITY: u8 = 0x8E;

/// What to join and how long to try.
#[derive(Debug, Clone)]
pub struct JoinParams {
    pub ieee: Eui64,
    pub channels: Vec<u8>,
    /// Only join this network. Without it, the strongest network open to joins wins.
    pub extended_pan_id: Option<Eui64>,
    /// The link key the trust center encrypts the network key with
    pub tc_link_key: Key,
    pub tx_power: i8,
    pub scan_dwell: Duration,
    /// macResponseWaitTime: delay before polling the association response
    pub response_wait: Duration,
    pub poll_wait: Duration,
    pub polls: u8,
    pub key_wait: Duration,
}

impl JoinParams {
    #[must_use]
    pub fn new(ieee: Eui64, tc_link_key: Key) -> Self {
        Self {
            ieee,
            channels: (11..=26).collect(),
            extended_pan_id: None,
            tc_link_key,
            tx_power: 8,
            scan_dwell: Duration::from_millis(250),
            response_wait: Duration::from_millis(500),
            poll_wait: Duration::from_millis(250),
            polls: 4,
            key_wait: Duration::from_secs(5),
        }
    }
}

/// A network this device is now a member of.
#[derive(Debug, Clone)]
pub struct JoinedNetwork {
    pub channel: u8,
    pub update_id: u8,
    pub pan_id: PanId,
    pub extended_pan_id: Eui64,
    pub network_address: Nwk,
    pub parent: Nwk,
    pub trust_center: Eui64,
    pub network_key: Key,
    pub network_key_seq_number: u8,
}

impl JoinedNetwork {
    /// The configuration of the router stack running on this network.
    #[must_use]
    pub fn router_config(&self, params: &JoinParams) -> NetworkConfig {
        NetworkConfig {
            role: NwkDeviceType::Router,
            channel: self.channel,
            update_id: self.update_id,
            pan_id: self.pan_id,
            extended_pan_id: self.extended_pan_id,
            network_address: self.network_address,
            ieee_address: params.ieee,
            network_key: self.network_key.clone(),
            network_key_seq_number: self.network_key_seq_number,
            network_key_tx_counter: 0,
            tc_link_key: params.tc_link_key.clone(),
            tclk_seed: None,
            tx_power: params.tx_power,
            source_routing: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("radio failure: {0}")]
    Radio(#[from] RadioError),
    #[error("no network open to joins")]
    NoNetwork,
    #[error("every parent refused or ignored the association; last status: {0:?}")]
    Refused(Option<Ieee802154AssociationStatus>),
    #[error("associated as {0:?} but the network key never came")]
    NoNetworkKey(Nwk),
}

/// A beacon worth trying to join through.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    channel: u8,
    pan_id: PanId,
    extended_pan_id: Eui64,
    parent: Nwk,
    update_id: u8,
    rssi: i8,
}

/// Scan, associate and fetch the network key. Takes over the radio: call it before
/// creating the stack that will run on the result.
pub async fn join_network<P: RadioPhy, R: Runtime>(
    phy: &P,
    params: &JoinParams,
) -> Result<JoinedNetwork, JoinError> {
    let mut rx = phy.subscribe_rx();

    phy.reset().await?;
    phy.reconfigure(&radio_config(
        params,
        params.channels[0],
        BROADCAST_PAN,
        UNASSIGNED_NWK,
    ))
    .await?;

    let candidates = scan::<P, R>(phy, params, &mut rx).await?;
    if candidates.is_empty() {
        return Err(JoinError::NoNetwork);
    }

    let mut last_status = None;
    for candidate in candidates {
        tracing::info!("Trying to join through {candidate:?}");
        phy.reconfigure(&radio_config(
            params,
            candidate.channel,
            candidate.pan_id,
            UNASSIGNED_NWK,
        ))
        .await?;

        match associate::<P, R>(phy, params, &candidate, &mut rx).await? {
            Ok(address) => {
                // Must land before the trust center's key transport: the radio only
                // acknowledges that frame once it knows its short address
                phy.reconfigure(&radio_config(
                    params,
                    candidate.channel,
                    candidate.pan_id,
                    address,
                ))
                .await?;

                let (network_key, sequence_number, trust_center) =
                    wait_network_key::<P, R>(params, address, &mut rx)
                        .await
                        .ok_or(JoinError::NoNetworkKey(address))?;

                tracing::info!("Joined {:?} as {address:?}", candidate.pan_id);
                return Ok(JoinedNetwork {
                    channel: candidate.channel,
                    update_id: candidate.update_id,
                    pan_id: candidate.pan_id,
                    extended_pan_id: candidate.extended_pan_id,
                    network_address: address,
                    parent: candidate.parent,
                    trust_center,
                    network_key,
                    network_key_seq_number: sequence_number,
                });
            }
            Err(status) => last_status = status.or(last_status),
        }
    }

    Err(JoinError::Refused(last_status))
}

fn radio_config(params: &JoinParams, channel: u8, pan_id: PanId, short: Nwk) -> RadioConfig {
    RadioConfig {
        channel,
        tx_power: params.tx_power,
        short_address: short,
        extended_address: params.ieee,
        pan_id,
        promiscuous: false,
        rx_on_when_idle: true,
        frame_pending_short: Vec::new(),
        frame_pending_extended: Vec::new(),
    }
}

async fn scan<P: RadioPhy, R: Runtime>(
    phy: &P,
    params: &JoinParams,
    rx: &mut P::RxStream,
) -> Result<Vec<Candidate>, RadioError> {
    // The strongest beacon per parent: a coordinator permitting joins may repeat itself
    let mut found: BTreeMap<(u16, u16), Candidate> = BTreeMap::new();

    let radio = phy.lock().await;
    radio.set_promiscuous(true).await?;

    for (sequence, &channel) in params.channels.iter().enumerate() {
        radio.set_channel(channel).await?;
        radio
            .transmit(TxFrame {
                psdu: beacon_request(sequence as u8),
                channel: None,
                csma_ca: true,
                max_frame_retries: 0,
                max_csma_backoffs: 4,
                security_processed: true,
            })
            .await?;

        let collect = async {
            while let Some(packet) = rx.recv().await {
                if let Some(candidate) = parse_candidate(params, channel, &packet) {
                    let key = (candidate.pan_id.0, candidate.parent.as_u16());
                    let best = found.entry(key).or_insert(candidate);
                    if candidate.rssi > best.rssi {
                        *best = candidate;
                    }
                }
            }
        };
        let _ = R::timeout(params.scan_dwell, collect).await;
    }

    radio.set_promiscuous(false).await?;
    drop(radio);

    let mut candidates: Vec<Candidate> = found.into_values().collect();
    candidates.sort_by_key(|candidate| core::cmp::Reverse(candidate.rssi));
    Ok(candidates)
}

fn parse_candidate(params: &JoinParams, channel: u8, packet: &RxFrame) -> Option<Candidate> {
    let Ok(Ieee802154Frame::Beacon(beacon)) = Ieee802154Frame::from_bytes_without_fcs(&packet.psdu)
    else {
        return None;
    };
    let payload = ZigbeeBeacon::from_abstract_bytes(&beacon.beacon_payload).ok()?;

    let open = beacon.superframe_specification.association_permit && payload.router_capacity;
    if !open || payload.stack_profile != STACK_PROFILE_PRO {
        return None;
    }
    if params
        .extended_pan_id
        .is_some_and(|wanted| wanted != payload.extended_pan_id)
    {
        return None;
    }

    let Some(Ieee802154Address::Nwk(parent)) = beacon.header.src_address else {
        return None;
    };

    Some(Candidate {
        channel,
        pan_id: beacon.header.src_pan_id?,
        extended_pan_id: payload.extended_pan_id,
        parent,
        update_id: payload.update_id,
        rssi: packet.rssi,
    })
}

fn mac_control(
    ack_request: bool,
    pan_id_compression: bool,
    dest: Ieee802154AddressingMode,
    src: Ieee802154AddressingMode,
) -> Ieee802154FrameControl {
    Ieee802154FrameControl {
        frame_type: Ieee802154FrameType::Command,
        security_enabled: false,
        frame_pending: false,
        ack_request,
        pan_id_compression,
        reserved1: false,
        sequence_number_suppression: false,
        information_elements_present: false,
        dest_addr_mode: dest,
        frame_version: Ieee802154FrameVersion::Ieee2003,
        src_addr_mode: src,
    }
}

fn beacon_request(sequence_number: u8) -> Vec<u8> {
    let frame: Ieee802154Frame = Ieee802154Frame::Command(Ieee802154CommandFrame {
        header: Ieee802154FrameHeader {
            frame_control: mac_control(
                false,
                false,
                Ieee802154AddressingMode::Short,
                Ieee802154AddressingMode::None,
            ),
            sequence_number: Some(sequence_number),
            dest_pan_id: Some(BROADCAST_PAN),
            dest_address: Some(Ieee802154Address::Nwk(Nwk(0xFFFF))),
            src_pan_id: None,
            src_address: None,
        },
        command_payload: Ieee802154CommandPayload::BeaconRequest(Ieee802154BeaconRequestCommand),
        fcs: 0x0000,
    });

    frame.to_bytes_without_fcs()
}

/// A MAC command from our EUI64 to the parent, acknowledged by the parent
fn parent_command(
    params: &JoinParams,
    candidate: &Candidate,
    sequence_number: u8,
    pan_id_compression: bool,
    command_payload: Ieee802154CommandPayload,
) -> TxFrame {
    let frame: Ieee802154Frame = Ieee802154Frame::Command(Ieee802154CommandFrame {
        header: Ieee802154FrameHeader {
            frame_control: mac_control(
                true,
                pan_id_compression,
                Ieee802154AddressingMode::Short,
                Ieee802154AddressingMode::Long,
            ),
            sequence_number: Some(sequence_number),
            dest_pan_id: Some(candidate.pan_id),
            dest_address: Some(Ieee802154Address::Nwk(candidate.parent)),
            src_pan_id: (!pan_id_compression).then_some(BROADCAST_PAN),
            src_address: Some(Ieee802154Address::Eui64(params.ieee)),
        },
        command_payload,
        fcs: 0x0000,
    });

    TxFrame {
        psdu: frame.to_bytes_without_fcs(),
        channel: None,
        csma_ca: true,
        max_frame_retries: 3,
        max_csma_backoffs: 4,
        security_processed: true,
    }
}

/// The MAC association: `Ok(Ok(address))` once accepted, `Ok(Err(status))` when this
/// parent did not take us.
async fn associate<P: RadioPhy, R: Runtime>(
    phy: &P,
    params: &JoinParams,
    candidate: &Candidate,
    rx: &mut P::RxStream,
) -> Result<Result<Nwk, Option<Ieee802154AssociationStatus>>, RadioError> {
    let request =
        Ieee802154CommandPayload::AssociationRequest(Ieee802154AssociationRequestCommand {
            alternate_pan_coordinator: false,
            device_type: AssociationRequestDeviceType::FullFunctionDevice,
            power_source: AssociationRequestPowerSource::MainsPower,
            receive_on_when_idle: true,
            reserved1: 0,
            security_capable: false,
            allocate_address: true,
        });

    let sent = phy
        .transmit(parent_command(params, candidate, 0x40, false, request))
        .await?;
    if sent != TxResult::Acked {
        tracing::info!("Association request not acknowledged: {sent:?}");
        return Ok(Err(None));
    }

    R::sleep(params.response_wait).await;

    for poll in 0..params.polls {
        let data_request = Ieee802154CommandPayload::DataRequest(Ieee802154DataRequestCommand);
        phy.transmit(parent_command(
            params,
            candidate,
            0x41_u8.wrapping_add(poll),
            true,
            data_request,
        ))
        .await?;

        let response = async {
            while let Some(packet) = rx.recv().await {
                if let Some(answer) = parse_association_response(params, &packet) {
                    return Some(answer);
                }
            }
            None
        };

        if let Ok(Some((status, address))) = R::timeout(params.poll_wait, response).await {
            return Ok(match status {
                Ieee802154AssociationStatus::AssociationSuccessful => Ok(address),
                refused => Err(Some(refused)),
            });
        }
    }

    Ok(Err(None))
}

fn parse_association_response(
    params: &JoinParams,
    packet: &RxFrame,
) -> Option<(Ieee802154AssociationStatus, Nwk)> {
    let Ok(Ieee802154Frame::Command(frame)) = Ieee802154Frame::from_bytes_without_fcs(&packet.psdu)
    else {
        return None;
    };
    if frame.header.dest_address != Some(Ieee802154Address::Eui64(params.ieee)) {
        return None;
    }
    match frame.command_payload {
        Ieee802154CommandPayload::AssociationResponse(response) => {
            Some((response.association_status, response.short_address))
        }
        _ => None,
    }
}

/// Wait for the trust center's Transport-Key, decrypted with the key-transport key of
/// our link key (spec 4.6.3.2).
async fn wait_network_key<P: RadioPhy, R: Runtime>(
    params: &JoinParams,
    address: Nwk,
    rx: &mut P::RxStream,
) -> Option<(Key, u8, Eui64)> {
    let mut security = ApsSecurity::new(params.tc_link_key.clone(), params.ieee, None);

    let wait = async {
        while let Some(packet) = rx.recv().await {
            if let Some(found) = parse_network_key(params, address, &mut security, &packet) {
                return Some(found);
            }
        }
        None
    };

    R::timeout(params.key_wait, wait).await.ok().flatten()
}

fn parse_network_key(
    params: &JoinParams,
    address: Nwk,
    security: &mut ApsSecurity,
    packet: &RxFrame,
) -> Option<(Key, u8, Eui64)> {
    let Ok(Ieee802154Frame::Data(data)) = Ieee802154Frame::from_bytes_without_fcs(&packet.psdu)
    else {
        return None;
    };
    let addressed_to_us = data.header.dest_address == Some(Ieee802154Address::Nwk(address))
        || data.header.dest_address == Some(Ieee802154Address::Eui64(params.ieee));
    if !addressed_to_us {
        return None;
    }

    let frame = EncryptedNwkFrame::from_bytes(&data.payload).ok()?;
    if frame.nwk_header.frame_control.security {
        return None;
    }
    let payload =
        NwkPayload::from_bytes(frame.nwk_header.frame_control.frame_type, frame.ciphertext);
    let ApsFrame::EncryptedCommand(encrypted) = parse_aps_frame(payload.as_opaque()?).ok()? else {
        return None;
    };

    let source = encrypted.aux_header.extended_source?;
    let unused_network_key = Key([0; 16]);
    let (command, _) = security.decrypt_command(source, &encrypted, &unused_network_key)?;

    let ApsCommandFrameCommand::TransportKey(transport) = command.command else {
        return None;
    };
    let ApsTransportKeyDescriptor::NetworkKey(descriptor) = transport.key_descriptor else {
        return None;
    };

    // Spec 4.4.10.1: all zeroes addresses every device
    if descriptor.destination_address != params.ieee
        && descriptor.destination_address != Eui64([0; 8])
    {
        return None;
    }

    Some((
        descriptor.key,
        descriptor.sequence_number,
        descriptor.source_address,
    ))
}

/// Device_annce of a freshly joined router (spec 2.4.3.1.11), broadcast by the caller.
#[must_use]
pub fn device_annce_payload(sequence: u8, network_address: Nwk, ieee: Eui64) -> Vec<u8> {
    let mut data = vec![sequence];
    data.extend(network_address.to_bytes());
    data.extend(ieee.to_bytes());
    data.push(ROUTER_CAPABILITY);
    data
}
