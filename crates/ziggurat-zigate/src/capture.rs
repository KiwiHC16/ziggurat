//! A tap between the stack and a real radio, for Wireshark.
//!
//! The radio is the coordinator, so it cannot sniff the whole channel; the tap sees
//! what the coordinator sends and receives, which covers joins, key transport and
//! every exchange with Abeille's devices. MAC acks are handled inside the RCP and are
//! not seen.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, SystemTime};

use tokio::sync::broadcast;
use ziggurat_ieee_802154::types::{Eui64, Nwk};
use ziggurat_phy::{
    ExclusiveRadio, RadioConfig, RadioError, RadioPhy, Receiver, RxFrame, TxFrame, TxResult,
};
use ziggurat_sim::medium::AirFrame;

/// RSSI and LQI recorded for our own transmissions
const TX_RSSI: i8 = 0;
const TX_LQI: u8 = 255;

pub struct TapPhy<P: RadioPhy> {
    inner: Arc<P>,
    tap: broadcast::Sender<AirFrame>,
    channel: Arc<AtomicU8>,
}

impl<P: RadioPhy> TapPhy<P> {
    #[must_use]
    pub fn new(inner: Arc<P>, tap: broadcast::Sender<AirFrame>) -> Self {
        Self {
            inner,
            tap,
            channel: Arc::new(AtomicU8::new(11)),
        }
    }

    fn record_tx(&self, frame: &TxFrame) {
        let channel = frame
            .channel
            .unwrap_or_else(|| self.channel.load(Ordering::Relaxed));
        // No capture client connected is the normal case
        let _ = self.tap.send(AirFrame {
            psdu: frame.psdu.clone(),
            channel,
            rssi: TX_RSSI,
            lqi: TX_LQI,
            time: SystemTime::now(),
        });
    }
}

pub struct TapRx<R> {
    inner: R,
    tap: broadcast::Sender<AirFrame>,
}

impl<R: Receiver<RxFrame>> Receiver<RxFrame> for TapRx<R> {
    async fn recv(&mut self) -> Option<RxFrame> {
        let frame = self.inner.recv().await?;
        let _ = self.tap.send(AirFrame {
            psdu: frame.psdu.clone(),
            channel: frame.channel,
            rssi: frame.rssi,
            lqi: frame.lqi,
            time: SystemTime::now(),
        });
        Some(frame)
    }
}

pub struct TapExclusive<'a, P: RadioPhy + 'a> {
    inner: P::Exclusive<'a>,
    phy: &'a TapPhy<P>,
}

// The inner radio's futures are returned as is: they are `Send` by the trait's contract,
// whereas an `async fn` here would hold `&self` across the await
impl<'a, P: RadioPhy> ExclusiveRadio for TapExclusive<'a, P> {
    fn set_channel(&self, channel: u8) -> impl Future<Output = Result<(), RadioError>> + Send {
        self.phy.channel.store(channel, Ordering::Relaxed);
        self.inner.set_channel(channel)
    }

    fn transmit(
        &self,
        frame: TxFrame,
    ) -> impl Future<Output = Result<TxResult, RadioError>> + Send {
        self.phy.record_tx(&frame);
        self.inner.transmit(frame)
    }

    fn set_promiscuous(
        &self,
        promiscuous: bool,
    ) -> impl Future<Output = Result<(), RadioError>> + Send {
        self.inner.set_promiscuous(promiscuous)
    }
}

impl<P: RadioPhy> RadioPhy for TapPhy<P> {
    type Exclusive<'a>
        = TapExclusive<'a, P>
    where
        Self: 'a;
    type RxStream = TapRx<P::RxStream>;
    type ResetStream = P::ResetStream;

    async fn reset(&self) -> Result<(), RadioError> {
        self.inner.reset().await
    }

    async fn reconfigure(&self, config: &RadioConfig) -> Result<(), RadioError> {
        self.channel.store(config.channel, Ordering::Relaxed);
        self.inner.reconfigure(config).await
    }

    async fn set_frame_pending_table(
        &self,
        short: &[Nwk],
        extended: &[Eui64],
    ) -> Result<(), RadioError> {
        self.inner.set_frame_pending_table(short, extended).await
    }

    async fn transmit(&self, frame: TxFrame) -> Result<TxResult, RadioError> {
        self.record_tx(&frame);
        self.inner.transmit(frame).await
    }

    async fn energy_detect(&self, channel: u8, duration: Duration) -> Result<i8, RadioError> {
        self.inner.energy_detect(channel, duration).await
    }

    async fn lock(&self) -> TapExclusive<'_, P> {
        TapExclusive {
            inner: self.inner.lock().await,
            phy: self,
        }
    }

    fn subscribe_rx(&self) -> TapRx<P::RxStream> {
        TapRx {
            inner: self.inner.subscribe_rx(),
            tap: self.tap.clone(),
        }
    }

    fn subscribe_reset(&self) -> P::ResetStream {
        self.inner.subscribe_reset()
    }
}
