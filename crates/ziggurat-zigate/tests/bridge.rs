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
        warm_up: Duration::ZERO,
        capture: None,
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

/// Restored router children are not aged: an IKEA bulb restored as a direct child
/// was evicted ("left: KeepaliveTimeout") a day after a restart (jeedompro, 28/10)
#[tokio::test(start_paused = true)]
async fn restored_router_children_are_not_aged() {
    use ziggurat_zigate::state::Child;

    let dir = std::env::temp_dir().join(format!("zigate-restored-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("network.json");

    let medium = Medium::new(6);
    let first = medium.add_node("first", COORDINATOR_IEEE);
    let bridge = start_bridge(
        Arc::new(SimPhy::new(medium.clone(), first)),
        COORDINATOR_IEEE,
        &config(path.clone()),
    )
    .await
    .unwrap();
    drop(bridge);

    let mut saved = NetworkState::load(&path).unwrap().unwrap();
    saved.children = vec![
        Child {
            ieee: "000B57FFFED2AF6A".into(),
            nwk: "10FD".into(),
            rx_on_when_idle: true,
            router: true,
        },
        Child {
            ieee: "00158D0001ED4520".into(),
            nwk: "2C85".into(),
            rx_on_when_idle: false,
            router: false,
        },
    ];
    saved.save(&path).unwrap();

    let second = medium.add_node("second", COORDINATOR_IEEE);
    let bridge = start_bridge(
        Arc::new(SimPhy::new(medium, second)),
        COORDINATOR_IEEE,
        &config(path),
    )
    .await
    .unwrap();
    let (tx, mut abeille) = mpsc::unbounded_channel();
    bridge.attach(tx);

    // Past the day restored end devices get before their first keepalive
    tokio::time::sleep(Duration::from_secs(25 * 60 * 60)).await;

    let mut left = Vec::new();
    while let Ok((message, _)) = abeille.try_recv() {
        if message.msg_type == msg::LEAVE_INDICATION {
            left.push(message.payload);
        }
    }
    assert_eq!(
        left.len(),
        1,
        "only the silent end device leaves: {left:02X?}"
    );
    assert_eq!(left[0][..8], 0x00158D0001ED4520u64.to_be_bytes());

    let children = bridge.state().children;
    assert!(
        children.iter().any(|c| c.nwk == "10FD" && c.router),
        "{children:?}"
    );
    assert!(!children.iter().any(|c| c.nwk == "2C85"), "{children:?}");
}

/// Our own Parent_annce of the restored end devices must not drop them: on
/// jeedomsauna (26/10) three Xiaomi sensors were dropped 45 ms after the bridge
/// announced them ("is now parented by Nwk(0x0000)") and then asked to leave
#[tokio::test(start_paused = true)]
async fn own_parent_announcement_keeps_restored_end_devices() {
    use ziggurat_zigate::state::Child;

    let s = setup("own-parent-annce").await;
    drop(s.bridge);

    let mut saved = NetworkState::load(&s.state_path).unwrap().unwrap();
    saved.children = vec![Child {
        ieee: "00158D0001DF3D57".into(),
        nwk: "B95A".into(),
        rx_on_when_idle: false,
        router: false,
    }];
    saved.save(&s.state_path).unwrap();

    // Same medium with the router still running, which relays our broadcasts; the
    // first radio is cut off in case its tasks outlive the dropped bridge
    let second = s.medium.add_node("second", COORDINATOR_IEEE);
    s.medium
        .set_link_both(s.coordinator_node, s.router_node, Link::DOWN);
    s.medium
        .set_link_both(s.coordinator_node, second, Link::DOWN);
    let bridge = start_bridge(
        Arc::new(SimPhy::new(s.medium.clone(), second)),
        COORDINATOR_IEEE,
        &config(s.state_path.clone()),
    )
    .await
    .unwrap();

    // Past the jittered post-boot announcement and its relay
    tokio::time::sleep(Duration::from_secs(60)).await;

    let children = bridge.state().children;
    assert!(children.iter().any(|c| c.nwk == "B95A"), "{children:?}");
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

/// Abeille's identification of a newly announced device, against a simulated
/// BlitzWolf plug: announce, endpoints, descriptor, model, bind, then a command
#[tokio::test(start_paused = true)]
async fn the_coordinator_answers_its_own_routing_table_request() {
    let mut s = setup("local-rtg").await;

    // 0x0530: mode, dst, src_ep, dst_ep, cluster, profile, security, radius, len, seq, start
    let request = [
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x32, 0x00, 0x00, 0x28, 0x30, 0x02, 0x5A, 0x00,
    ];
    s.bridge.handle(&Message {
        msg_type: msg::RAW_APS_DATA,
        payload: request.to_vec(),
    });
    let (status, _) = expect(&mut s.abeille, msg::STATUS).await;
    assert_eq!(status.payload[0], 0x00);

    let (indication, _) = expect(&mut s.abeille, msg::DATA_INDICATION).await;
    assert_eq!(indication.payload[3..5], [0x80, 0x32]);
    assert_eq!(indication.payload[7..10], [0x02, 0x00, 0x00]);
    // ASDU: same seq, success, then the table header
    assert_eq!(indication.payload[13..15], [0x5A, 0x00]);
    assert_eq!(indication.payload[16], 0x00);
}

#[tokio::test(start_paused = true)]
async fn abeille_identifies_a_simulated_plug() {
    use ziggurat_sim::device::{Model, SimDevice};

    const PLUG_NWK: u16 = 0x1A01;
    const PLUG_IEEE: Eui64 = Eui64([0x01, 0x00, 0x00, 0x00, 0x00, 0x38, 0xC1, 0xA4]);

    let dir = std::env::temp_dir().join(format!("zigate-plug-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("network.json");

    let medium = Medium::new(9);
    let coordinator_node = medium.add_node("coordinator", COORDINATOR_IEEE);
    let plug_node = medium.add_node("plug", PLUG_IEEE);
    let bridge = start_bridge(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        COORDINATOR_IEEE,
        &config(state_path.clone()),
    )
    .await
    .unwrap();
    let (tx, mut abeille) = mpsc::unbounded_channel();
    bridge.attach(tx);

    let network = NetworkState::load(&state_path)
        .unwrap()
        .unwrap()
        .network_config()
        .unwrap();
    let _plug = SimDevice::start(
        Arc::new(SimPhy::new(medium, plug_node)),
        network,
        PLUG_IEEE,
        Nwk(PLUG_NWK),
        Model::blitzwolf_plug(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(40)).await;

    // Inclusion: the plug announces itself when joins open
    bridge.handle(&Message {
        msg_type: msg::PERMIT_JOIN,
        payload: vec![0xFF, 0xFC, 0xFE, 0x00],
    });
    let (announce, _) = expect(&mut abeille, msg::DEVICE_ANNOUNCE).await;
    assert_eq!(announce.payload[..2], PLUG_NWK.to_be_bytes());
    assert_eq!(announce.payload[2..10], eui_to_u64(PLUG_IEEE).to_be_bytes());

    // 0x8002 helper: wait for a ZDO/ZCL indication of this cluster from the plug
    async fn indication(abeille: &mut mpsc::UnboundedReceiver<Outbound>, cluster: u16) -> Vec<u8> {
        loop {
            let (m, _) = expect(abeille, msg::DATA_INDICATION).await;
            // status profile(2) cluster(2) src_ep dst_ep src_mode src(2) dst_mode dst(2)
            if m.payload[3..5] == cluster.to_be_bytes()
                && m.payload[8..10] == PLUG_NWK.to_be_bytes()
            {
                return m.payload[13..].to_vec();
            }
        }
    }

    // Active endpoints: 01 and F2
    bridge.handle(&Message {
        msg_type: msg::ACTIVE_ENDPOINTS,
        payload: PLUG_NWK.to_be_bytes().to_vec(),
    });
    let rsp = indication(&mut abeille, 0x8005).await;
    assert_eq!(rsp[1..], [0x00, 0x01, 0x1A, 0x02, 0x01, 0xF2]);

    // Simple descriptor of endpoint 01: HA profile, On/Off among the servers
    let mut payload = PLUG_NWK.to_be_bytes().to_vec();
    payload.push(0x01);
    bridge.handle(&Message {
        msg_type: msg::SIMPLE_DESCRIPTOR,
        payload,
    });
    let rsp = indication(&mut abeille, 0x8004).await;
    assert_eq!(rsp[5..8], [0x01, 0x04, 0x01]);
    assert!(rsp.windows(2).any(|w| w == [0x06, 0x00]));

    // Group membership, which Abeille asks before going on: a cluster-specific response
    let mut payload = vec![0x02];
    payload.extend(PLUG_NWK.to_be_bytes());
    payload.extend([0x01, 0x01, 0x00]);
    bridge.handle(&Message {
        msg_type: msg::GET_GROUP_MEMBERSHIP,
        payload,
    });
    let rsp = indication(&mut abeille, 0x0004).await;
    assert_eq!(rsp[0] & 0x03, 0x01, "cluster-specific frame type");
    assert_eq!(rsp[2], 0x02, "Get Group Membership Response");

    // Basic cluster: manufacturer name and model identifier, as Abeille's findModel needs
    let mut payload = vec![0x02];
    payload.extend(PLUG_NWK.to_be_bytes());
    payload.extend([
        0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x04, 0x00, 0x05,
    ]);
    bridge.handle(&Message {
        msg_type: msg::READ_ATTRIBUTE,
        payload,
    });
    let rsp = indication(&mut abeille, 0x0000).await;
    let text = String::from_utf8_lossy(&rsp);
    assert!(text.contains("_TZ3000_amdymr7l"), "{rsp:02X?}");
    assert!(text.contains("TS011F"), "{rsp:02X?}");

    // Bind On/Off of the plug to the coordinator: the bridge finds the plug's NWK address
    let mut payload = eui_to_u64(PLUG_IEEE).to_be_bytes().to_vec();
    payload.extend([0x01, 0x00, 0x06, 0x03]);
    payload.extend(eui_to_u64(COORDINATOR_IEEE).to_be_bytes());
    payload.push(0x01);
    bridge.handle(&Message {
        msg_type: msg::BIND,
        payload,
    });
    let rsp = indication(&mut abeille, 0x8021).await;
    assert_eq!(rsp[1], 0x00, "bind success");

    // Switch it on: the plug reports On/Off = 1
    let mut payload = vec![0x02];
    payload.extend(PLUG_NWK.to_be_bytes());
    payload.extend([0x01, 0x01, 0x01]);
    bridge.handle(&Message {
        msg_type: msg::ON_OFF,
        payload,
    });
    loop {
        let rsp = indication(&mut abeille, 0x0006).await;
        // Report attributes: FCF seq 0A, attribute 0000, bool, value
        if rsp[2] == 0x0A {
            assert_eq!(rsp[3..], [0x00, 0x00, 0x10, 0x01]);
            break;
        }
    }

    // Toggle: back off
    let mut payload = vec![0x02];
    payload.extend(PLUG_NWK.to_be_bytes());
    payload.extend([0x01, 0x01, 0x02]);
    bridge.handle(&Message {
        msg_type: msg::ON_OFF,
        payload,
    });
    loop {
        let rsp = indication(&mut abeille, 0x0006).await;
        if rsp[2] == 0x0A {
            assert_eq!(rsp[3..], [0x00, 0x00, 0x10, 0x00]);
            break;
        }
    }
}

/// A simulated IKEA remote drives a simulated plug through a group set up by Abeille
#[tokio::test(start_paused = true)]
async fn remote_switches_a_plug_through_a_group() {
    use ziggurat_sim::device::{Model, SimDevice};

    const PLUG_NWK: u16 = 0x1A01;
    const REMOTE_NWK: u16 = 0x1A02;
    const GROUP: u16 = 0x7A01;

    let dir = std::env::temp_dir().join(format!("zigate-group-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("network.json");

    let medium = Medium::new(11);
    let coordinator_node = medium.add_node("coordinator", COORDINATOR_IEEE);
    let bridge = start_bridge(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        COORDINATOR_IEEE,
        &config(state_path.clone()),
    )
    .await
    .unwrap();
    let (tx, mut abeille) = mpsc::unbounded_channel();
    bridge.attach(tx);

    let network = || {
        NetworkState::load(&state_path)
            .unwrap()
            .unwrap()
            .network_config()
            .unwrap()
    };
    let plug_ieee = Eui64([0x01, 0, 0, 0, 0, 0x38, 0xC1, 0xA4]);
    let remote_ieee = Eui64([0x01, 0, 0, 0xFE, 0xFF, 0x72, 0x02, 0x5C]);
    let plug_node = medium.add_node("plug", plug_ieee);
    let remote_node = medium.add_node("remote", remote_ieee);
    let plug = SimDevice::start(
        Arc::new(SimPhy::new(medium.clone(), plug_node)),
        network(),
        plug_ieee,
        Nwk(PLUG_NWK),
        Model::blitzwolf_plug(),
    )
    .await
    .unwrap();
    let remote = SimDevice::start(
        Arc::new(SimPhy::new(medium, remote_node)),
        network(),
        remote_ieee,
        Nwk(REMOTE_NWK),
        Model::ikea_remote(GROUP),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(40)).await;

    // Abeille's addGroup: 0060 mode 02, plug, ep 01 → 01, group
    let mut payload = vec![0x02];
    payload.extend(PLUG_NWK.to_be_bytes());
    payload.extend([0x01, 0x01]);
    payload.extend(GROUP.to_be_bytes());
    bridge.handle(&Message {
        msg_type: msg::ADD_GROUP,
        payload,
    });
    expect(&mut abeille, msg::APS_ACK).await;
    assert_eq!(plug.groups(), [GROUP]);

    // Abeille then reads the membership back
    let mut payload = vec![0x02];
    payload.extend(PLUG_NWK.to_be_bytes());
    payload.extend([0x01, 0x01, 0x00]);
    bridge.handle(&Message {
        msg_type: msg::GET_GROUP_MEMBERSHIP,
        payload,
    });
    loop {
        let (m, _) = expect(&mut abeille, msg::DATA_INDICATION).await;
        // The Add Group Response (command 00) comes first; wait for command 02
        if m.payload[3..5] == [0x00, 0x04] && m.payload[13 + 2] == 0x02 {
            // FCF seq 02 capacity count=1 group(LE)
            assert_eq!(m.payload[13 + 2..], [0x02, 0xFE, 0x01, 0x01, 0x7A]);
            break;
        }
    }

    // Button On: the remote's groupcast is seen by Abeille (group destination) and the
    // plug switches on and reports it
    remote.press(0x01).unwrap();
    let mut saw_remote = false;
    let mut saw_report = false;
    while !(saw_remote && saw_report) {
        let (m, _) = expect(&mut abeille, msg::DATA_INDICATION).await;
        let src = u16::from_be_bytes([m.payload[8], m.payload[9]]);
        if src == REMOTE_NWK && m.payload[3..5] == [0x00, 0x06] {
            // Destination mode 01 (group) 7A01, command On
            assert_eq!(m.payload[10..13], [0x01, 0x7A, 0x01]);
            assert_eq!(m.payload[13 + 2], 0x01);
            saw_remote = true;
        }
        if src == PLUG_NWK && m.payload[3..5] == [0x00, 0x06] && m.payload[13 + 2] == 0x0A {
            assert_eq!(m.payload[13 + 3..], [0x00, 0x00, 0x10, 0x01]);
            saw_report = true;
        }
    }
    assert!(plug.attribute(1, 0x0006, 0x0000).unwrap().value == [1]);

    // Button Off
    remote.press(0x00).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(plug.attribute(1, 0x0006, 0x0000).unwrap().value == [0]);
}

/// An OTA client's Query Next Image Request gets "no image available", as from a ZiGate
#[tokio::test(start_paused = true)]
async fn ota_queries_get_no_image_available() {
    let s = setup("ota").await;

    // Query Next Image Request as the Aqara plug sends it: field control 00,
    // manufacturer 115F, image type 0018, file version 0000002B
    s.router
        .send_aps_unicast(
            Nwk(0x0000),
            0x0104,
            0x0019,
            1,
            1,
            ApsAck::Request,
            30,
            vec![
                0x01, 0xA0, 0x01, 0x00, 0x5F, 0x11, 0x18, 0x00, 0x2B, 0x00, 0x00, 0x00,
            ],
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
                    cluster_id: 0x0019,
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
    .expect("OTA response from the coordinator");
    // Cluster-specific server to client, same sequence, Query Next Image Response,
    // NO_IMAGE_AVAILABLE
    assert_eq!(response, [0x19, 0xA0, 0x02, 0x98]);
}

/// A device's read of the Time attribute is answered by the bridge, as by a ZiGate
#[tokio::test(start_paused = true)]
async fn time_reads_get_the_time_attribute() {
    let s = setup("time").await;

    // Read Attributes on Time, as the Aqara ctrl_neutral2 sends it
    s.router
        .send_aps_unicast(
            Nwk(0x0000),
            0x0104,
            0x000A,
            1,
            1,
            ApsAck::None,
            30,
            vec![0x10, 0x53, 0x00, 0x00, 0x00],
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
                    cluster_id: 0x000A,
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
    .expect("Time response from the coordinator");
    // Global, server to client, same sequence, Read Attributes Response: attribute 0000,
    // success, UTCTime, then a date after 2020 (seconds since 2000)
    assert_eq!(response[..7], [0x18, 0x53, 0x01, 0x00, 0x00, 0x00, 0xE2]);
    let time = u32::from_le_bytes(response[7..11].try_into().unwrap());
    assert!(time > 631_152_000, "time {time}");
}
