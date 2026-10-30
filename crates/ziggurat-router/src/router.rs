//! Bring the router on the network, keep its state saved and serve what is addressed
//! to it. Generic over the radio, so the simulator runs the same code.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::sync::broadcast;
use ziggurat_driver::join::{JoinParams, device_annce_payload, join_network};
use ziggurat_driver::runtime::{DefaultRuntime, TokioSpawner};
use ziggurat_driver::zigbee_stack::{
    ApsAck, RouteDirective, Tunables, TxPriority, ZigbeeNotification, ZigbeeStack,
};
use ziggurat_ieee_802154::types::{Eui64, Nwk};
use ziggurat_phy::RadioPhy;
use ziggurat_protocol::LoadChildrenPayload;

use crate::state::{RouterState, to_hex};
use crate::zdo;

const RADIUS: u8 = 30;
const BROADCAST_ROUTERS: Nwk = Nwk(0xFFFD);
const BROADCAST_MIN: u16 = 0xFFF8;
const SAVE_PERIOD: Duration = Duration::from_secs(60);

/// A frame received for this router, as shown on the control port
#[derive(Debug, Clone)]
pub struct Received {
    pub source: u16,
    pub profile: u16,
    pub cluster: u16,
    pub src_ep: u8,
    pub dst_ep: u8,
    pub lqi: u8,
    pub rssi: i8,
    pub data: Vec<u8>,
}

pub struct Router<P: RadioPhy> {
    pub stack: Arc<ZigbeeStack<P>>,
    pub ieee: Eui64,
    pub address: u16,
    state: Mutex<RouterState>,
    state_path: PathBuf,
    pub received: broadcast::Sender<Received>,
}

impl<P: RadioPhy> Router<P> {
    /// Join with `params` unless `state_path` already holds a network, then start
    pub async fn start(
        phy: Arc<P>,
        params: JoinParams,
        state_path: &Path,
        accept_children: bool,
    ) -> Result<Arc<Self>> {
        let state = match RouterState::load(state_path)? {
            Some(state) => {
                tracing::info!(
                    "Back on PAN {:04X} as {:04X} (channel {})",
                    state.pan_id,
                    state.network_address,
                    state.channel
                );
                state
            }
            None => {
                let joined = join_network::<P, DefaultRuntime>(phy.as_ref(), &params)
                    .await
                    .map_err(|e| anyhow!("joining: {e}"))?;
                tracing::info!(
                    "Joined PAN {:04X} as {:04X} through {:04X}",
                    joined.pan_id.0,
                    joined.network_address.0,
                    joined.parent.0
                );
                RouterState::new(&joined, &params)
            }
        };

        let network = state.network_config()?;
        let mut state = state;
        // The floor moved: a crash before the next save must not reuse these counters
        state.nwk_frame_counter = network.network_key_tx_counter;
        state.save(state_path)?;

        let ieee = state.ieee()?;
        let address = state.network_address;
        let stack = ZigbeeStack::new(phy, network, Tunables::default(), TokioSpawner::default());
        stack.set_accepts_children(accept_children);
        let entries = state.child_entries()?;
        ziggurat_protocol::apply_children(&stack, LoadChildrenPayload { entries });
        stack
            .start_network()
            .await
            .map_err(|e| anyhow!("starting the stack: {e}"))?;
        stack.spawn_tracked(|s| async move { s.run().await });

        let (received, _) = broadcast::channel(256);
        let router = Arc::new(Self {
            stack,
            ieee,
            address,
            state: Mutex::new(state),
            state_path: state_path.to_path_buf(),
            received,
        });
        router.announce()?;
        router.spawn_tasks();
        Ok(router)
    }

    fn announce(&self) -> Result<()> {
        let sequence = 1;
        self.stack
            .send_aps_broadcast(
                BROADCAST_ROUTERS,
                zdo::PROFILE_ZDO,
                0x0013,
                0,
                0,
                RADIUS,
                device_annce_payload(sequence, Nwk(self.address), self.ieee),
                TxPriority::UserNormal,
            )
            .map_err(|e| anyhow!("Device_annce rejected: {e:?}"))?;
        Ok(())
    }

