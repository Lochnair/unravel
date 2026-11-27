// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! The options a script law declares, and their enforcement on the merged
//! `law.script` table. Frontends read the same schema to build their forms.

use std::{collections::HashSet, ops::RangeInclusive};

use anyhow::{Context, bail, ensure};
use config::ValueKind;
use serde::{Deserialize, Serialize};

use super::Value;

/// Keys of the `law` section that never reach the script: `instance` is the
/// UCI owner option, `type` selects the law (`LawSettings`) and `path` names
/// the script.
const RESERVED: &[&str] = &["instance", "type", "path"];

/// Lua 5.1 numbers are doubles. Up to 2^53 - 1 each integer has a double of its
/// own; 2^53 + 1 already rounds to 2^53. Integers outside this range are
/// rejected rather than rounded.
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Schema {
    pub options: Vec<OptionSpec>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Boolean,
    Integer,
    Number,
    String,
}

/// One option, as the script declares it. Which fields go with which `type`
/// is checked by [`Schema::new`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OptionSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Scalar>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// Inclusive bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Exclusive bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// A default, exactly as the script wrote it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Scalar {
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(String),
}

impl Schema {
    /// Checks the declaration. The first problem is the error.
    pub fn new(mut options: Vec<OptionSpec>) -> anyhow::Result<Self> {
        let mut names = HashSet::new();
        for (index, option) in options.iter_mut().enumerate() {
            option
                .check()
                .with_context(|| format!("options[{}] ({:?})", index + 1, option.name))?;
            ensure!(
                names.insert(option.name.clone()),
                "options[{}]: option {:?} is declared twice",
                index + 1,
                option.name
            );
        }
        Ok(Self { options })
    }

    /// The script's config from the merged `law.script` table, without
    /// `path`. Errors name the option and where it was set, never its value.
    pub fn apply(&self, mut table: config::Map<String, config::Value>) -> anyhow::Result<Value> {
        let mut errors = Vec::new();

        let mut unknown: Vec<_> = table
            .iter()
            .filter(|(key, _)| !self.options.iter().any(|o| &o.name == *key))
            .map(|(key, value)| (key.clone(), value.origin().map(str::to_owned)))
            .collect();
        unknown.sort();
        for (key, origin) in unknown {
            errors.push(format!(
                "{}: not an option of this script",
                location(&key, origin.as_deref())
            ));
        }

        let mut entries = Vec::new();
        for option in &self.options {
            match table.remove(&option.name) {
                Some(value) => match option.convert(&value.kind) {
                    Some(converted) => entries.push((option.name.clone(), converted)),
                    None => errors.push(format!(
                        "{}: expected {}",
                        location(&option.name, value.origin()),
                        option.expected()
                    )),
                },
                None if option.required => {
                    errors.push(format!("{}: required", location(&option.name, None)));
                }
                None => {
                    if let Some(default) = &option.default {
                        entries.push((option.name.clone(), default.to_value()));
                    }
                }
            }
        }

        if !errors.is_empty() {
            bail!(
                "script config validation failed:\n  {}",
                errors.join("\n  ")
            );
        }
        Ok(Value::Map(entries))
    }
}

fn location(key: &str, origin: Option<&str>) -> String {
    match origin {
        Some(origin) => format!("law.script.{key} ({origin})"),
        None => format!("law.script.{key}"),
    }
}

impl Scalar {
    fn to_value(&self) -> Value {
        match self {
            Scalar::Boolean(b) => Value::Bool(*b),
            Scalar::Integer(n) => Value::Number(*n as f64),
            Scalar::Number(n) => Value::Number(*n),
            Scalar::String(s) => Value::String(s.clone()),
        }
    }
}

