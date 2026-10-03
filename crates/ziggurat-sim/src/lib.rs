//! Hardware-free testing for the Ziggurat stack.
//!
//! - [`medium::Medium`]: a simulated 802.15.4 air with per-direction link quality
//! - [`phy::SimPhy`]: a [`ziggurat_phy::RadioPhy`] on that air, to run stacks in-process
//! - [`rcp::serve`]: a simulated OpenThread RCP speaking Spinel, a drop-in for the real
//!   dongle behind `--device tcp://…`

pub mod medium;
pub mod phy;
pub mod rcp;
