//! ZiGate v1 emulator for the Jeedom Abeille plugin, backed by the Ziggurat stack.
//!
//! Abeille connects to it like to a ZiGate WiFi: `ab::gtwSubType = WIFI`,
//! `ab::gtwIpAddr = <host>:20108`. See README.md for the contract and the plan.

pub mod bridge;
pub mod capture;
pub mod frame;
#[allow(dead_code)] // Not every ZiGate message type is wired in yet
pub mod msg;
pub mod native;
pub mod server;
pub mod state;
pub mod zdo;

/// Upper-case hex, as Abeille logs frames
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02X}");
        s
    })
}
