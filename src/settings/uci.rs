// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use config::{ConfigError, Map, Source, Value, ValueKind};
use ucifer::document::Section;
use ucifer::syntax::DirectiveIter;
use ucifer::{ConfigOption, Directive, Document};

use super::uci_schema::{UciConfigSchema, UciOptionMapping, UciSectionMapping};
use super::valid_instance_name;

const OWNER_OPTION: &str = "instance";

#[derive(Clone, Debug)]
pub(crate) struct UciSource {
    package: &'static str,
    instance: String,
    sections: &'static [UciSectionMapping],
    instance_options: &'static [UciOptionMapping],
    path: PathBuf,
}

impl UciSource {
    pub(crate) fn from_schema<T: UciConfigSchema>(instance: &str, config_dir: &Path) -> Self {
        Self {
            package: T::UCI_PACKAGE,
            instance: instance.to_owned(),
            sections: T::UCI_SECTIONS,
            instance_options: T::UCI_INSTANCE_OPTIONS,
            path: config_dir.join(T::UCI_PACKAGE),
        }
    }

    /// Value of the option that selects a passthrough section's sub-table.
    fn passthrough_table(
        mapping: &UciSectionMapping,
        key_field: &str,
        section: &Section<'_>,
        section_key: &str,
    ) -> Result<Option<String>, ConfigError> {
        let key_option = mapping
            .options
            .iter()
            .find(|option| option.config_field == key_field)
            .map_or(key_field, |option| option.uci_option);

        let table = match section.get(key_option) {
            // libuci treats an empty scalar as an unset option.
            None => return Ok(None),
            Some(ConfigOption::Scalar(table)) if table.is_empty() => return Ok(None),
            Some(ConfigOption::Scalar(table)) => table.as_ref(),
            Some(ConfigOption::List(_)) => {
                return Err(ConfigError::Message(format!(
                    "UCI option {section_key}.{key_option} must be a single value, not a list"
                )));
            }
        };

        let valid = !table.is_empty()
            && table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && mapping
                .options
                .iter()
                .all(|option| option.config_field != table);
        if !valid {
            return Err(ConfigError::Message(format!(
                "UCI option {section_key}.{key_option} has unusable value {table:?}: \
                 expected letters, digits and underscores, and not the name of an \
                 option of this section"
            )));
        }

        Ok(Some(table.to_owned()))
    }

    fn collect_package(&self, package: &Document<'_>) -> Result<Map<String, Value>, ConfigError> {
        let instance_type = self
            .sections
            .iter()
            .find(|mapping| mapping.instance)
            .expect("the schema declares an instance section")
            .uci_section;
        let section_type = |key: ucifer::document::SectionKey| {
            package
                .type_name(key.type_key)
                .expect("document section types are interned")
        };
        let section_key = |key: ucifer::document::SectionKey| match package.section_name(key) {
            Some(name) => format!("{}.{name}", self.package),
            None => format!("{}.@{}[{}]", self.package, section_type(key), key.index),
        };

        // Validate the package's instance names and ownership links before
        // reading any instance's options.
        let mut instances = HashSet::new();
        for (key, _) in package.sections_iter() {
            if section_type(*key) == instance_type {
                let Some(name) = package
                    .section_name(*key)
                    .filter(|name| valid_instance_name(name))
                else {
                    return Err(ConfigError::Message(format!(
                        "UCI section {} must have an explicit instance name with letters, digits and underscores",
                        section_key(*key)
                    )));
                };
                instances.insert(name);
            }
        }

        let mut selected = HashMap::<_, Vec<_>>::new();
        for (key, section) in package.sections_iter() {
            let kind = section_type(*key);
            let owner = if kind == instance_type {
                package.section_name(*key).expect("validated instance name")
            } else {
                if !self.sections.iter().any(|mapping| {
                    mapping.uci_section == kind
                        || mapping
                            .repeated
                            .iter()
                            .any(|mapping| mapping.uci_section == kind)
                }) {
                    return Err(ConfigError::Message(format!(
                        "UCI section {} has unrecognised type {kind:?}",
                        section_key(*key)
                    )));
                }

                let owner = match section.get(OWNER_OPTION) {
                    Some(ConfigOption::Scalar(owner)) if !owner.is_empty() => owner.as_ref(),
                    _ => {
                        return Err(ConfigError::Message(format!(
                            "UCI section {} must specify one owner with option {OWNER_OPTION}",
                            section_key(*key)
                        )));
                    }
                };
                if !instances.contains(owner) {
                    return Err(ConfigError::Message(format!(
                        "UCI option {}.{OWNER_OPTION} names nonexistent instance {owner:?}",
                        section_key(*key)
                    )));
                }
                owner
            };

            if owner == self.instance {
                selected
                    .entry(kind)
                    .or_default()
                    .push((section_key(*key), section));
            }
        }
        if !instances.contains(self.instance.as_str()) {
            return Err(ConfigError::Message(format!(
                "UCI package {:?} has no {instance_type} section named {:?}",
                self.package, self.instance
            )));
        }

        let mut values = Map::new();
        for mapping in self.sections {
            if let Some(sections) = selected.get(mapping.uci_section) {
                let (key, section) = &sections[0];
                if let Some((duplicate, _)) = sections.get(1) {
                    return Err(ConfigError::Message(format!(
                        "duplicate UCI {:?} sections for instance {:?}: {key} and {duplicate}",
                        mapping.uci_section, self.instance
                    )));
                }
                self.collect_singleton(mapping, key, section, &mut values)?;
            }

            for repeated in mapping.repeated {
                if let Some(sections) = selected.get(repeated.uci_section) {
                    let mut items = Vec::with_capacity(sections.len());
                    for (key, section) in sections {
                        let mut item = Map::new();
                        collect_options(key, section, &mut item, |name| {
                            if name == OWNER_OPTION {
                                return Ok(None);
                            }
                            match mapped_field(repeated.options, name) {
                                Some(field) => Ok(Some(field.to_owned())),
                                None => Err(unknown_option(key, name)),
                            }
                        })?;
                        let origin = format!("UCI section {key}");
                        items.push(Value::new(Some(&origin), ValueKind::Table(item)));
                    }
                    let path = format!("{}.{}", mapping.config_section, repeated.field);
                    values.insert(path, Value::new(None, ValueKind::Array(items)));
                }
            }
        }

        Ok(values)
    }

