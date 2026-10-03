//! Hardware-free testing for the Ziggurat stack.
//!
//! - [`medium::Medium`]: a simulated 802.15.4 air with per-direction link quality
//! - [`phy::SimPhy`]: a [`ziggurat_phy::RadioPhy`] on that air, to run stacks in-process
//! - [`rcp::serve`]: a simulated OpenThread RCP speaking Spinel, a drop-in for the real
//!   dongle behind `--device tcp://…`
//! - [`pcap::serve`]: live capture of the air for Wireshark (`-i TCP@host:port`)
//! - [`device::SimDevice`]: simulated devices (a router stack plus the device's ZDO/ZCL
//!   behavior) to exercise a client end to end

pub mod device;
pub mod medium;
pub mod pcap;
pub mod phy;
pub mod rcp;
