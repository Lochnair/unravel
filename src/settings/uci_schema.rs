// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UciOptionMapping {
    pub(crate) uci_option: &'static str,
    pub(crate) config_field: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UciSectionMapping {
    /// UCI section type, corresponding to `@section_type[0]`.
    pub(crate) uci_section: &'static str,

    /// Top-level config-rs / Serde settings key.
    pub(crate) config_section: &'static str,

    /// A section of this type is an instance. It has an option named after
    /// each other section type, naming the section of that type it uses.
    pub(crate) anchor: bool,

    pub(crate) options: &'static [UciOptionMapping],

    /// Options in this section that something other than unRAVEL reads, such
    /// as the init script. They are skipped without a warning.
    pub(crate) external: &'static [&'static str],

    /// Config field whose value names a sub-table. When set, every option the
    /// schema does not know is passed through to
    /// `<config_section>.<value>.<option>`.
    pub(crate) passthrough_key: Option<&'static str>,
}

#[cfg(all(feature = "uci", unix))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct UciRepeatedSectionMapping {
    /// UCI section type, for both named and anonymous sections.
    pub(crate) uci_section: &'static str,

    /// Config-rs / Serde path receiving an array of section tables.
    pub(crate) config_path: &'static str,

    pub(crate) options: &'static [UciOptionMapping],
}

pub(crate) trait UciSectionSchema {
    const UCI_OPTIONS: &'static [UciOptionMapping];
    const UCI_EXTERNAL_OPTIONS: &'static [&'static str];
    const UCI_PASSTHROUGH_KEY: Option<&'static str>;
}

pub(crate) trait UciConfigSchema {
    const UCI_PACKAGE: &'static str;
    const UCI_SECTIONS: &'static [UciSectionMapping];
}
