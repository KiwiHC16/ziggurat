//! A device joins a coordinator from scratch, without pre-provisioned network key,
//! then runs as a childless router on the result.

use std::sync::Arc;
use std::time::Duration;

use ziggurat_driver::join::{JoinError, JoinParams, device_annce_payload, join_network};
use ziggurat_driver::runtime::TokioRuntime;
use ziggurat_driver::zigbee_stack::{
    NetworkConfig, NwkDeviceType, ZigbeeNotification, ZigbeeStack,
};
use ziggurat_ieee_802154::types::{Eui64, Key, Nwk, PanId};
use ziggurat_phy::RadioPhy;
use ziggurat_sim::medium::Medium;
use ziggurat_sim::phy::SimPhy;
use ziggurat_zigbee::constants::WELL_KNOWN_LINK_KEY;

const CHANNEL: u8 = 20;
const NETWORK_KEY: Key = Key([0x5A; 16]);
const COORDINATOR_IEEE: Eui64 = Eui64([0x01; 8]);
const JOINER_IEEE: Eui64 = Eui64([0x02; 8]);
const EXTENDED_PAN: Eui64 = Eui64([0xDD; 8]);

fn coordinator_config() -> NetworkConfig {
    NetworkConfig {
        role: NwkDeviceType::Coordinator,
        channel: CHANNEL,
        update_id: 0,
        pan_id: PanId(0x1A62),
        extended_pan_id: EXTENDED_PAN,
        network_address: Nwk(0x0000),
        ieee_address: COORDINATOR_IEEE,
        network_key: NETWORK_KEY,
        network_key_seq_number: 0,
        network_key_tx_counter: 0,
        tc_link_key: WELL_KNOWN_LINK_KEY,
        tclk_seed: None,
        tx_power: 8,
        source_routing: false,
    }
}

async fn start<P: RadioPhy>(phy: Arc<P>, config: NetworkConfig) -> Arc<ZigbeeStack<P>> {
    let stack = ZigbeeStack::new(
        phy,
        config,
        ziggurat_driver::zigbee_stack::Tunables::default(),
        ziggurat_driver::runtime::TokioSpawner::default(),
    );
    stack.start_network().await.expect("network starts");
    stack.spawn_tracked(|s| async move { s.run().await });
    stack
}

fn params() -> JoinParams {
    let mut params = JoinParams::new(JOINER_IEEE, WELL_KNOWN_LINK_KEY);
    params.channels = vec![15, CHANNEL];
    params
}

#[tokio::test(start_paused = true)]
async fn joins_and_exchanges_frames_as_a_router() {
    let medium = Medium::new(1);
    let coordinator_node = medium.add_node("coordinator", COORDINATOR_IEEE);
    let joiner_node = medium.add_node("joiner", JOINER_IEEE);
    let coordinator = start(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        coordinator_config(),
    )
    .await;
    coordinator.permit_joins(120, true);

    let joiner_phy = Arc::new(SimPhy::new(medium.clone(), joiner_node));
    let joined = join_network::<_, TokioRuntime>(joiner_phy.as_ref(), &params())
        .await
        .expect("joins");

    assert_eq!(joined.channel, CHANNEL);
    assert_eq!(joined.extended_pan_id, EXTENDED_PAN);
    assert_eq!(joined.network_key, NETWORK_KEY);
    assert_eq!(joined.trust_center, COORDINATOR_IEEE);
    assert_ne!(joined.network_address, Nwk(0x0000));

    let router = start(joiner_phy, joined.router_config(&params())).await;
    let annce = device_annce_payload(1, joined.network_address, JOINER_IEEE);
    router
        .send_aps_broadcast(
            Nwk(0xFFFD),
            0x0000,
            0x0013,
            0,
            0,
            30,
            annce,
            ziggurat_driver::zigbee_stack::TxPriority::UserNormal,
        )
        .expect("announces");

    // The coordinator decrypts what the router sends with the key it received
    let heard = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            for notification in coordinator.next_notifications().await {
                if let ZigbeeNotification::ReceivedApsCommand {
                    source,
                    cluster_id: 0x0013,
                    ..
                } = notification
                {
                    return source;
                }
            }
        }
    })
    .await
    .expect("Device_annce reaches the coordinator");
    assert_eq!(heard, joined.network_address);
}

#[tokio::test(start_paused = true)]
async fn closed_network_cannot_be_joined() {
    let medium = Medium::new(1);
    let coordinator_node = medium.add_node("coordinator", COORDINATOR_IEEE);
    let joiner_node = medium.add_node("joiner", JOINER_IEEE);
    let _coordinator = start(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        coordinator_config(),
    )
    .await;

    let joiner_phy = SimPhy::new(medium, joiner_node);
    let result = join_network::<_, TokioRuntime>(&joiner_phy, &params()).await;
    assert!(matches!(result, Err(JoinError::NoNetwork)), "{result:?}");
}

#[tokio::test(start_paused = true)]
async fn other_network_is_left_alone() {
    let medium = Medium::new(1);
    let coordinator_node = medium.add_node("coordinator", COORDINATOR_IEEE);
    let joiner_node = medium.add_node("joiner", JOINER_IEEE);
    let coordinator = start(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        coordinator_config(),
    )
    .await;
    coordinator.permit_joins(120, true);

    let mut wanted = params();
    wanted.extended_pan_id = Some(Eui64([0xEE; 8]));
    let joiner_phy = SimPhy::new(medium, joiner_node);
    let result = join_network::<_, TokioRuntime>(&joiner_phy, &wanted).await;
    assert!(matches!(result, Err(JoinError::NoNetwork)), "{result:?}");
}

/// A joined router must not attract devices it would ignore: out of the coordinator's
/// range, a third device finds nothing to join, even with joins open on the network.
#[tokio::test(start_paused = true)]
async fn joined_router_does_not_attract_joiners() {
    let medium = Medium::new(1);
    let coordinator_node = medium.add_node("coordinator", COORDINATOR_IEEE);
    let joiner_node = medium.add_node("joiner", JOINER_IEEE);
    let third_ieee = Eui64([0x03; 8]);
    let third_node = medium.add_node("third", third_ieee);
    medium.set_link_both(
        coordinator_node,
        third_node,
        ziggurat_sim::medium::Link::DOWN,
    );

    let coordinator = start(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        coordinator_config(),
    )
    .await;
    coordinator.permit_joins(120, true);

    let joiner_phy = Arc::new(SimPhy::new(medium.clone(), joiner_node));
    let joined = join_network::<_, TokioRuntime>(joiner_phy.as_ref(), &params())
        .await
        .expect("joins");
    let _router = start(joiner_phy, joined.router_config(&params())).await;

    let mut wanted = params();
    wanted.ieee = third_ieee;
    let third_phy = SimPhy::new(medium, third_node);
    let result = join_network::<_, TokioRuntime>(&third_phy, &wanted).await;
    assert!(matches!(result, Err(JoinError::NoNetwork)), "{result:?}");
}
