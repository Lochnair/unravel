// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::settings::uci_schema::{UciConfigSchema, UciSectionMapping};
use config::{ConfigError as SourceError, Map, Source, Value, ValueKind};

use ucifer::document::{FromStrError, Section};
use ucifer::{ConfigOption, Document};

#[derive(Clone, Debug)]
enum PackageSelection {
    Selected(Vec<String>),
    All,
}

/// A section of a singleton type, mapped but not yet chosen.
struct MappedSection {
    /// `<package>.<name>` or `<package>.@<type>[0]`.
    key: String,
    section_type: &'static str,
    /// `None` for the anonymous section.
    name: Option<String>,
    values: Map<String, Value>,
    /// Only an anchor section has these.
    references: Vec<Reference>,
}

/// An anchor section's option naming the section of one type it uses.
struct Reference {
    section_type: String,
    target: String,
    uci_key: String,
}

#[derive(Clone, Debug)]
pub(crate) struct UciSource {
    packages: PackageSelection,
    /// Name of the anchor section to read. `None` reads the anonymous one.
    instance: Option<String>,
    required: bool,
    schema: &'static [UciSectionMapping],
    config_dir: Option<PathBuf>,
}

impl UciSource {
    fn with_selection(packages: PackageSelection) -> Self {
        Self {
            packages,
            instance: None,
            required: true,
            schema: &[],
            config_dir: None,
        }
    }

    pub(crate) fn new<I, S>(packages: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::with_selection(PackageSelection::Selected(
            packages.into_iter().map(Into::into).collect(),
        ))
    }

    #[allow(dead_code)]
    pub(crate) fn all() -> Self {
        Self::with_selection(PackageSelection::All)
    }

    pub(crate) fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    pub(crate) fn from_schema<T>() -> Self
    where
        T: UciConfigSchema,
    {
        Self::new([T::UCI_PACKAGE]).with_schema(T::UCI_SECTIONS)
    }

    /// Reads the anchor section with this name and the sections it names,
    /// instead of the anonymous sections.
    pub(crate) fn with_instance(mut self, instance: Option<&str>) -> Self {
        self.instance = instance.map(str::to_owned);
        self
    }

