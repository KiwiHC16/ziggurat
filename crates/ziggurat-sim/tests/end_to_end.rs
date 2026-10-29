//! Full Ziggurat stacks on the simulated medium, without hardware.

use std::sync::Arc;
use std::time::Duration;

use ziggurat_driver::runtime::TokioSpawner;
use ziggurat_driver::zigbee_stack::{
    ApsAck, DeliveryError, HostRoute, NetworkConfig, NwkDeviceType, RouteDirective, Tunables,
    TxPriority, WELL_KNOWN_LINK_KEY, ZigbeeNotification, ZigbeeStack,
};
use ziggurat_ieee_802154::types::{Eui64, Key, Nwk, PanId};
use ziggurat_phy::RadioPhy;
use ziggurat_phy_spinel::SpinelPhy;
use ziggurat_sim::medium::{Link, Medium, NodeId};
use ziggurat_sim::phy::SimPhy;
use ziggurat_sim::rcp;
use ziggurat_spinel::client::SpinelClient;

const CHANNEL: u8 = 25;
const PAN_ID: PanId = PanId(0x1A62);
const ROUTER_NWK: Nwk = Nwk(0x2AAE);
const PROFILE_HA: u16 = 0x0104;
const CLUSTER_ON_OFF: u16 = 0x0006;

const fn network(role: NwkDeviceType, nwk: Nwk, ieee: Eui64) -> NetworkConfig {
    NetworkConfig {
        role,
        channel: CHANNEL,
        update_id: 0,
        pan_id: PAN_ID,
        extended_pan_id: Eui64([0xDD; 8]),
        network_address: nwk,
        ieee_address: ieee,
        network_key: Key([0x5A; 16]),
        network_key_seq_number: 0,
        network_key_tx_counter: 0,
        tc_link_key: WELL_KNOWN_LINK_KEY,
        tclk_seed: None,
        tx_power: 8,
        source_routing: false,
    }
}

async fn start<P: RadioPhy>(phy: Arc<P>, config: NetworkConfig) -> Arc<ZigbeeStack<P>> {
    let stack = ZigbeeStack::new(phy, config, Tunables::default(), TokioSpawner::default());
    stack.start_network().await.expect("network starts");
    stack.spawn_tracked(|s| async move { s.run().await });
    stack
}

/// Wait for the first APS data frame delivered to the application
async fn next_aps<P: RadioPhy>(stack: &ZigbeeStack<P>) -> (Nwk, u16, Vec<u8>) {
    loop {
        for n in stack.next_notifications().await {
            if let ZigbeeNotification::ReceivedApsCommand {
                source,
                cluster_id,
                data,
                profile_id: PROFILE_HA,
                ..
            } = n
            {
                return (source, cluster_id, data);
            }
        }
    }
}

fn on_off_toggle<P: RadioPhy>(stack: &ZigbeeStack<P>) -> ziggurat_driver::zigbee_stack::SendHandle {
    on_off_toggle_routed(stack, RouteDirective::StackDecides)
}

fn on_off_toggle_routed<P: RadioPhy>(
    stack: &ZigbeeStack<P>,
    route: RouteDirective,
) -> ziggurat_driver::zigbee_stack::SendHandle {
    // ZCL: cluster-specific, client to server, disable default response, seq 1, Toggle
    stack
        .send_aps_unicast(
            ROUTER_NWK,
            PROFILE_HA,
            CLUSTER_ON_OFF,
            1,
            1,
            ApsAck::Request,
            30,
            vec![0x11, 0x01, 0x02],
            None,
            false,
            TxPriority::UserNormal,
            route,
        )
        .expect("send admitted")
}

struct TwoNodes {
    medium: Arc<Medium>,
    coordinator_node: NodeId,
    router_node: NodeId,
    coordinator: Arc<ZigbeeStack<SimPhy>>,
    router: Arc<ZigbeeStack<SimPhy>>,
}

async fn two_nodes() -> TwoNodes {
    let medium = Medium::new(1);
    let coordinator_node = medium.add_node("coordinator", Eui64([0x01; 8]));
    let router_node = medium.add_node("router", Eui64([0x02; 8]));

    let coordinator_phy = Arc::new(SimPhy::new(medium.clone(), coordinator_node));
    let router_phy = Arc::new(SimPhy::new(medium.clone(), router_node));

    let coordinator = start(
        coordinator_phy,
        network(NwkDeviceType::Coordinator, Nwk(0x0000), Eui64([0x01; 8])),
    )
    .await;
    // The router is provisioned as already joined: same network key, fixed address
    let router = start(
        router_phy,
        network(NwkDeviceType::Router, ROUTER_NWK, Eui64([0x02; 8])),
    )
    .await;

    // Let link status exchanges populate both neighbor tables
    tokio::time::sleep(Duration::from_secs(40)).await;

    TwoNodes {
        medium,
        coordinator_node,
        router_node,
        coordinator,
        router,
    }
}

#[tokio::test(start_paused = true)]
async fn aps_unicast_with_ack_between_two_stacks() {
    let net = two_nodes().await;

    let handle = on_off_toggle(&net.coordinator);
    let (source, cluster, data) =
        tokio::time::timeout(Duration::from_secs(10), next_aps(&net.router))
            .await
            .expect("router receives the frame");

    assert_eq!(source, Nwk(0x0000));
    assert_eq!(cluster, CLUSTER_ON_OFF);
    assert_eq!(data, [0x11, 0x01, 0x02]);
    handle.handed_off().await.expect("MAC ack from the router");
    handle.delivered().await.expect("APS ack from the router");
}