    fn collect_singleton(
        &self,
        mapping: &UciSectionMapping,
        key: &str,
        section: &Section<'_>,
        values: &mut Map<String, Value>,
    ) -> Result<(), ConfigError> {
        let table = mapping
            .passthrough_key
            .map(|field| Self::passthrough_table(mapping, field, section, key))
            .transpose()?
            .flatten();

        collect_options(key, section, values, |name| {
            if mapping.instance {
                if let Some(field) = mapped_field(self.instance_options, name) {
                    return Ok(Some(field.to_owned()));
                }
            } else if name == OWNER_OPTION {
                return Ok(None);
            }

            let group = mapping.config_section;
            if let Some(field) = mapped_field(mapping.options, name) {
                Ok(Some(format!("{group}.{field}")))
            } else if mapping.external.contains(&name) {
                Ok(None)
            } else if let Some(table) = &table {
                Ok(Some(format!("{group}.{table}.{name}")))
            } else if mapping.passthrough_key.is_some() {
                Err(ConfigError::Message(format!(
                    "UCI option {key}.{name} cannot be placed: section {key} does not say which {group} it configures"
                )))
            } else {
                Err(unknown_option(key, name))
            }
        })
    }
}

fn mapped_field(options: &[UciOptionMapping], name: &str) -> Option<&'static str> {
    options
        .iter()
        .find(|option| option.uci_option == name)
        .map(|option| option.config_field)
}

fn unknown_option(section: &str, option: &str) -> ConfigError {
    ConfigError::Message(format!("UCI option {section}.{option} is not recognised"))
}

fn collect_options(
    key: &str,
    section: &Section<'_>,
    values: &mut Map<String, Value>,
    destination: impl Fn(&str) -> Result<Option<String>, ConfigError>,
) -> Result<(), ConfigError> {
    for (name, option) in section.iter() {
        // libuci treats an empty scalar as an unset option.
        if matches!(option, ConfigOption::Scalar(value) if value.is_empty()) {
            continue;
        }
        let Some(path) = destination(name.as_ref())? else {
            continue;
        };
        let origin = format!("UCI option {key}.{name}");
        let value = match option {
            ConfigOption::Scalar(value) => ValueKind::String(value.as_ref().to_owned()),
            ConfigOption::List(items) => ValueKind::Array(
                items
                    .iter()
                    .map(|item| {
                        Value::new(Some(&origin), ValueKind::String(item.as_ref().to_owned()))
                    })
                    .collect(),
            ),
        };
        if values
            .insert(path.clone(), Value::new(Some(&origin), value))
            .is_some()
        {
            return Err(ConfigError::Message(format!(
                "duplicate UCI destination key {path:?} while mapping {key}.{name}"
            )));
        }
    }
    Ok(())
}

impl Source for UciSource {
    fn clone_into_box(&self) -> Box<dyn Source + Send + Sync> {
        Box::new(self.clone())
    }

