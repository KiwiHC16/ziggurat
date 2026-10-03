//! The bridge driven like Abeille would, on the simulated radio with a simulated router.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use ziggurat_driver::runtime::TokioSpawner;
use ziggurat_driver::zigbee_stack::{
    ApsAck, NwkDeviceType, RouteDirective, Tunables, TxPriority, ZigbeeNotification, ZigbeeStack,
};
use ziggurat_ieee_802154::types::{Eui64, Nwk};
use ziggurat_sim::medium::{Link, Medium, NodeId};
use ziggurat_sim::phy::SimPhy;
use ziggurat_zigate::bridge::{Bridge, Outbound};
use ziggurat_zigate::frame::Message;
use ziggurat_zigate::msg;
use ziggurat_zigate::server::{FlowControlMode, RunConfig, start_bridge};
use ziggurat_zigate::state::{NetworkState, eui_to_u64};

const ROUTER_NWK: u16 = 0x2AAE;
const ROUTER_IEEE: Eui64 = Eui64([0x81, 0x72, 0xCA, 0x3F, 0x8F, 0x38, 0xC1, 0xA4]);
const COORDINATOR_IEEE: Eui64 = Eui64([0x01, 0x00, 0xED, 0xC0, 0xDE, 0x00, 0xED, 0x00]);

struct Setup {
    medium: Arc<Medium>,
    coordinator_node: NodeId,
    router_node: NodeId,
    bridge: Arc<Bridge<SimPhy>>,
    router: Arc<ZigbeeStack<SimPhy>>,
    abeille: mpsc::UnboundedReceiver<Outbound>,
    state_path: std::path::PathBuf,
}

fn config(state_path: std::path::PathBuf) -> RunConfig {
    RunConfig {
        device: "sim".into(),
        baudrate: 0,
        flow_control: FlowControlMode::None,
        state_path,
        channel: 25,
        tx_power: 8,
        allow_erase: false,
        listen: "127.0.0.1:0".parse().unwrap(),
    }
}

