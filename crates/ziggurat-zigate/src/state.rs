//! Persistent network state: what a ZiGate keeps in its PDM.
//!
//! Ziggurat keeps nothing across restarts, so the bridge saves the network parameters,
//! the frame counters, the unique link keys, the children and the address map to a
//! JSON file, written atomically.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use ziggurat_driver::zigbee_stack::{
    NetworkConfig, NwkDeviceType, WELL_KNOWN_LINK_KEY, ZigbeeStack,
};
use ziggurat_ieee_802154::types::{Eui64, Key, Nwk, PanId};
use ziggurat_phy::RadioPhy;
use ziggurat_protocol::{
    AddressEntry, ChildDeviceType, ChildEntry, ChildFlags, KeyEntry, LoadAddressCachePayload,
    LoadChildrenPayload, LoadKeyTablePayload,
};

pub const FORMAT_VERSION: u32 = 1;

/// Frame counters are saved every 100 frames by the stack's notifications; on restore
/// they jump ahead by this much so a counter is never reused after a crash.
pub const FRAME_COUNTER_MARGIN: u32 = 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkState {
    pub version: u32,
    pub channel: u8,
    pub update_id: u8,
    /// Hex, as printed (big-endian)
    pub pan_id: String,
    pub extended_pan_id: String,
    pub coordinator_ieee: String,
    pub network_key: String,
    pub network_key_seq: u8,
    pub nwk_frame_counter: u32,
    pub aps_frame_counter: u32,
    pub tx_power: i8,
    #[serde(default)]
    pub devices: Vec<Device>,
    #[serde(default)]
    pub children: Vec<Child>,
    #[serde(default)]
    pub link_keys: Vec<LinkKey>,
    /// IEEE of devices whose receiver is off when idle (Device_annce capability), so
    /// that sends to them get the stack's longer APS ack timeout
    #[serde(default)]
    pub sleepy: Vec<String>,
}

/// An IEEE ↔ NWK address mapping
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    pub ieee: String,
    pub nwk: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Child {
    pub ieee: String,
    pub nwk: String,
    pub rx_on_when_idle: bool,
    pub router: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkKey {
    pub ieee: String,
    pub key: String,
    pub rx_counter: u32,
}

/// EUI64 in the printed (big-endian) form Abeille and Jeedom display
#[must_use]
pub fn eui_to_hex(eui: Eui64) -> String {
    let mut bytes = eui.to_bytes();
    bytes.reverse();
    hex(&bytes)
}

pub fn eui_from_hex(text: &str) -> Result<Eui64> {
    let mut bytes: [u8; 8] = unhex(text)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("EUI64 must be 8 bytes: {text}"))?;
    bytes.reverse();
    Ok(Eui64(bytes))
}

/// EUI64 as the big-endian `u64` the ZiGate protocol carries
#[must_use]
pub const fn eui_to_u64(eui: Eui64) -> u64 {
    u64::from_le_bytes(eui.to_bytes())
}

#[must_use]
pub const fn eui_from_u64(value: u64) -> Eui64 {
    Eui64(value.to_le_bytes())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02X}");
        s
    })
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(text.len().is_multiple_of(2), "odd-length hex: {text}");
    (0..text.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&text[i..i + 2], 16).with_context(|| format!("bad hex: {text}"))
        })
        .collect()
}