    fn collect(&self) -> Result<Map<String, Value>, ConfigError> {
        let path = &self.path;
        // ucifer reads package files directly. OpenWrt's runtime overlays and
        // staged deltas are outside this source's file input.
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Map::new()),
            Err(error) => {
                return Err(ConfigError::Message(format!(
                    "failed to read UCI package {path:?}: {error}"
                )));
            }
        };
        let parse_error = |detail| {
            ConfigError::Message(format!("failed to parse UCI package {path:?}: {detail}"))
        };
        let mut package = Document::default();
        let mut importer = package.importer();
        let mut names = HashSet::new();
        // The importer merges repeated declarations with the same name, which
        // would hide duplicate singleton sections and ambiguous instance IDs.
        for directive in DirectiveIter::new(&contents) {
            let directive = directive
                .map_err(|error| parse_error(error.to_string()))?
                .item;
            if let Directive::Section {
                name: Some(name), ..
            } = &directive
                && !names.insert(name.clone())
            {
                return Err(parse_error(format!("duplicate section name {name:?}")));
            }
            importer
                .import(directive)
                .map_err(|error| parse_error(error.to_string()))?;
        }

        self.collect_package(&package)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::law::LawKind;
    use crate::measurement::model::MeasurementTarget;
    use crate::settings::{ReflectorSettings, Settings};
    use config::{Config, Environment};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};

            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "unravel-uci-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config(instance: &str, contents: &str) -> Result<Config, ConfigError> {
        let dir = TestDir::new();
        fs::write(dir.0.join("unravel"), contents).unwrap();
        Config::builder()
            .add_source(UciSource::from_schema::<Settings>(instance, &dir.0))
            .build()
    }

    const WAN: &str = "
config instance 'wan'
    option upload_interface 'wan'
    option download_interface 'ifb4wan'
    option download_base_kbits '50000'
    option upload_base_kbits '10000'
