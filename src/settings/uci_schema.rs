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

    /// This section declares a named instance. Other sections carry an owner.
    pub(crate) instance: bool,

    pub(crate) options: &'static [UciOptionMapping],

    /// Options in this section that something other than unRAVEL reads, such
    /// as the init script. They are skipped without a warning.
    pub(crate) external: &'static [&'static str],

    /// Fields of this section collecting every section of another type.
    pub(crate) repeated: &'static [UciRepeatedSectionMapping],

    /// Config field whose value names a sub-table. When set, every option the
    /// schema does not know is passed through to
    /// `<config_section>.<value>.<option>`.
    pub(crate) passthrough_key: Option<&'static str>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UciRepeatedSectionMapping {
    /// UCI section type, for both named and anonymous sections.
    pub(crate) uci_section: &'static str,

    /// Field of the parent section receiving an array of section tables.
    pub(crate) field: &'static str,

    pub(crate) options: &'static [UciOptionMapping],
}

pub(crate) trait UciSectionSchema {
    const UCI_OPTIONS: &'static [UciOptionMapping];
    const UCI_EXTERNAL_OPTIONS: &'static [&'static str];
    const UCI_PASSTHROUGH_KEY: Option<&'static str>;
    const UCI_REPEATED: &'static [UciRepeatedSectionMapping];
}

pub(crate) trait UciConfigSchema {
    const UCI_INSTANCE_OPTIONS: &'static [UciOptionMapping];
    const UCI_PACKAGE: &'static str;
    const UCI_SECTIONS: &'static [UciSectionMapping];
}
