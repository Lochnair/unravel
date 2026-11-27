// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::env;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    match target_os.as_str() {
        "linux" => {}
        "macos" => println!(
            "cargo::warning=traffic control is unavailable on macOS; requested rate changes will be calculated but not applied"
        ),
        "freebsd" => println!(
            "cargo::warning=traffic control is unavailable on FreeBSD; requested rate changes will be calculated but not applied"
        ),
        _ => {}
    }

    println!("cargo::rerun-if-changed=build.rs");
}