    pub(crate) fn with_schema(mut self, schema: &'static [UciSectionMapping]) -> Self {
        self.schema = schema;
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_config_dir(mut self, config_dir: impl AsRef<Path>) -> Self {
        self.config_dir = Some(config_dir.as_ref().to_owned());
        self
    }

    fn package_names(&self, config_dir: &Path) -> Result<Vec<String>, SourceError> {
        if let PackageSelection::Selected(names) = &self.packages {
            return Ok(names.clone());
        }

        let enumerate_error = |error| {
            SourceError::Message(format!(
                "failed to enumerate UCI packages in {config_dir:?}: {error}"
            ))
        };
        let entries = match fs::read_dir(config_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(enumerate_error(error)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(enumerate_error)?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.contains('.') && entry.path().is_file() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Without any schema the source maps every option to
    /// `<package>.<section>.<option>` and has nothing to check options against.
    fn has_schema(&self) -> bool {
        !self.schema.is_empty()
    }

    /// Value of the option that selects a passthrough section's sub-table.
    fn passthrough_table(
        mapping: &UciSectionMapping,
        key_field: &str,
        section: &Section<'_>,
        section_key: &str,
    ) -> Result<Option<String>, SourceError> {
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
                return Err(SourceError::Message(format!(
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
            return Err(SourceError::Message(format!(
                "UCI option {section_key}.{key_option} has unusable value {table:?}: \
                 expected letters, digits and underscores, and not the name of an \
                 option of this section"
            )));
        }

        Ok(Some(table.to_owned()))
    }

    fn collect_package(
        &self,
        package_name: &str,
        package: &Document<'_>,
        values: &mut Map<String, Value>,
        repeated_values: &mut Map<String, Vec<Value>>,
    ) -> Result<(), SourceError> {
        // Section types an anchor section can refer to by name.
        let reference_types: Vec<&str> = self
            .schema
            .iter()
            .filter(|mapping| !mapping.anchor)
            .map(|mapping| mapping.uci_section)
            .collect();

        let mut mapped = Vec::new();

        for (key, section) in package.sections_iter() {
            let section_type = package
                .type_name(key.type_key)
                .expect("document section types are interned");
            let section_name = package.section_name(*key);
            // A repeated section goes to `<parent section>.<field>`.
            let repeated_mapping = self.schema.iter().find_map(|parent| {
                let mapping = parent
                    .repeated
                    .iter()
                    .find(|mapping| mapping.uci_section == section_type)?;
                Some((
                    format!("{}.{}", parent.config_section, mapping.field),
                    mapping,
                ))
            });
            let mut section_values = Map::new();

            // config-rs paths omit the @ prefix used by UCI anonymous selectors.
            let config_selector = section_name
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{section_type}[{}]", key.index));
            let uci_selector = if section_name.is_some() {
                config_selector.clone()
            } else {
                format!("@{config_selector}")
            };

            let section_key = format!("{package_name}.{uci_selector}");

            let mut singleton = None;
            if repeated_mapping.is_none() && self.has_schema() {
                let Some(mapping) = self
                    .schema
                    .iter()
                    .find(|mapping| mapping.uci_section == section_type)
                else {
                    return Err(SourceError::Message(format!(
                        "UCI section {section_key} has unrecognised type {:?}",
                        section_type
                    )));
                };

                // `@type[n]` counts named sections too, so the index of the
                // first anonymous section is not always 0.
                if section_name.is_none()
                    && mapped.iter().any(|mapped: &MappedSection| {
                        mapped.section_type == mapping.uci_section && mapped.name.is_none()
                    })
                {
                    return Err(SourceError::Message(format!(
                        "UCI section {section_key} is not supported: only one anonymous \
                         {section_type:?} section is read; give the others names"
                    )));
                }

                singleton = Some(mapping);
            }

            let passthrough_table = match singleton {
                Some(mapping) => match mapping.passthrough_key {
                    Some(key_field) => {
                        Self::passthrough_table(mapping, key_field, section, &section_key)?
                    }
                    None => None,
                },
                None => None,
            };

            let mut references = Vec::new();

            for (option_name, option) in section.iter() {
                // libuci treats an empty scalar as an unset option.
                if matches!(option, ConfigOption::Scalar(value) if value.is_empty()) {
                    continue;
                }
                let option_name = option_name.as_ref();
                let uci_key = format!("{section_key}.{option_name}");

                let repeated_option = repeated_mapping.as_ref().and_then(|(_, mapping)| {
                    mapping
                        .options
                        .iter()
                        .find(|option| option.uci_option == option_name)
                });

                let destination_key = if let Some(mapping) = repeated_option {
                    mapping.config_field.to_owned()
                } else if repeated_mapping.is_some() {
                    return Err(SourceError::Message(format!(
                        "UCI option {uci_key} is not recognised"
                    )));
                } else if let Some(mapping) = singleton {
                    if let Some(option) = mapping
                        .options
                        .iter()
                        .find(|option| option.uci_option == option_name)
                    {
                        format!("{}.{}", mapping.config_section, option.config_field)
                    } else if mapping.anchor && reference_types.contains(&option_name) {
                        let ConfigOption::Scalar(target) = option else {
                            return Err(SourceError::Message(format!(
                                "UCI option {uci_key} must name one section, not a list"
                            )));
                        };
                        references.push(Reference {
                            section_type: option_name.to_owned(),
                            target: target.as_ref().to_owned(),
                            uci_key,
                        });
                        continue;
                    } else if mapping.external.contains(&option_name) {
                        continue;
                    } else if mapping.passthrough_key.is_some() {
                        let Some(table) = &passthrough_table else {
                            return Err(SourceError::Message(format!(
                                "UCI option {uci_key} cannot be placed: section \
                                 {section_key} does not say which {} it configures",
                                mapping.config_section
                            )));
                        };
                        format!("{}.{table}.{option_name}", mapping.config_section)
                    } else {
                        return Err(SourceError::Message(format!(
                            "UCI option {uci_key} is not recognised"
                        )));
                    }
                } else {
                    format!("{package_name}.{config_selector}.{option_name}")
                };

                let origin = format!("UCI option {uci_key}");

                let value = match option {
                    ConfigOption::Scalar(value) => {
                        Value::new(Some(&origin), ValueKind::String(value.as_ref().to_owned()))
                    }
                    ConfigOption::List(items) => {
                        let items = items
                            .iter()
                            .map(|item| {
                                Value::new(
                                    Some(&origin),
                                    ValueKind::String(item.as_ref().to_owned()),
                                )
                            })
                            .collect();

                        Value::new(Some(&origin), ValueKind::Array(items))
                    }
                };

                // Singleton sections are kept aside until the instance's own
                // sections are known.
                let destination = if repeated_mapping.is_some() || singleton.is_some() {
                    &mut section_values
                } else {
                    &mut *values
                };

                if destination.insert(destination_key.clone(), value).is_some() {
                    return Err(SourceError::Message(format!(
                        "duplicate UCI destination key \
                         {destination_key:?} while mapping {uci_key:?}"
                    )));
                }
            }

            if let Some((config_path, _)) = repeated_mapping {
                let origin = format!("UCI section {section_key}");
                repeated_values
                    .entry(config_path)
                    .or_default()
                    .push(Value::new(Some(&origin), ValueKind::Table(section_values)));
            } else if let Some(mapping) = singleton {
                mapped.push(MappedSection {
                    key: section_key,
                    section_type: mapping.uci_section,
                    name: section_name.map(str::to_owned),
                    values: section_values,
                    references,
                });
            }
        }

        for index in self.instance_sections(package_name, &mapped)? {
            for (key, value) in std::mem::take(&mut mapped[index].values) {
                if values.insert(key.clone(), value).is_some() {
                    return Err(SourceError::Message(format!(
                        "duplicate UCI destination key {key:?} while mapping {}",
                        mapped[index].key
                    )));
                }
            }
        }

        Ok(())
    }

    /// Indexes into `mapped` of the sections the instance uses: its anchor
    /// section and the sections that names. An anonymous anchor also uses the
    /// anonymous section of each type it does not name. Without an anchor
    /// section, every anonymous section is used.
    fn instance_sections(
        &self,
        package_name: &str,
        mapped: &[MappedSection],
    ) -> Result<Vec<usize>, SourceError> {
        let anchor_type = self
            .schema
            .iter()
            .find(|mapping| mapping.anchor)
            .map(|mapping| mapping.uci_section);
        let anonymous = |section_type: &str| {
            mapped
                .iter()
                .position(|section| section.section_type == section_type && section.name.is_none())
        };

        let anchor = match (anchor_type, &self.instance) {
            (Some(anchor_type), Some(instance)) => {
                let Some(index) = mapped
                    .iter()
                    .position(|section| section.name.as_deref() == Some(instance))
                else {
                    return Err(SourceError::Message(format!(
                        "UCI package {package_name:?} has no {anchor_type:?} section \
                         named {instance:?}"
                    )));
                };
                if mapped[index].section_type != anchor_type {
                    return Err(SourceError::Message(format!(
                        "UCI section {} is a {:?} section, not a {anchor_type:?} section",
                        mapped[index].key, mapped[index].section_type
                    )));
                }
                Some(index)
            }
            (Some(anchor_type), None) => {
                let index = anonymous(anchor_type);
                let named: Vec<&str> = mapped
                    .iter()
                    .filter(|section| section.section_type == anchor_type)
                    .filter_map(|section| section.name.as_deref())
                    .collect();
                if index.is_none() && !named.is_empty() {
                    return Err(SourceError::Message(format!(
                        "UCI package {package_name:?} has no anonymous {anchor_type:?} \
                         section; start unravel with --instance <name> (configured: {})",
                        named.join(", ")
                    )));
                }
                index
            }
            (None, Some(_)) => {
                return Err(SourceError::Message(
                    "instances need a UCI schema with an anchor section".to_owned(),
                ));
            }
            (None, None) => None,
        };

        let Some(anchor) = anchor else {
            return Ok(mapped
                .iter()
                .enumerate()
                .filter(|(_, section)| section.name.is_none())
                .map(|(index, _)| index)
                .collect());
        };

        let mut chosen = vec![anchor];
        for mapping in self.schema.iter().filter(|mapping| !mapping.anchor) {
            let reference = mapped[anchor]
                .references
                .iter()
                .find(|reference| reference.section_type == mapping.uci_section);

            if let Some(reference) = reference {
                let Some(index) = mapped
                    .iter()
                    .position(|section| section.name.as_deref() == Some(&reference.target))
                else {
                    return Err(SourceError::Message(format!(
                        "UCI option {} names {package_name}.{}, which does not exist",
                        reference.uci_key, reference.target
                    )));
                };
                if mapped[index].section_type != mapping.uci_section {
                    return Err(SourceError::Message(format!(
                        "UCI option {} names {}, which is a {:?} section, not a {:?} section",
                        reference.uci_key,
                        mapped[index].key,
                        mapped[index].section_type,
                        mapping.uci_section
                    )));
                }
                chosen.push(index);
            } else if mapped[anchor].name.is_none()
                && let Some(index) = anonymous(mapping.uci_section)
            {
                chosen.push(index);
            }
        }

        Ok(chosen)
    }
}

impl Source for UciSource {
    fn clone_into_box(&self) -> Box<dyn Source + Send + Sync> {
        Box::new(self.clone())
    }

    fn collect(&self) -> Result<Map<String, Value>, SourceError> {
        let config_dir = self
            .config_dir
            .as_deref()
            .unwrap_or(Path::new("/etc/config"));
        let mut values = Map::new();
        let mut repeated_values = Map::new();

        // ucifer reads package files directly. OpenWrt's runtime overlays and staged
        // deltas are managed by libuci and are outside this source's file input.
        for package_name in self.package_names(config_dir)? {
            let path = config_dir.join(&package_name);
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    return if self.required {
                        Err(SourceError::NotFound(format!("UCI package {path:?}")))
                    } else {
                        Ok(Map::new())
                    };
                }
                Err(error) => {
                    return Err(SourceError::Message(format!(
                        "failed to read UCI package {path:?}: {error}"
                    )));
                }
            };
            let package = Document::try_from(contents.as_str()).map_err(|error| {
                let detail = match &error {
                    FromStrError::Syntax(error) => error.to_string(),
                    FromStrError::Import(error) => error.to_string(),
                };
                SourceError::Message(format!("failed to parse UCI package {path:?}: {detail}"))
            })?;

            self.collect_package(&package_name, &package, &mut values, &mut repeated_values)?;
        }

        for (config_path, sections) in repeated_values {
            let origin = "UCI repeated sections".to_owned();
            if values
                .insert(
                    config_path.clone(),
                    Value::new(Some(&origin), ValueKind::Array(sections)),
                )
                .is_some()
            {
                return Err(SourceError::Message(format!(
                    "duplicate UCI destination key {config_path:?} while mapping repeated sections"
                )));
            }
        }

        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use crate::settings::uci_schema::{UciOptionMapping, UciRepeatedSectionMapping};

    #[test]
    fn optional_package_distinguishes_missing_and_malformed_files() {
        let root = std::env::temp_dir().join(format!("unravel-uci-{}", std::process::id()));
        let config_dir = root.join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let config_file = config_dir.join("unravel");
        let source = UciSource::from_schema::<Settings>()
            .with_config_dir(&config_dir)
            .required(false);
        let missing = source.collect();
        fs::write(&config_file, "config network 'unterminated\n").unwrap();
        let result = source.collect();

        fs::remove_dir_all(root).unwrap();
        assert!(missing.unwrap().is_empty());
        assert!(result.is_err());
    }

    #[test]
    fn maps_anonymous_indices_and_preserves_repeated_list_order() {
        let config_dir =
            std::env::temp_dir().join(format!("unravel-uci-mapping-{}", std::process::id()));
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("unravel"),
            "\
config network 'named'
    option device 'eth0'
config reflector 'first'
    list address 'one'
    list address 'two'
config network
    option device 'eth1'
    option unused ''
config reflector
    list address 'three'
",
        )
        .unwrap();

        let source = UciSource::new(["unravel"]).with_config_dir(&config_dir);
        let raw = source.collect().unwrap();
        assert!(raw.contains_key("unravel.named.device"));
        assert!(raw.contains_key("unravel.network[1].device"));
        assert!(!raw.contains_key("unravel.network[1].unused"));

        let source = source.with_schema(&[UciSectionMapping {
            uci_section: "network",
            config_section: "network",
            anchor: true,
            options: &[UciOptionMapping {
                uci_option: "device",
                config_field: "device",
            }],
            external: &[],
            repeated: &[UciRepeatedSectionMapping {
                uci_section: "reflector",
                field: "entries",
                options: &[UciOptionMapping {
                    uci_option: "address",
                    config_field: "addresses",
                }],
            }],
            passthrough_key: None,
        }]);
        let result = config::Config::builder().add_source(source.clone()).build();
        let named = config::Config::builder()
            .add_source(source.with_instance(Some("named")))
            .build();
        fs::remove_dir_all(config_dir).unwrap();

        let config = result.unwrap();
        assert_eq!(config.get_string("network.device").unwrap(), "eth1");
        assert_eq!(named.unwrap().get_string("network.device").unwrap(), "eth0");
        assert_eq!(
            config
                .get::<Vec<String>>("network.entries[0].addresses")
                .unwrap(),
            ["one", "two"]
        );
        assert_eq!(
            config
                .get::<Vec<String>>("network.entries[1].addresses")
                .unwrap(),
            ["three"]
        );
    }
}
