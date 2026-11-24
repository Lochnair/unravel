// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InterfaceStats {
    pub(crate) rx_bytes: u64,
    pub(crate) tx_bytes: u64,
}

#[cfg_attr(target_os = "linux", path = "linux.rs")]
#[cfg_attr(target_os = "freebsd", path = "freebsd.rs")]
#[cfg_attr(target_os = "macos", path = "macos.rs")]
mod imp;

pub(crate) use imp::{Platform, warn_platform_limitations};

#[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
compile_error!("unRAVEL supports only Linux, FreeBSD, and macOS");
