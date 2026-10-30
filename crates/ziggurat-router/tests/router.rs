//! The router binary's logic against a simulated coordinator.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ziggurat_driver::join::{JoinParams, JoinedNetwork, join_network};
use ziggurat_driver::runtime::{DefaultRuntime, TokioSpawner};
use ziggurat_driver::zigbee_stack::{
    ApsAck, NetworkConfig, NwkDeviceType, RouteDirective, Tunables, TxPriority, ZigbeeNotification,
    ZigbeeStack,
};
use ziggurat_ieee_802154::commands::Ieee802154DataRequestCommand;
use ziggurat_ieee_802154::types::{Eui64, Key, Nwk, PanId};
use ziggurat_ieee_802154::{
    Ieee802154Address, Ieee802154AddressingMode, Ieee802154CommandFrame, Ieee802154CommandPayload,
    Ieee802154Frame, Ieee802154FrameControl, Ieee802154FrameHeader, Ieee802154FrameType,
    Ieee802154FrameVersion,
};
use ziggurat_phy::{RadioPhy, Receiver, RxFrame, TxFrame};
use ziggurat_router::control;
use ziggurat_router::router::Router;
use ziggurat_router::state::RouterState;
use ziggurat_sim::medium::Medium;
use ziggurat_sim::phy::SimPhy;
use ziggurat_zigbee::constants::WELL_KNOWN_LINK_KEY;

const COORDINATOR_IEEE: Eui64 = Eui64([0x01; 8]);
const ROUTER_IEEE: Eui64 = Eui64([0x02; 8]);
const THIRD_IEEE: Eui64 = Eui64([0x03; 8]);

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