#[tokio::test(start_paused = true)]
async fn one_way_link_loses_the_aps_ack() {
    let net = two_nodes().await;

    // A first exchange establishes the route (discovery needs the reverse link)
    let first = on_off_toggle(&net.coordinator);
    tokio::time::timeout(Duration::from_secs(10), next_aps(&net.router))
        .await
        .expect("router receives the first frame");
    first.delivered().await.expect("first frame acked");

    // The router still hears the coordinator, but nothing it sends gets through: its
    // MAC acks and its APS ack are lost (the ZiGate JnJ / cellier pattern)
    net.medium
        .set_link(net.router_node, net.coordinator_node, Link::DOWN);

    let handle = on_off_toggle(&net.coordinator);
    let (_, cluster, _) = tokio::time::timeout(Duration::from_secs(10), next_aps(&net.router))
        .await
        .expect("router still receives the frame");
    assert_eq!(cluster, CLUSTER_ON_OFF);

    let result = handle.handed_off().await;
    assert!(
        matches!(result, Err(DeliveryError::NwkNoAck { .. })),
        "{result:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn one_way_neighbor_is_reached_through_a_relay() {
    let medium = Medium::new(1);
    let coordinator_node = medium.add_node("coordinator", Eui64([0x01; 8]));
    let relay_node = medium.add_node("relay", Eui64([0x03; 8]));
    let far_node = medium.add_node("far", Eui64([0x02; 8]));
    // The coordinator hears the far router, which does not hear it (T2 on the Terrasse
    // network of jeedompro, 29/10/2026); the relay hears both
    medium.set_link(coordinator_node, far_node, Link::DOWN);

    let coordinator = start(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        network(NwkDeviceType::Coordinator, Nwk(0x0000), Eui64([0x01; 8])),
    )
    .await;
    let _relay = start(
        Arc::new(SimPhy::new(medium.clone(), relay_node)),
        network(NwkDeviceType::Router, Nwk(0x1111), Eui64([0x03; 8])),
    )
    .await;
    let far = start(
        Arc::new(SimPhy::new(medium.clone(), far_node)),
        network(NwkDeviceType::Router, ROUTER_NWK, Eui64([0x02; 8])),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(40)).await;

    // Sent directly, as the bridge hints for a neighbor: lost
    let direct = on_off_toggle_routed(
        &coordinator,
        RouteDirective::Hint(HostRoute::NextHop(ROUTER_NWK)),
    );
    let result = direct.handed_off().await;
    assert!(
        matches!(result, Err(DeliveryError::NwkNoAck { .. })),
        "{result:?}"
    );
    let link = coordinator.state.core.lock().nib.neighbors.link(ROUTER_NWK);
    assert!(link.is_some_and(|link| link.is_one_way()), "{link:?}");

    // Left to the stack, the frame goes through the relay
    let routed = on_off_toggle(&coordinator);
    let (source, cluster, _) = tokio::time::timeout(Duration::from_secs(10), next_aps(&far))
        .await
        .expect("far router receives the frame through the relay");
    assert_eq!(source, Nwk(0x0000));
    assert_eq!(cluster, CLUSTER_ON_OFF);
    routed
        .delivered()
        .await
        .expect("APS ack back through the relay");
}

#[tokio::test(start_paused = true)]
async fn stack_starts_on_the_simulated_spinel_rcp() {
    let medium = Medium::new(1);
    let hw = Eui64([0x01, 0x00, 0xED, 0x00, 0x00, 0x4B, 0x12, 0x00]);
    let dongle = medium.add_node("dongle", hw);

    let (host_side, rcp_side) = tokio::io::duplex(4096);
    tokio::spawn(rcp::serve(rcp_side, medium.clone(), dongle));

    let phy = Arc::new(SpinelPhy::new(Arc::new(SpinelClient::new(Box::new(
        host_side,
    )))));
    assert_eq!(phy.hw_address().await.expect("HWADDR"), hw);

    let _stack = start(phy, network(NwkDeviceType::Coordinator, Nwk(0x0000), hw)).await;

    // The radio was programmed through Spinel property sets
    let config = medium.config(dongle).expect("radio enabled");
    assert_eq!(config.channel, CHANNEL);
    assert_eq!(config.pan_id, PAN_ID);
    assert_eq!(config.short_address, Nwk(0x0000));
    assert_eq!(config.extended_address, hw);
    assert!(config.rx_on_when_idle);
}

#[tokio::test(start_paused = true)]
async fn spinel_rcp_coordinator_talks_to_a_simulated_router() {
    let medium = Medium::new(1);
    let hw = Eui64([0x01; 8]);
    let dongle = medium.add_node("dongle", hw);
    let router_node = medium.add_node("router", Eui64([0x02; 8]));

    let (host_side, rcp_side) = tokio::io::duplex(4096);
    tokio::spawn(rcp::serve(rcp_side, medium.clone(), dongle));
    let coordinator_phy = Arc::new(SpinelPhy::new(Arc::new(SpinelClient::new(Box::new(
        host_side,
    )))));

    let coordinator = start(
        coordinator_phy,
        network(NwkDeviceType::Coordinator, Nwk(0x0000), hw),
    )
    .await;
    let router = start(
        Arc::new(SimPhy::new(medium.clone(), router_node)),
        network(NwkDeviceType::Router, ROUTER_NWK, Eui64([0x02; 8])),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(40)).await;

    let handle = on_off_toggle(&coordinator);
    let (source, _, data) = tokio::time::timeout(Duration::from_secs(10), next_aps(&router))
        .await
        .expect("router receives the frame through the Spinel RCP");
    assert_eq!(source, Nwk(0x0000));
    assert_eq!(data, [0x11, 0x01, 0x02]);
    handle
        .delivered()
        .await
        .expect("APS ack through the Spinel RCP");
}
