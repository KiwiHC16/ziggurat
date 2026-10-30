//! The router's persistent state: everything needed to come back on the network
//! without joining again. Holds the network key, hence the 0600 file.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use ziggurat_driver::join::{JoinParams, JoinedNetwork};
use ziggurat_driver::zigbee_stack::{NetworkConfig, NwkDeviceType};
use ziggurat_ieee_802154::types::{Eui64, Key, Nwk, PanId};

/// Frames that may have been sent since the last save. Peers reject a counter that
/// goes backwards, so a restart skips past them.
pub const FRAME_COUNTER_MARGIN: u32 = 1024;
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterState {
    pub version: u32,
    pub ieee: String,
    pub channel: u8,
    pub update_id: u8,
    pub pan_id: u16,
    pub extended_pan_id: String,
    pub network_address: u16,
    pub parent: u16,
    pub trust_center: String,
    pub network_key: String,
    pub network_key_seq_number: u8,
    pub tc_link_key: String,
    pub tx_power: i8,
    pub nwk_frame_counter: u32,
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(anyhow!("odd hex length"));
    }
    (0..text.len())
        .step_by(2)
        .map(|at| {
            u8::from_str_radix(
                text.get(at..at + 2).ok_or_else(|| anyhow!("invalid hex"))?,
                16,
            )
            .context("invalid hex")
        })
        .collect()
}

fn array<const N: usize>(text: &str) -> Result<[u8; N]> {
    from_hex(text)?
        .try_into()
        .map_err(|_| anyhow!("expected {N} hex bytes in {text}"))
}

impl RouterState {
    #[must_use]
    pub fn new(joined: &JoinedNetwork, params: &JoinParams) -> Self {
        Self {
            version: VERSION,
            ieee: to_hex(&params.ieee.0),
            channel: joined.channel,
            update_id: joined.update_id,
            pan_id: joined.pan_id.0,
            extended_pan_id: to_hex(&joined.extended_pan_id.0),
            network_address: joined.network_address.0,
            parent: joined.parent.0,
            trust_center: to_hex(&joined.trust_center.0),
            network_key: to_hex(&joined.network_key.0),
            network_key_seq_number: joined.network_key_seq_number,
            tc_link_key: to_hex(&params.tc_link_key.0),
            tx_power: params.tx_power,
            nwk_frame_counter: 0,
        }
    }

    pub fn ieee(&self) -> Result<Eui64> {
        Ok(Eui64(array(&self.ieee)?))
    }

    /// The stack configuration, with the frame counter moved past anything sent
    pub fn network_config(&self) -> Result<NetworkConfig> {
        Ok(NetworkConfig {
            role: NwkDeviceType::Router,
            channel: self.channel,
            update_id: self.update_id,
            pan_id: PanId(self.pan_id),
            extended_pan_id: Eui64(array(&self.extended_pan_id)?),
            network_address: Nwk(self.network_address),
            ieee_address: self.ieee()?,
            network_key: Key(array(&self.network_key)?),
            network_key_seq_number: self.network_key_seq_number,
            network_key_tx_counter: self.nwk_frame_counter.saturating_add(FRAME_COUNTER_MARGIN),
            tc_link_key: Key(array(&self.tc_link_key)?),
            tclk_seed: None,
            tx_power: self.tx_power,
            source_routing: false,
        })
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let state: Self = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if state.version != VERSION {
                    return Err(anyhow!("unknown state version {}", state.version));
                }
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let tmp = path.with_extension("json.tmp");
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            // `mode` only applies to new files: fix a leftover temporary file too
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }
}