/// A MAC Data Request from a joined device to its parent
fn data_request(joined: &JoinedNetwork) -> TxFrame {
    let frame: Ieee802154Frame = Ieee802154Frame::Command(Ieee802154CommandFrame {
        header: Ieee802154FrameHeader {
            frame_control: Ieee802154FrameControl {
                frame_type: Ieee802154FrameType::Command,
                security_enabled: false,
                frame_pending: false,
                ack_request: true,
                pan_id_compression: true,
                reserved1: false,
                sequence_number_suppression: false,
                information_elements_present: false,
                dest_addr_mode: Ieee802154AddressingMode::Short,
                frame_version: Ieee802154FrameVersion::Ieee2003,
                src_addr_mode: Ieee802154AddressingMode::Short,
            },
            sequence_number: Some(0x60),
            dest_pan_id: Some(joined.pan_id),
            dest_address: Some(Ieee802154Address::Nwk(joined.parent)),
            src_pan_id: None,
            src_address: Some(Ieee802154Address::Nwk(joined.network_address)),
        },
        command_payload: Ieee802154CommandPayload::DataRequest(Ieee802154DataRequestCommand),
        fcs: 0,
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

trait TryRecv {
    fn try_recv_frame(&mut self) -> Option<RxFrame>;
}

impl<T: Receiver<RxFrame>> TryRecv for T {
    /// The next frame already delivered, without waiting
    fn try_recv_frame(&mut self) -> Option<RxFrame> {
        let mut pending = std::pin::pin!(self.recv());
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        match pending.as_mut().poll(&mut context) {
            std::task::Poll::Ready(frame) => frame,
            std::task::Poll::Pending => None,
        }
    }
}

/// Drain the frames heard so far and count the data frames addressed to `address`. The
/// simulated radio does not sleep, so it also hears the network's broadcasts.
fn unicasts_to<T: Receiver<RxFrame>>(rx: &mut T, address: u16) -> usize {
    let mut count = 0;
    while let Some(packet) = rx.try_recv_frame() {
        if let Ok(Ieee802154Frame::Data(frame)) =
            Ieee802154Frame::from_bytes_without_fcs(&packet.psdu)
            && frame.header.dest_address == Some(Ieee802154Address::Nwk(Nwk(address)))
        {
            count += 1;
        }
    }
    count
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
    let router = Router::start(phy, params(), &path, false)
        .await
        .expect("starts");
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
    let first = Router::start(phy.clone(), params(), &path, false)
        .await
        .expect("starts");
    let address = first.address;
    coordinator_receives(&coordinator, 0x0013).await;
    let saved = RouterState::load(&path).expect("loads").expect("present");
    assert_eq!(saved.network_key, "5a".repeat(16));
    drop(first);

    // Joins are closed now: only the saved state can bring it back
    coordinator.permit_joins(0, false);
    let second = Router::start(phy, params(), &path, false)
        .await
        .expect("restarts");
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

/// A coordinator and a router taking children; the third node only hears the router.
async fn with_child(
    name: &str,
) -> (
    Arc<ZigbeeStack<SimPhy>>,
    Arc<Router<SimPhy>>,
    PathBuf,
    ziggurat_driver::join::JoinedNetwork,
    Arc<SimPhy>,
    Arc<SimPhy>,
) {
    let medium = Medium::new(1);
    let coordinator_node = medium.add_node("coordinator", COORDINATOR_IEEE);
    let router_node = medium.add_node("router", ROUTER_IEEE);
    let third_node = medium.add_node("third", THIRD_IEEE);
    medium.set_link_both(
        coordinator_node,
        third_node,
        ziggurat_sim::medium::Link::DOWN,
    );

    let coordinator = ZigbeeStack::new(
        Arc::new(SimPhy::new(medium.clone(), coordinator_node)),
        coordinator_config(),
        Tunables::default(),
        TokioSpawner::default(),
    );
    coordinator.start_network().await.expect("network starts");
    coordinator.spawn_tracked(|s| async move { s.run().await });
    coordinator.permit_joins(300, true);

    let path = temp_state(name);
    let phy = Arc::new(SimPhy::new(medium.clone(), router_node));
    let router = Router::start(phy.clone(), params(), &path, true)
        .await
        .expect("starts");
    tokio::time::sleep(Duration::from_secs(40)).await;

    // The trust center's permit reaches the router over the air
    coordinator
        .send_aps_broadcast(
            Nwk(0xFFFC),
            0x0000,
            0x0036,
            0,
            0,
            30,
            vec![0x11, 200, 0x01],
            TxPriority::UserNormal,
        )
        .expect("permit sent");
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut wanted = params();
    wanted.ieee = THIRD_IEEE;
    wanted.rx_on_when_idle = false;
    let third = Arc::new(SimPhy::new(medium, third_node));
    let joined = join_network::<_, DefaultRuntime>(third.as_ref(), &wanted)
        .await
        .expect("joins through the router");
    (coordinator, router, path, joined, third, phy)
}

#[tokio::test(start_paused = true)]
async fn serves_a_sleeping_child() {
    let (coordinator, router, path, joined, third, router_phy) = with_child("child").await;
    let child = joined.network_address.0;
    assert_eq!(joined.parent, Nwk(router.address));
    assert_eq!(router.children(), vec![(THIRD_IEEE, child)]);

    // The parent answers for a child that sleeps
    let mut request = vec![0x20];
    request.extend(THIRD_IEEE.0);
    request.extend([0x00, 0x00]);
    coordinator
        .send_aps_broadcast(
            Nwk(0xFFFD),
            0x0000,
            0x0000,
            0,
            0,
            30,
            request,
            TxPriority::UserNormal,
        )
        .expect("sent");
    let response = coordinator_receives(&coordinator, 0x8000).await;
    assert_eq!(response[2..10], THIRD_IEEE.0);
    assert_eq!(response[10..12], child.to_le_bytes());

    // Downstream: the frame waits in the router until the child polls
    let mut rx = third.subscribe_rx();
    unicast(
        &coordinator,
        child,
        0x0104,
        0x0006,
        1,
        vec![0x01, 0x02, 0x01],
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        unicasts_to(&mut rx, child),
        0,
        "the router must hold the frame until the child polls"
    );
    third.transmit(data_request(&joined)).await.expect("polls");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        unicasts_to(&mut rx, child),
        1,
        "the poll releases the frame"
    );

    // A restart keeps the child
    router.save().expect("saves");
    let saved = RouterState::load(&path).expect("loads").expect("present");
    assert_eq!(saved.children.len(), 1);
    assert_eq!(saved.children[0].nwk, child);
    assert!(!saved.children[0].rx_on_when_idle);

    let restarted = Router::start(router_phy, params(), &path, true)
        .await
        .expect("restarts");
    assert_eq!(restarted.children(), vec![(THIRD_IEEE, child)]);
    std::fs::remove_file(path).ok();
}