impl OptionSpec {
    fn check(&mut self) -> anyhow::Result<()> {
        let name = self.name.as_bytes();
        ensure!(
            name.first().is_some_and(u8::is_ascii_lowercase)
                && name
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_'),
            "name must be a lowercase letter followed by lowercase letters, digits and underscores"
        );
        ensure!(
            !RESERVED.contains(&self.name.as_str()),
            "name is reserved by the law section"
        );

        let bounds = [self.min, self.max, self.above, self.below];
        let numeric = matches!(self.kind, Kind::Integer | Kind::Number);
        ensure!(
            numeric || bounds.iter().all(Option::is_none),
            "min, max, above and below need an integer or number option"
        );
        ensure!(
            self.choices.is_none() || self.kind == Kind::String,
            "choices need a string option"
        );
        ensure!(
            !(self.required && self.default.is_some()),
            "a required option can't have a default"
        );
        ensure!(
            !(self.min.is_some() && self.above.is_some()),
            "min and above can't both be set"
        );
        ensure!(
            !(self.max.is_some() && self.below.is_some()),
            "max and below can't both be set"
        );
        for bound in bounds.into_iter().flatten() {
            ensure!(bound.is_finite(), "bounds must be finite");
            if self.kind == Kind::Integer {
                ensure!(
                    integral(bound).is_some(),
                    "bounds of an integer option must be integers within ±{MAX_SAFE_INTEGER}"
                );
            }
        }
        let empty = match self.kind {
            Kind::Integer => self.integer_range().is_none(),
            _ => match (self.lower(), self.upper()) {
                (Some((lo, true)), Some((hi, true))) => lo > hi,
                (Some((lo, _)), Some((hi, _))) => lo >= hi,
                _ => false,
            },
        };
        ensure!(!empty, "no value satisfies the bounds");

        if let Some(choices) = &self.choices {
            ensure!(!choices.is_empty(), "choices must not be empty");
            let mut seen = HashSet::new();
            ensure!(
                choices.iter().all(|choice| seen.insert(choice)),
                "choices must be unique"
            );
        }

        if let Some(default) = &self.default {
            // Defaults are typed Lua values, so they aren't coerced. Lua 5.1
            // can't tell 6 from 6.0, so an integer is any integral number.
            let value = match (self.kind, default) {
                (Kind::Boolean, Scalar::Boolean(b)) => Some(Scalar::Boolean(*b)),
                (Kind::String, Scalar::String(s)) => {
                    self.accepts_string(s).then(|| default.clone())
                }
                (Kind::Number, Scalar::Integer(n)) => safe(*n)
                    .map(|n| n as f64)
                    .filter(|n| self.accepts_number(*n))
                    .map(Scalar::Number),
                (Kind::Number, Scalar::Number(n)) => {
                    self.accepts_number(*n).then_some(Scalar::Number(*n))
                }
                (Kind::Integer, Scalar::Integer(n)) => safe(*n)
                    .filter(|n| self.accepts_integer(*n))
                    .map(Scalar::Integer),
                (Kind::Integer, Scalar::Number(n)) => integral(*n)
                    .filter(|n| self.accepts_integer(*n))
                    .map(Scalar::Integer),
                _ => None,
            };
            ensure!(value.is_some(), "default must be {}", self.expected());
            self.default = value;
        }
        Ok(())
    }

    fn lower(&self) -> Option<(f64, bool)> {
        self.min
            .map(|v| (v, true))
            .or(self.above.map(|v| (v, false)))
    }

    fn upper(&self) -> Option<(f64, bool)> {
        self.max
            .map(|v| (v, true))
            .or(self.below.map(|v| (v, false)))
    }

    /// The integers an integer option takes, or `None` if there are none.
    /// Exclusive bounds become inclusive ones by integer arithmetic, and the
    /// range never leaves ±[`MAX_SAFE_INTEGER`], so `above = 2^53 - 1` alone
    /// is as empty as `above = 1, below = 2`. Bounds are checked integral
    /// before this is called.
    fn integer_range(&self) -> Option<RangeInclusive<i64>> {
        let bound = |b: Option<(f64, bool)>, step: i64, unbounded: i64| match b {
            Some((b, inclusive)) => integral(b).map(|b| if inclusive { b } else { b + step }),
            None => Some(unbounded),
        };
        let lo = bound(self.lower(), 1, -MAX_SAFE_INTEGER)?.max(-MAX_SAFE_INTEGER);
        let hi = bound(self.upper(), -1, MAX_SAFE_INTEGER)?.min(MAX_SAFE_INTEGER);
        (lo <= hi).then_some(lo..=hi)
    }

