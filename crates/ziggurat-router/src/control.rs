//! The control port, one command or event per text line, values in hex:
//!
//! - `send <nwk> <profile> <cluster> <src_ep> <dst_ep> <payload>` answers `ok` or `err <why>`
//! - `permit <seconds>`, in decimal, opens this router to children; 0 closes it
//! - `children` answers `children <ieee>:<nwk> ...`
//! - `status` answers `status <pan> <nwk> <channel> <extended_pan>`
//! - `rx <source> <profile> <cluster> <src_ep> <dst_ep> <lqi> <rssi> <payload>`
//!   is emitted for every frame received; `rssi` is signed decimal dBm
//!
//! It has no authentication: keep it on the loopback.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use ziggurat_phy::RadioPhy;

use crate::router::{Received, Router};
use crate::state::{from_hex, to_hex};

/// One command line's effect, as the line to send back
pub fn execute<P: RadioPhy>(router: &Router<P>, line: &str) -> String {
    match run_command(router, line) {
        Ok(reply) => reply,
        Err(e) => format!("err {e:#}"),
    }
}

fn run_command<P: RadioPhy>(router: &Router<P>, line: &str) -> Result<String> {
    let mut words = line.split_whitespace();
    match words.next() {
        Some("status") => {
            let state = router.state();
            Ok(format!(
                "status {:04x} {:04x} {} {}",
                state.pan_id, state.network_address, state.channel, state.extended_pan_id
            ))
        }
        Some("children") => {
            let children: Vec<String> = router
                .children()
                .iter()
                .map(|(ieee, nwk)| format!("{}:{nwk:04x}", to_hex(&ieee.0)))
                .collect();
            Ok(format!("children {}", children.join(" "))
                .trim_end()
                .to_string())
        }
        Some("permit") => {
            let seconds = words
                .next()
                .ok_or_else(|| anyhow!("missing seconds"))?
                .parse()
                .context("invalid seconds")?;
            router.permit_joins(seconds);
            Ok("ok".to_string())
        }
        Some("send") => {
            let mut hex16 = |name: &str| -> Result<u16> {
                let word = words.next().ok_or_else(|| anyhow!("missing {name}"))?;
                u16::from_str_radix(word, 16).with_context(|| format!("invalid {name}"))
            };
            let destination = hex16("nwk")?;
            let profile = hex16("profile")?;
            let cluster = hex16("cluster")?;
            let src_ep = u8::try_from(hex16("src_ep")?).context("invalid src_ep")?;
            let dst_ep = u8::try_from(hex16("dst_ep")?).context("invalid dst_ep")?;
            let payload = from_hex(words.next().ok_or_else(|| anyhow!("missing payload"))?)?;
            router.send(destination, profile, cluster, src_ep, dst_ep, payload)?;
            Ok("ok".to_string())
        }
        other => Err(anyhow!("unknown command {other:?}")),
    }
}

#[must_use]
pub fn event_line(frame: &Received) -> String {
    format!(
        "rx {:04x} {:04x} {:04x} {:02x} {:02x} {:02x} {} {}",
        frame.source,
        frame.profile,
        frame.cluster,
        frame.src_ep,
        frame.dst_ep,
        frame.lqi,
        frame.rssi,
        to_hex(&frame.data)
    )
}

pub async fn serve<P: RadioPhy>(router: Arc<Router<P>>, listen: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!("Control port on {listen}");
    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::info!("Control client {peer}");
        let router = Arc::clone(&router);
        tokio::spawn(async move {
            if let Err(e) = client(router, stream).await {
                tracing::debug!("Control client {peer}: {e:#}");
            }
        });
    }
}

async fn client<P: RadioPhy>(router: Arc<Router<P>>, stream: TcpStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut events = router.received.subscribe();

    loop {
        let out = tokio::select! {
            line = lines.next_line() => match line? {
                Some(line) => execute(router.as_ref(), &line),
                None => return Ok(()),
            },
            event = events.recv() => match event {
                Ok(frame) => event_line(&frame),
                // A slow client misses frames rather than stalling the router
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
        };
        writer.write_all(out.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }
}
