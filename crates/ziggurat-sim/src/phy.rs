//! [`RadioPhy`] over the simulated [`Medium`], for running a stack in-process.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, mpsc};
use ziggurat_ieee_802154::types::{Eui64, Nwk};
use ziggurat_phy::{
    ExclusiveRadio, RadioConfig, RadioError, RadioPhy, Receiver, ResetEvent, RxFrame, TxFrame,
    TxResult,
};

use crate::medium::{Medium, NodeId};

/// Time for the simulated radio to come back from a reset
const RESET_TIME: Duration = Duration::from_millis(5);

pub struct SimPhy {
    medium: Arc<Medium>,
    node: NodeId,
    exclusive: AsyncMutex<()>,
    reset_slot: Arc<Mutex<Option<mpsc::Sender<ResetEvent>>>>,
}

pub struct SimRx<T>(mpsc::Receiver<T>);

impl<T: Send> Receiver<T> for SimRx<T> {
    async fn recv(&mut self) -> Option<T> {
        self.0.recv().await
    }
}

impl SimPhy {
    #[must_use]
    pub fn new(medium: Arc<Medium>, node: NodeId) -> Self {
        Self {
            medium,
            node,
            exclusive: AsyncMutex::new(()),
            reset_slot: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn hw_address(&self) -> Eui64 {
        self.medium.hw_address(self.node)
    }

    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }
}

pub struct SimExclusive<'a> {
    phy: &'a SimPhy,
    _guard: MutexGuard<'a, ()>,
}

impl ExclusiveRadio for SimExclusive<'_> {
    async fn set_channel(&self, channel: u8) -> Result<(), RadioError> {
        self.phy.medium.set_channel(self.phy.node, channel);
        Ok(())
    }

    async fn transmit(&self, frame: TxFrame) -> Result<TxResult, RadioError> {
        Ok(self.phy.medium.transmit(self.phy.node, &frame).await)
    }

    async fn set_promiscuous(&self, promiscuous: bool) -> Result<(), RadioError> {
        self.phy.medium.set_promiscuous(self.phy.node, promiscuous);
        Ok(())
    }
}

impl RadioPhy for SimPhy {
    type Exclusive<'a> = SimExclusive<'a>;
    type RxStream = SimRx<RxFrame>;
    type ResetStream = SimRx<ResetEvent>;

    async fn reset(&self) -> Result<(), RadioError> {
        self.medium.configure(self.node, None);
        // Like an RCP, the reset is acknowledged asynchronously by a notification
        let slot = Arc::clone(&self.reset_slot);
        tokio::spawn(async move {
            tokio::time::sleep(RESET_TIME).await;
            let tx = slot.lock().clone();
            if let Some(tx) = tx {
                let _ = tx
                    .send(ResetEvent {
                        reason: "ResetSoftware".into(),
                    })
                    .await;
            }
        });
        Ok(())
    }

    async fn reconfigure(&self, config: &RadioConfig) -> Result<(), RadioError> {
        let _radio = self.exclusive.lock().await;
        self.medium.configure(self.node, Some(config.clone()));
        Ok(())
    }

    async fn set_frame_pending_table(
        &self,
        short: &[Nwk],
        extended: &[Eui64],
    ) -> Result<(), RadioError> {
        self.medium
            .set_frame_pending_table(self.node, short, extended);
        Ok(())
    }

    async fn transmit(&self, frame: TxFrame) -> Result<TxResult, RadioError> {
        let _radio = self.exclusive.lock().await;
        Ok(self.medium.transmit(self.node, &frame).await)
    }

    async fn energy_detect(&self, channel: u8, duration: Duration) -> Result<i8, RadioError> {
        let _radio = self.exclusive.lock().await;
        tokio::time::sleep(duration).await;
        Ok(self.medium.noise(channel))
    }

    async fn lock(&self) -> SimExclusive<'_> {
        SimExclusive {
            phy: self,
            _guard: self.exclusive.lock().await,
        }
    }

    fn subscribe_rx(&self) -> SimRx<RxFrame> {
        let (tx, rx) = mpsc::channel(64);
        self.medium.set_receiver(self.node, tx);
        SimRx(rx)
    }

    fn subscribe_reset(&self) -> SimRx<ResetEvent> {
        let (tx, rx) = mpsc::channel(8);
        *self.reset_slot.lock() = Some(tx);
        SimRx(rx)
    }
}