fn nwk_from_hex(text: &str) -> Result<Nwk> {
    u16::from_str_radix(text, 16)
        .map(Nwk)
        .with_context(|| format!("bad NWK address: {text}"))
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    use std::io::Read;
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

impl NetworkState {
    /// A new network: random PAN ID, extended PAN ID and network key
    pub fn generate(coordinator: Eui64, channel: u8, tx_power: i8) -> Result<Self> {
        // PAN IDs 0x0000 and 0xFFFx are reserved or special
        let pan = u16::from_le_bytes(random_bytes()?) % 0xFFF0 + 1;
        Ok(Self {
            version: FORMAT_VERSION,
            channel,
            update_id: 0,
            pan_id: format!("{pan:04X}"),
            extended_pan_id: hex(&random_bytes::<8>()?),
            coordinator_ieee: eui_to_hex(coordinator),
            network_key: hex(&random_bytes::<16>()?),
            network_key_seq: 0,
            nwk_frame_counter: 0,
            aps_frame_counter: 0,
            tx_power,
            devices: Vec::new(),
            children: Vec::new(),
            link_keys: Vec::new(),
            sleepy: Vec::new(),
        })
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let state: Self = serde_json::from_slice(&bytes)
                    .with_context(|| format!("reading {}", path.display()))?;
                anyhow::ensure!(
                    state.version == FORMAT_VERSION,
                    "unsupported state version {}",
                    state.version
                );
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write to a temporary file, flush it to disk, then rename over the old state.
    /// The file holds the network key: it is readable by its owner only.
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
        std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
    }

    pub fn coordinator(&self) -> Result<Eui64> {
        eui_from_hex(&self.coordinator_ieee)
    }

    pub fn pan(&self) -> Result<PanId> {
        u16::from_str_radix(&self.pan_id, 16)
            .map(PanId)
            .context("bad PAN ID")
    }

    pub fn extended_pan(&self) -> Result<Eui64> {
        eui_from_hex(&self.extended_pan_id)
    }

    /// The stack configuration, with the frame counters moved past anything that may
    /// have been sent since the last save
    pub fn network_config(&self) -> Result<NetworkConfig> {
        let key: [u8; 16] = unhex(&self.network_key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("network key must be 16 bytes"))?;
        Ok(NetworkConfig {
            role: NwkDeviceType::Coordinator,
            channel: self.channel,
            update_id: self.update_id,
            pan_id: self.pan()?,
            extended_pan_id: self.extended_pan()?,
            network_address: Nwk(0x0000),
            ieee_address: self.coordinator()?,
            network_key: Key(key),
            network_key_seq_number: self.network_key_seq,
            network_key_tx_counter: self.nwk_frame_counter.saturating_add(FRAME_COUNTER_MARGIN),
            tc_link_key: WELL_KNOWN_LINK_KEY,
            tclk_seed: None,
            tx_power: self.tx_power,
            source_routing: false,
        })
    }

    /// Load keys, children, address map and the APS counter into a fresh stack,
    /// before `start_network`
    pub fn restore_into<P: RadioPhy>(&self, stack: &ZigbeeStack<P>) -> Result<()> {
        let keys = self
            .link_keys
            .iter()
            .map(|k| {
                let key: [u8; 16] = unhex(&k.key)?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("link key must be 16 bytes"))?;
                Ok(KeyEntry {
                    key: Key(key),
                    tx_counter: 0,
                    rx_counter: k.rx_counter,
                    seq: 0,
                    partner_ieee: eui_from_hex(&k.ieee)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let children = self
            .children
            .iter()
            .map(|c| {
                Ok(ChildEntry {
                    ieee: eui_from_hex(&c.ieee)?,
                    nwk: nwk_from_hex(&c.nwk)?,
                    flags: ChildFlags {
                        rx_on_when_idle: c.rx_on_when_idle,
                        device_type: if c.router {
                            ChildDeviceType::Router
                        } else {
                            ChildDeviceType::EndDevice
                        },
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let addresses = self
            .devices
            .iter()
            .map(|d| {
                Ok(AddressEntry {
                    ieee: eui_from_hex(&d.ieee)?,
                    nwk: nwk_from_hex(&d.nwk)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        ziggurat_protocol::apply_key_table(stack, LoadKeyTablePayload { entries: keys });
        ziggurat_protocol::apply_children(stack, LoadChildrenPayload { entries: children });
        ziggurat_protocol::apply_address_cache(
            stack,
            LoadAddressCachePayload { entries: addresses },
        );
        stack
            .state
            .core
            .lock()
            .aib
            .aps_security
            .restore_outgoing_frame_counter(
                self.aps_frame_counter.saturating_add(FRAME_COUNTER_MARGIN),
            );
        Ok(())
    }

    /// Refresh keys, children and address map from the running stack
    pub fn snapshot_from<P: RadioPhy>(&mut self, stack: &ZigbeeStack<P>) {
        self.link_keys = ziggurat_protocol::key_entries(stack)
            .into_iter()
            .map(|k| LinkKey {
                ieee: eui_to_hex(k.partner_ieee),
                key: hex(&k.key.to_bytes()),
                rx_counter: k.rx_counter,
            })
            .collect();
        self.children = ziggurat_protocol::child_entries(stack)
            .into_iter()
            .map(|c| Child {
                ieee: eui_to_hex(c.ieee),
                nwk: format!("{:04X}", c.nwk.0),
                rx_on_when_idle: c.flags.rx_on_when_idle,
                router: matches!(c.flags.device_type, ChildDeviceType::Router),
            })
            .collect();
        self.devices = ziggurat_protocol::address_entries(stack)
            .into_iter()
            .map(|a| Device {
                ieee: eui_to_hex(a.ieee),
                nwk: format!("{:04X}", a.nwk.0),
            })
            .collect();
        self.devices.sort_by(|a, b| a.ieee.cmp(&b.ieee));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eui_hex_is_big_endian_as_printed() {
        let eui = Eui64([0x81, 0x72, 0xCA, 0x3F, 0x8F, 0x38, 0xC1, 0xA4]);
        assert_eq!(eui_to_hex(eui), "A4C1388F3FCA7281");
        assert_eq!(eui_from_hex("A4C1388F3FCA7281").unwrap(), eui);
        assert_eq!(eui_to_u64(eui), 0xA4C1_388F_3FCA_7281);
        assert_eq!(eui_from_u64(0xA4C1_388F_3FCA_7281), eui);
    }

    #[test]
    fn generated_state_round_trips_and_advances_counters() {
        let dir = std::env::temp_dir().join(format!("zigate-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let mut state = NetworkState::generate(Eui64([1; 8]), 25, 8).unwrap();
        state.nwk_frame_counter = 5000;
        state.save(&path).unwrap();
        let loaded = NetworkState::load(&path).unwrap().unwrap();
        assert_eq!(loaded, state);
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "network key must not be world-readable"
            );
        }

        let config = loaded.network_config().unwrap();
        assert_eq!(config.channel, 25);
        assert_eq!(config.network_key_tx_counter, 5000 + FRAME_COUNTER_MARGIN);
        assert_ne!(config.pan_id.0, 0);
        assert!(config.pan_id.0 < 0xFFF1);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_state_is_none() {
        assert!(
            NetworkState::load(Path::new("/nonexistent/zigate.json"))
                .unwrap()
                .is_none()
        );
    }
}