";

    #[test]
    fn optional_package_distinguishes_missing_and_malformed_files() {
        let dir = TestDir::new();
        let source = UciSource::from_schema::<Settings>("wan", &dir.0);
        let missing = source.collect();
        fs::write(dir.0.join("unravel"), "config instance 'unterminated\n").unwrap();
        let malformed = source.collect();

        assert!(missing.unwrap().is_empty());
        assert!(
            malformed
                .unwrap_err()
                .to_string()
                .contains("failed to parse UCI package")
        );
    }

    #[test]
    fn maps_only_sections_owned_by_the_selected_instance() {
        let dir = TestDir::new();
        let no = dir.0.join("no.json");
        let eu = dir.0.join("eu.json");
        fs::write(
            &no,
            r#"[{"protocol": "icmp-echo", "endpoint": "192.0.2.1"}]"#,
        )
        .unwrap();
        fs::write(
            &eu,
            r#"[{"protocol": "irtt", "endpoint": "192.0.2.2:2112"}]"#,
        )
        .unwrap();
        let no = no.to_str().unwrap();
        let eu = eu.to_str().unwrap();
        let contents = format!(
            "{WAN}
config reflector
    option instance 'default'
    option protocol 'icmp-timestamps'
    option endpoint '192.0.2.3'
config reflectors
    option instance 'wan'
    list files '{no}'
config reflectors
    option instance 'default'
    list files '{eu}'
config reflector
    option instance 'wan'
    option protocol 'irtt'
    option endpoint '[2001:db8::1]:2112'
    option authentication '007'
config reflector 'extra'
    option instance 'default'
    option protocol 'irtt'
    option endpoint '192.0.2.4:3112'
config law 'satellite'
    option instance 'default'
    option type 'script'
    option path '/etc/unravel/custom.lua'
    option arbitrary '007'
    list endpoints 'one'
    list endpoints 'two'
config instance 'default'
    option enabled '1'
    option dry_run '1'
    option upload_interface 'sat'
    option download_interface 'ifb4sat'
config output
    option instance 'default'
    option folder '/tmp/satellite'
    option use_syslog '0'
"
        );
        let wan = Settings::from_config(config("wan", &contents).unwrap()).unwrap();
        assert!(!wan.dry_run);
        assert_eq!(wan.network.upload_interface, "wan");
        assert_eq!(wan.law.name, LawKind::Sqma);
        assert_eq!(wan.reflectors.files, [no]);
        assert_eq!(
            wan.reflectors.entries,
            [
                MeasurementTarget::Irtt {
                    endpoint: "[2001:db8::1]:2112".parse().unwrap(),
                    authentication: Some("007".into()),
                },
                MeasurementTarget::IcmpEcho {
                    endpoint: "192.0.2.1".parse().unwrap(),
                },
            ]
        );
        assert_eq!(wan.output.folder, "/tmp/unravel");
        assert!(!wan.observability.enabled);

        let sat = config("default", &contents).unwrap();
        let settings = Settings::from_config(sat.clone()).unwrap();
        assert!(settings.dry_run);
        assert_eq!(settings.network.upload_interface, "sat");
        assert_eq!(settings.law.name, LawKind::Script);
        assert_eq!(settings.reflectors.files, [eu]);
        assert_eq!(
            settings
                .reflectors
                .entries
                .iter()
                .map(MeasurementTarget::name)
                .collect::<Vec<_>>(),
            ["192.0.2.3", "192.0.2.4:3112", "192.0.2.2:2112"]
        );
        assert_eq!(settings.output.folder, "/tmp/satellite");
        assert_eq!(sat.get_string("law.script.arbitrary").unwrap(), "007");
        assert_eq!(
            sat.get::<Vec<String>>("law.script.endpoints").unwrap(),
            ["one", "two"]
        );
        assert!(matches!(
            sat.get::<String>("law.script.instance"),
            Err(ConfigError::NotFound(_))
        ));
    }

    #[test]
    fn shipped_openwrt_configuration_has_a_named_wan() {
        let settings: Settings = config("wan", include_str!("../../dist/openwrt/unravel.config"))
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert_eq!(settings.network.upload_interface, "wan");
        assert!(!settings.dry_run);
    }

    #[test]
    fn missing_optional_sections_use_rust_defaults() {
        let settings: Settings = config("wan", WAN).unwrap().try_deserialize().unwrap();
        assert!(!settings.dry_run);
        assert_eq!(settings.law.name, LawKind::Sqma);
        assert_eq!(
            settings.reflectors.files,
            ReflectorSettings::default().files
        );
        assert!(settings.reflectors.entries.is_empty());
        assert_eq!(settings.output.folder, "/tmp/unravel");
        assert!(!settings.observability.enabled);
    }

    #[test]
    fn rejects_unresolvable_ownership_for_the_package() {
        for (extra, expected) in [
            ("config instance", "explicit instance name"),
            ("config instance 'wan'", "duplicate section name \"wan\""),
            (
                "config instance 'wan.invalid'",
                "failed to parse UCI package",
            ),
            (
                "config reflector\n option endpoint '192.0.2.1'",
                "@reflector[0] must specify one owner",
            ),
            (
                "config output\n list instance 'wan'",
                "@output[0] must specify one owner",
            ),
            (
                "config law\n option instance ''",
                "@law[0] must specify one owner",
            ),
            (
                "config reflector 'seed'\n option instance 'missing'",
                "seed.instance names nonexistent instance \"missing\"",
            ),
            (
                "config network 'legacy'",
                "has unrecognised type \"network\"",
            ),
        ] {
            let error = config("wan", &format!("{WAN}\n{extra}\n")).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
        let error = config("missing", WAN).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no instance section named \"missing\"")
        );
    }

    #[test]
    fn invalid_owned_options_and_duplicate_singletons_are_local_to_the_owner() {
        for (sections, expected) in [
            (
                "config law\n option instance 'broken'\n list type 'sqma'",
                "@law[0].type must be a single value",
            ),
            (
                "config output\n option instance 'broken'\n option unexpected '1'",
                "@output[0].unexpected is not recognised",
            ),
            (
                "config law 'first'\n option instance 'broken'\nconfig law\n option instance 'broken'",
                "duplicate UCI \"law\" sections for instance \"broken\"",
            ),
        ] {
            let contents = format!("{WAN}\nconfig instance 'broken'\n{sections}\n");
            assert!(config("wan", &contents).is_ok());
            let error = config("broken", &contents).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn loads_dry_run_with_the_normal_source_precedence() {
        let dir = TestDir::new();
        let dir = &dir.0;
        fs::write(
            dir.join("unravel"),
            format!("{WAN}    option dry_run '1'\nconfig reflectors\n option instance 'wan'\n option files 'none'\n"),
        )
        .unwrap();
        let load = |values: &[(&str, &str)]| {
            let source = values
                .iter()
                .map(|&(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            Settings::load_from(
                "wan",
                dir,
                dir,
                Environment::with_prefix("UNRAVEL").source(Some(source)),
            )
            .unwrap()
        };
        assert!(load(&[]).dry_run);
        fs::write(dir.join("config.toml"), "this is not valid TOML").unwrap();

        #[cfg(feature = "toml")]
        {
            fs::write(
                dir.join("wan.toml"),
                "dry_run = false\n[network]\nupload_base_kbits = 9000\n",
            )
            .unwrap();
            let settings = load(&[]);
            assert!(!settings.dry_run);
            assert_eq!(settings.network.upload_base_kbits, 9000.0);
            assert_eq!(settings.network.download_base_kbits, 50000.0);
        }

        let settings = load(&[
            ("UNRAVEL__DRY_RUN", "true"),
            ("UNRAVEL__NETWORK__UPLOAD_INTERFACE", "override"),
        ]);
        assert!(settings.dry_run);
        assert_eq!(settings.network.upload_interface, "override");
        assert!(!load(&[("UNRAVEL__DRY_RUN", "off")]).dry_run);
    }
}
