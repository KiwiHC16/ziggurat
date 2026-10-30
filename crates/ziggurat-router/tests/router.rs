//! The router binary's logic against a simulated coordinator.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ziggurat_driver::join::JoinParams;
use ziggurat_driver::runtime::TokioSpawner;
use ziggurat_driver::zigbee_stack::{
    ApsAck, NetworkConfig, NwkDeviceType, RouteDirective, Tunables, TxPriority, ZigbeeNotification,
    ZigbeeStack,
};
use ziggurat_ieee_802154::types::{Eui64, Key, Nwk, PanId};
use ziggurat_router::control;
use ziggurat_router::router::Router;
use ziggurat_router::state::RouterState;
use ziggurat_sim::medium::Medium;
use ziggurat_sim::phy::SimPhy;
use ziggurat_zigbee::constants::WELL_KNOWN_LINK_KEY;

const COORDINATOR_IEEE: Eui64 = Eui64([0x01; 8]);
const ROUTER_IEEE: Eui64 = Eui64([0x02; 8]);

fn coordinator_config() -> NetworkConfig {
    NetworkConfig {
        role: NwkDeviceType::Coordinator,
        channel: 20,
        update_id: 0,
        pan_id: PanId(0x1A62),
        extended_pan_id: Eui64([0xDD; 8]),
        network_address: Nwk(0x0000),
        ieee_address: COORDINATOR_IEEE,
        network_key: Key([0x5A; 16]),
        network_key_seq_number: 0,
        network_key_tx_counter: 0,
        tc_link_key: WELL_KNOWN_LINK_KEY,
        tclk_seed: None,
        tx_power: 8,
        source_routing: false,
    }
}

fn temp_state(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ziggurat-router-{name}-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn params() -> JoinParams {
    let mut params = JoinParams::new(ROUTER_IEEE, WELL_KNOWN_LINK_KEY);
    params.channels = vec![20];
    params
}

async fn network() -> (Arc<Medium>, Arc<ZigbeeStack<SimPhy>>, Arc<SimPhy>) {
    let medium = Medium::new(1);
    let coordinator_node = medium.add_node("coordinator", COORDINATOR_IEEE);
    let router_node = medium.add_node("router", ROUTER_IEEE);
    let stack = ZigbeeStack::new(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        coordinator_config(),
        Tunables::default(),
        TokioSpawner::default(),
    );
    stack.start_network().await.expect("network starts");
    stack.spawn_tracked(|s| async move { s.run().await });
    stack.permit_joins(120, true);
    let phy = Arc::new(SimPhy::new(medium.clone(), router_node));
    (medium, stack, phy)
}

/// Wait for the coordinator to receive `cluster` and return its ASDU
async fn coordinator_receives(stack: &ZigbeeStack<SimPhy>, cluster: u16) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            for notification in stack.next_notifications().await {
                if let ZigbeeNotification::ReceivedApsCommand {
                    cluster_id, data, ..
                } = notification
                    && cluster_id == cluster
                {
                    return data;
                }
            }
        }
    })
    .await
    .expect("frame reaches the coordinator")
}

fn unicast(
    stack: &ZigbeeStack<SimPhy>,
    to: u16,
    profile: u16,
    cluster: u16,
    ep: u8,
    data: Vec<u8>,
) {
    stack
        .send_aps_unicast(
            Nwk(to),
            profile,
            cluster,
            ep,
            ep,
            ApsAck::None,
            30,
            data,
            None,
            false,
            TxPriority::UserNormal,
            RouteDirective::StackDecides,
        )
        .expect("sent");
}

#[tokio::test(start_paused = true)]
async fn is_interviewed_and_sends_on_request() {
    let (_medium, coordinator, phy) = network().await;
    let path = temp_state("interview");
    let router = Router::start(phy, params(), &path).await.expect("starts");
    let address = router.address;

    let annce = coordinator_receives(&coordinator, 0x0013).await;
    assert_eq!(annce[1..3], address.to_le_bytes());

    // Let link status exchanges establish the link cost
    tokio::time::sleep(Duration::from_secs(40)).await;

    // Node_Desc_req: logical type router
    let mut request = vec![0x10];
    request.extend(address.to_le_bytes());
    unicast(&coordinator, address, 0x0000, 0x0002, 0, request);
    let response = coordinator_receives(&coordinator, 0x8002).await;
    assert_eq!(response[1], 0x00);
    assert_eq!(response[4] & 0x07, 0x01);

    // Basic read: model identifier
    unicast(
        &coordinator,
        address,
        0x0104,
        0x0000,
        1,
        vec![0x00, 0x22, 0x00, 0x05, 0x00],
    );
    let response = coordinator_receives(&coordinator, 0x0000).await;
    assert!(
        response.windows(15).any(|w| w == b"ziggurat-router"),
        "{response:02x?}"
    );

    // The control port's send
    let reply = control::execute(router.as_ref(), "send 0000 0104 0006 01 01 010203");
    assert_eq!(reply, "ok");
    assert_eq!(
        coordinator_receives(&coordinator, 0x0006).await,
        vec![1, 2, 3]
    );

    let status = control::execute(router.as_ref(), "status");
    assert!(
        status.starts_with(&format!("status 1a62 {address:04x} 20 ")),
        "{status}"
    );
    assert!(control::execute(router.as_ref(), "send zz").starts_with("err "));

    std::fs::remove_file(path).ok();
}

#[tokio::test(start_paused = true)]
async fn restarts_without_joining_with_higher_counters() {
    let (_medium, coordinator, phy) = network().await;
    let path = temp_state("restart");
    let first = Router::start(phy.clone(), params(), &path)
        .await
        .expect("starts");
    let address = first.address;
    coordinator_receives(&coordinator, 0x0013).await;
    let saved = RouterState::load(&path).expect("loads").expect("present");
    assert_eq!(saved.network_key, "5a".repeat(16));
    drop(first);

    // Joins are closed now: only the saved state can bring it back
    coordinator.permit_joins(0, false);
    let second = Router::start(phy, params(), &path).await.expect("restarts");
    assert_eq!(second.address, address);
    let again = RouterState::load(&path).expect("loads").expect("present");
    assert!(again.nwk_frame_counter > saved.nwk_frame_counter);

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path)
        .expect("exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    std::fs::remove_file(path).ok();
}