    fn spawn_tasks(self: &Arc<Self>) {
        let router = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                for notification in router.stack.next_notifications().await {
                    router.on_notification(notification);
                }
            }
        });

        let router = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SAVE_PERIOD).await;
                if let Err(e) = router.save() {
                    tracing::error!("Failed to save the state: {e:#}");
                }
            }
        });
    }

    pub fn save(&self) -> Result<()> {
        let children = ziggurat_protocol::child_entries(&self.stack);
        let mut state = self.state.lock().expect("state lock");
        state.set_children(children);
        state
            .save(&self.state_path)
            .context("saving the router state")
    }

    /// Our children, as `(ieee, address)`
    #[must_use]
    pub fn children(&self) -> Vec<(Eui64, u16)> {
        ziggurat_protocol::child_entries(&self.stack)
            .into_iter()
            .map(|child| (child.ieee, child.nwk.0))
            .collect()
    }

    /// Open this router to new children for `seconds`. The trust center has the last
    /// word on each of them.
    pub fn permit_joins(&self, seconds: u64) {
        self.stack.permit_joins(seconds, true);
    }

    #[must_use]
    pub fn state(&self) -> RouterState {
        self.state.lock().expect("state lock").clone()
    }

    fn on_notification(&self, notification: ZigbeeNotification) {
        match notification {
            ZigbeeNotification::FrameCounterUpdate { frame_counter } => {
                self.state.lock().expect("state lock").nwk_frame_counter = frame_counter;
            }
            ZigbeeNotification::ReceivedApsCommand {
                source,
                destination,
                profile_id,
                cluster_id,
                src_ep,
                dst_ep,
                lqi,
                rssi,
                data,
                ..
            } => {
                if profile_id == zdo::PROFILE_ZDO {
                    if let Some(seconds) = zdo::permit_joining(cluster_id, &data) {
                        self.permit_joins(u64::from(seconds));
                    }
                    self.answer_zdo(source, destination, cluster_id, &data);
                } else if profile_id == zdo::PROFILE_HA
                    && cluster_id == zdo::CLUSTER_BASIC
                    && dst_ep == zdo::ENDPOINT
                {
                    self.answer_basic(source, src_ep, &data);
                }
                // No listener is not an error: nobody is on the control port
                let _ = self.received.send(Received {
                    source: source.0,
                    profile: profile_id,
                    cluster: cluster_id,
                    src_ep,
                    dst_ep,
                    lqi,
                    rssi,
                    data,
                });
            }
            _ => {}
        }
    }

    fn answer_zdo(&self, source: Nwk, destination: Nwk, cluster: u16, data: &[u8]) {
        if let Some(response) = zdo::answer(
            source.0,
            destination.0,
            cluster,
            data,
            self.ieee,
            self.address,
            &self.children(),
        ) {
            tracing::debug!("ZDO {cluster:04X} from {:04X}", source.0);
            if let Err(e) = self.send(
                response.to,
                zdo::PROFILE_ZDO,
                response.cluster,
                0,
                0,
                response.asdu,
            ) {
                tracing::warn!("ZDO response rejected: {e:#}");
            }
        }
    }

    fn answer_basic(&self, source: Nwk, src_ep: u8, data: &[u8]) {
        if let Some(response) = zdo::read_basic(data)
            && let Err(e) = self.send(
                source.0,
                zdo::PROFILE_HA,
                zdo::CLUSTER_BASIC,
                zdo::ENDPOINT,
                src_ep,
                response,
            )
        {
            tracing::warn!("Basic response rejected: {e:#}");
        }
    }

    /// Send application data to a device, or broadcast it for a broadcast address
    pub fn send(
        &self,
        destination: u16,
        profile: u16,
        cluster: u16,
        src_ep: u8,
        dst_ep: u8,
        data: Vec<u8>,
    ) -> Result<()> {
        tracing::debug!("Sending to {destination:04X}: {}", to_hex(&data));
        if destination >= BROADCAST_MIN {
            self.stack
                .send_aps_broadcast(
                    Nwk(destination),
                    profile,
                    cluster,
                    src_ep,
                    dst_ep,
                    RADIUS,
                    data,
                    TxPriority::UserNormal,
                )
                .map_err(|e| anyhow!("broadcast rejected: {e:?}"))?;
            return Ok(());
        }

        self.stack
            .send_aps_unicast(
                Nwk(destination),
                profile,
                cluster,
                src_ep,
                dst_ep,
                ApsAck::Request,
                RADIUS,
                data,
                None,
                false,
                TxPriority::UserNormal,
                RouteDirective::StackDecides,
            )
            .map_err(|e| anyhow!("unicast rejected: {e:?}"))?;
        Ok(())
    }
}
