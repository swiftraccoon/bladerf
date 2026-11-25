//! Reusable offline channel preparation for bounded RF analysis.
//!
//! The library exposes the IEEE-channel and arbitrary-frequency paths used by
//! the `ieee802154-monitor` command-line program. It processes files only: it does
//! not open a radio, network socket or transmit path. Channelized samples are
//! evidence for later analysis, not proof of protocol activity, transmitter
//! identity or complete reception.

#![forbid(unsafe_code)]

pub mod channelize;