    /// Converts a configured value, strictly by the option's type.
    fn convert(&self, kind: &ValueKind) -> Option<Value> {
        match self.kind {
            Kind::String => match kind {
                ValueKind::String(s) => self.accepts_string(s).then(|| Value::String(s.clone())),
                _ => None,
            },
            Kind::Boolean => config::Value::new(None, kind.clone())
                .into_bool()
                .ok()
                .map(Value::Bool),
            Kind::Integer => {
                let n = whole(kind)?;
                self.accepts_integer(n).then_some(Value::Number(n as f64))
            }
            Kind::Number => {
                let n = number(kind)?;
                self.accepts_number(n).then_some(Value::Number(n))
            }
        }
    }

    fn accepts_integer(&self, n: i64) -> bool {
        self.integer_range().is_some_and(|range| range.contains(&n))
    }

    fn accepts_number(&self, n: f64) -> bool {
        n.is_finite()
            && self
                .lower()
                .is_none_or(|(lo, inclusive)| if inclusive { n >= lo } else { n > lo })
            && self
                .upper()
                .is_none_or(|(hi, inclusive)| if inclusive { n <= hi } else { n < hi })
    }

    fn accepts_string(&self, s: &str) -> bool {
        self.choices
            .as_ref()
            .is_none_or(|choices| choices.iter().any(|choice| choice == s))
    }

    /// What a valid value looks like, for error messages.
    fn expected(&self) -> String {
        let mut text = match self.kind {
            Kind::Boolean => "a boolean".to_owned(),
            Kind::Integer => "an integer".to_owned(),
            Kind::Number => "a number".to_owned(),
            Kind::String => "a string".to_owned(),
        };
        let mut bounds = Vec::new();
        if let Some((lo, inclusive)) = self.lower() {
            bounds.push(format!(
                "{} {lo}",
                if inclusive { "at least" } else { "above" }
            ));
        }
        if let Some((hi, inclusive)) = self.upper() {
            bounds.push(format!(
                "{} {hi}",
                if inclusive { "at most" } else { "below" }
            ));
        }
        if !bounds.is_empty() {
            text = format!("{text} {}", bounds.join(" and "));
        }
        if let Some(choices) = &self.choices {
            let choices: Vec<_> = choices.iter().map(|c| format!("{c:?}")).collect();
            text = format!("{text}, one of {}", choices.join(", "));
        }
        text
    }
}

fn safe(n: i64) -> Option<i64> {
    (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER)
        .contains(&n)
        .then_some(n)
}

/// `n` as an integer, if it is a whole number in the safe range.
fn integral(n: f64) -> Option<i64> {
    // MAX_SAFE_INTEGER is exact as a double, so this bounds check is too.
    (n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER as f64).then_some(n as i64)
}

/// An integer of any integer kind, in the safe range.
fn typed_integer(kind: &ValueKind) -> Option<i64> {
    match kind {
        ValueKind::I64(n) => safe(*n),
        ValueKind::I128(n) => safe(i64::try_from(*n).ok()?),
        ValueKind::U64(n) => safe(i64::try_from(*n).ok()?),
        ValueKind::U128(n) => safe(i64::try_from(*n).ok()?),
        _ => None,
    }
}

/// An integer option's value. UCI strings are parsed. A whole float, such as
/// TOML's `6.0`, is the integer it equals: Lua holds both as the same double.
/// Booleans and fractions are not integers.
fn whole(kind: &ValueKind) -> Option<i64> {
    match kind {
        ValueKind::Float(n) => integral(*n),
        ValueKind::String(s) => safe(s.trim().parse().ok()?),
        _ => typed_integer(kind),
    }
}

/// A number option's value. An integer is taken only if a double holds it
/// exactly; a decimal string is parsed as a float literal, which rounds.
fn number(kind: &ValueKind) -> Option<f64> {
    let n = match kind {
        ValueKind::Float(n) => *n,
        ValueKind::String(s) => s.trim().parse().ok()?,
        _ => typed_integer(kind)? as f64,
    };
    n.is_finite().then_some(n)
}