async fn setup(name: &str) -> Setup {
    let dir = std::env::temp_dir().join(format!("zigate-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("network.json");

    let medium = Medium::new(3);
    let coordinator_node = medium.add_node("coordinator", COORDINATOR_IEEE);
    let router_node = medium.add_node("router", ROUTER_IEEE);

    let bridge = start_bridge(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        COORDINATOR_IEEE,
        &config(state_path.clone()),
    )
    .await
    .expect("bridge starts");
    let (tx, abeille) = mpsc::unbounded_channel();
    bridge.attach(tx);

    // The router is provisioned with the bridge's network, as if it had joined
    let mut network = NetworkState::load(&state_path)
        .unwrap()
        .unwrap()
        .network_config()
        .unwrap();
    network.role = NwkDeviceType::Router;
    network.network_address = Nwk(ROUTER_NWK);
    network.ieee_address = ROUTER_IEEE;
    network.network_key_tx_counter = 0;
    let router = ZigbeeStack::new(
        Arc::new(SimPhy::new(medium.clone(), router_node)),
        network,
        Tunables::default(),
        TokioSpawner::default(),
    );
    router.start_network().await.expect("router starts");
    router.spawn_tracked(|s| async move { s.run().await });

    // Link status exchanges
    tokio::time::sleep(Duration::from_secs(40)).await;

    Setup {
        medium,
        coordinator_node,
        router_node,
        bridge,
        router,
        abeille,
        state_path,
    }
}

/// Next message for Abeille of the given type, skipping others
async fn expect(abeille: &mut mpsc::UnboundedReceiver<Outbound>, msg_type: u16) -> (Message, u8) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (message, lqi) = abeille.recv().await.expect("bridge alive");
            if message.msg_type == msg_type {
                return (message, lqi);
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {msg_type:04X} for Abeille"))
}

/// Abeille's 0530 for an On/Off Toggle to the router, with APS ack (addr mode 02)
fn toggle() -> Message {
    let zcl = [0x11, 0x01, 0x02];
    let mut payload = vec![0x02];
    payload.extend(ROUTER_NWK.to_be_bytes());
    payload.extend([0x01, 0x01]); // src/dst endpoints
    payload.extend(0x0006u16.to_be_bytes()); // cluster before profile
    payload.extend(0x0104u16.to_be_bytes());
    payload.extend([0x02, 0x1E, u8::try_from(zcl.len()).unwrap()]);
    payload.extend(zcl);
    Message {
        msg_type: msg::RAW_APS_DATA,
        payload,
    }
}

async fn next_aps(stack: &ZigbeeStack<SimPhy>) -> (Nwk, u16, Vec<u8>) {
    loop {
        for n in stack.next_notifications().await {
            if let ZigbeeNotification::ReceivedApsCommand {
                source,
                cluster_id,
                data,
                profile_id: 0x0104,
                ..
            } = n
            {
                return (source, cluster_id, data);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn handshake_matches_abeille_expectations() {
    let mut s = setup("handshake").await;

    s.bridge.handle(&Message {
        msg_type: msg::START_NETWORK,
        payload: vec![],
    });
    let (status, _) = expect(&mut s.abeille, msg::STATUS).await;
    assert_eq!(status.payload[..4], [0x00, 0x01, 0x00, 0x24]);
    let (formed, _) = expect(&mut s.abeille, msg::NETWORK_FORMED).await;
    // Newly formed, coordinator 0000, the radio's IEEE, channel 25
    let mut expected = vec![0x01, 0x00, 0x00];
    expected.extend(eui_to_u64(COORDINATOR_IEEE).to_be_bytes());
    expected.push(25);
    assert_eq!(formed.payload, expected);

    s.bridge.handle(&Message {
        msg_type: msg::PERMIT_JOIN,
        payload: vec![0xFF, 0xFC, 0xFE, 0x00],
    });
    expect(&mut s.abeille, msg::STATUS).await;
    s.bridge.handle(&Message {
        msg_type: msg::GET_PERMIT_JOIN,
        payload: vec![],
    });
    expect(&mut s.abeille, msg::STATUS).await;
    let (permit, _) = expect(&mut s.abeille, msg::PERMIT_JOIN_STATUS).await;
    assert_eq!(permit.payload, [0x01]);

    // Commands not implemented yet are refused cleanly, one 8000 each
    s.bridge.handle(&Message {
        msg_type: 0x0026,
        payload: vec![0x00, 0x00],
    });
    let (status, _) = expect(&mut s.abeille, msg::STATUS).await;
    assert_eq!(status.payload[0], 0x02);
    assert_eq!(status.payload[2..4], [0x00, 0x26]);

    // A malformed native command too
    s.bridge.handle(&Message {
        msg_type: msg::READ_ATTRIBUTE,
        payload: vec![0x02, 0x2A, 0xAE, 0x01, 0x01],
    });
    let (status, _) = expect(&mut s.abeille, msg::STATUS).await;
    assert_eq!(status.payload[0], 0x01);
    assert_eq!(status.payload[2..4], [0x01, 0x00]);
}

#[tokio::test(start_paused = true)]
async fn raw_aps_data_gets_8000_then_8011_with_the_same_tag() {
    let mut s = setup("raw-aps").await;

    s.bridge.handle(&toggle());
    let (status, _) = expect(&mut s.abeille, msg::STATUS).await;
    let [st, _sqn, t0, t1, sent, tag, npdu, apdu] = status.payload[..] else {
        panic!("8000 must be the 8-byte form: {:02X?}", status.payload);
    };
    assert_eq!(
        (st, t0, t1, sent, npdu, apdu),
        (0x00, 0x05, 0x30, 0x01, 0x00, 0x00)
    );

    let (source, cluster, data) =
        tokio::time::timeout(Duration::from_secs(10), next_aps(&s.router))
            .await
            .expect("router receives the toggle");
    assert_eq!((source, cluster), (Nwk(0x0000), 0x0006));
    assert_eq!(data, [0x11, 0x01, 0x02]);

    let (ack, _) = expect(&mut s.abeille, msg::APS_ACK).await;
    let mut expected = vec![0x00];
    expected.extend(ROUTER_NWK.to_be_bytes());
    expected.push(0x01);
    expected.extend(0x0006u16.to_be_bytes());
    expected.push(tag);
    assert_eq!(ack.payload, expected);
}

#[tokio::test(start_paused = true)]
async fn lost_aps_ack_is_reported_to_abeille() {
    let mut s = setup("lost-ack").await;

    // Establish the route, then make the router unable to answer
    s.bridge.handle(&toggle());
    expect(&mut s.abeille, msg::APS_ACK).await;
    s.medium
        .set_link(s.router_node, s.coordinator_node, Link::DOWN);

    s.bridge.handle(&toggle());
    let (status, _) = expect(&mut s.abeille, msg::STATUS).await;
    let tag = status.payload[5];
    let (ack, _) = expect(&mut s.abeille, msg::APS_ACK).await;
    assert_ne!(ack.payload[0], 0x00, "failure status expected");
    assert_eq!(ack.payload[6], tag);
}

#[tokio::test(start_paused = true)]
async fn reports_and_announces_reach_abeille() {
    let mut s = setup("indications").await;

    // A ZCL attribute report from the router: On/Off = 1
    let report = vec![0x18, 0x07, 0x0A, 0x00, 0x00, 0x10, 0x01];
    s.router
        .send_aps_unicast(
            Nwk(0x0000),
            0x0104,
            0x0006,
            1,
            1,
            ApsAck::Request,
            30,
            report.clone(),
            None,
            false,
            TxPriority::UserNormal,
            RouteDirective::StackDecides,
        )
        .unwrap();
    let (indication, lqi) = expect(&mut s.abeille, msg::DATA_INDICATION).await;
    let mut expected = vec![0x00, 0x01, 0x04, 0x00, 0x06, 0x01, 0x01, 0x02];
    expected.extend(ROUTER_NWK.to_be_bytes());
    expected.extend([0x02, 0x00, 0x00]);
    expected.extend(&report);
    assert_eq!(indication.payload, expected);
    assert_eq!(lqi, 255);

    // Device_annce broadcast: seq, nwk (LE), ieee (LE), capability (router, mains)
    let mut annce = vec![0x42];
    annce.extend(ROUTER_NWK.to_le_bytes());
    annce.extend(ROUTER_IEEE.to_bytes());
    annce.push(0x8E);
    s.router
        .send_aps_broadcast(
            Nwk(0xFFFD),
            0x0000,
            0x0013,
            0,
            0,
            30,
            annce,
            TxPriority::UserNormal,
        )
        .unwrap();
    let (announce, _) = expect(&mut s.abeille, msg::DEVICE_ANNOUNCE).await;
    let mut expected = ROUTER_NWK.to_be_bytes().to_vec();
    expected.extend(eui_to_u64(ROUTER_IEEE).to_be_bytes());
    expected.push(0x8E);
    assert_eq!(announce.payload, expected);

    // The new device is in the saved network state
    let saved = NetworkState::load(&s.state_path).unwrap().unwrap();
    assert!(
        saved
            .devices
            .iter()
            .any(|d| d.ieee == "A4C1388F3FCA7281" && d.nwk == "2AAE"),
        "{:?}",
        saved.devices
    );
}

#[tokio::test(start_paused = true)]
async fn restart_restores_the_network() {
    let dir = std::env::temp_dir().join(format!("zigate-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("network.json");

    let medium = Medium::new(5);
    let first = medium.add_node("first", COORDINATOR_IEEE);
    let bridge = start_bridge(
        Arc::new(SimPhy::new(medium.clone(), first)),
        COORDINATOR_IEEE,
        &config(path.clone()),
    )
    .await
    .unwrap();
    let before = bridge.state();
    drop(bridge);

    // Same file, another radio: the network and the coordinator IEEE are kept
    let second = medium.add_node("second", Eui64([0x99; 8]));
    let bridge = start_bridge(
        Arc::new(SimPhy::new(medium, second)),
        Eui64([0x99; 8]),
        &config(path),
    )
    .await
    .unwrap();
    let after = bridge.state();
    assert_eq!(after.pan_id, before.pan_id);
    assert_eq!(after.network_key, before.network_key);
    assert_eq!(after.coordinator_ieee, before.coordinator_ieee);
    assert!(after.nwk_frame_counter > before.nwk_frame_counter);
}

#[tokio::test(start_paused = true)]
async fn native_read_attribute_reaches_the_device() {
    let mut s = setup("native-read").await;

    // Abeille readAttribute: cluster 0000, attributes 0004 (manufacturer) and 0005 (model)
    s.bridge.handle(&Message {
        msg_type: msg::READ_ATTRIBUTE,
        payload: vec![
            0x02, 0x2A, 0xAE, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x04,
            0x00, 0x05,
        ],
    });
    let (status, _) = expect(&mut s.abeille, msg::STATUS).await;
    assert_eq!(status.payload[..5], [0x00, 0x01, 0x01, 0x00, 0x01]);

    let (source, cluster, data) =
        tokio::time::timeout(Duration::from_secs(10), next_aps(&s.router))
            .await
            .expect("router receives the read");
    assert_eq!((source, cluster), (Nwk(0x0000), 0x0000));
    // Global read attributes, attribute ids little-endian
    assert_eq!(data[0], 0x10);
    assert_eq!(data[2..], [0x00, 0x04, 0x00, 0x05, 0x00]);
    expect(&mut s.abeille, msg::APS_ACK).await;
}

#[tokio::test(start_paused = true)]
async fn the_coordinator_answers_zdo_requests_itself() {
    let s = setup("zdo-server").await;

    // The router asks for the coordinator's node descriptor
    s.router
        .send_aps_unicast(
            Nwk(0x0000),
            0x0000,
            0x0002,
            0,
            0,
            ApsAck::None,
            30,
            vec![0x55, 0x00, 0x00],
            None,
            false,
            TxPriority::UserNormal,
            RouteDirective::StackDecides,
        )
        .unwrap();

    let response = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            for n in s.router.next_notifications().await {
                if let ZigbeeNotification::ReceivedApsCommand {
                    profile_id: 0x0000,
                    cluster_id: 0x8002,
                    data,
                    ..
                } = n
                {
                    return data;
                }
            }
        }
    })
    .await
    .expect("Node_Desc_rsp from the coordinator");
    // seq, success, nwk 0000, then a coordinator descriptor with NXP's manufacturer code
    assert_eq!(response[..4], [0x55, 0x00, 0x00, 0x00]);
    assert_eq!(response[4..9], [0x00, 0x40, 0x8F, 0x37, 0x10]);
}
