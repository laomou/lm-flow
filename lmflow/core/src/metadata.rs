use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

pub type Metadata = BTreeMap<String, MetadataValue>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MetadataValue {
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
}

impl From<bool> for MetadataValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for MetadataValue {
    fn from(value: i64) -> Self {
        Self::I64(value)
    }
}

impl From<f64> for MetadataValue {
    fn from(value: f64) -> Self {
        Self::F64(value)
    }
}

impl From<String> for MetadataValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for MetadataValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComparisonOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    Exists,
    Contains,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataCondition {
    pub metadata: String,
    pub op: ComparisonOp,
    #[serde(default)]
    pub value: Option<MetadataValue>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimestampComparison {
    pub op: ComparisonOp,
    #[serde(default)]
    pub value: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum MetadataPredicate {
    All { all: Vec<MetadataPredicate> },
    Any { any: Vec<MetadataPredicate> },
    Not { not: Box<MetadataPredicate> },
    Timestamp { timestamp: TimestampComparison },
    Metadata(MetadataCondition),
}

impl MetadataPredicate {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::All { all } => validate_group("all", all),
            Self::Any { any } => validate_group("any", any),
            Self::Not { not } => not.validate(),
            Self::Timestamp { timestamp } => {
                if matches!(timestamp.op, ComparisonOp::Exists | ComparisonOp::Contains) {
                    return Err(Error::InvalidArg(format!(
                        "timestamp condition does not support `{}`",
                        op_name(timestamp.op)
                    )));
                }
                if timestamp.value.is_none() {
                    return Err(Error::InvalidArg(
                        "timestamp condition requires `value`".into(),
                    ));
                }
                Ok(())
            }
            Self::Metadata(condition) => {
                if condition.metadata.is_empty() {
                    return Err(Error::InvalidArg(
                        "metadata condition key must not be empty".into(),
                    ));
                }
                if condition.op == ComparisonOp::Exists {
                    if condition.value.is_some() {
                        return Err(Error::InvalidArg(
                            "metadata `exists` condition must not declare `value`".into(),
                        ));
                    }
                } else if condition.value.is_none() {
                    return Err(Error::InvalidArg(format!(
                        "metadata `{}` condition requires `value`",
                        op_name(condition.op)
                    )));
                }
                if condition.op == ComparisonOp::Contains
                    && !matches!(condition.value, Some(MetadataValue::String(_)))
                {
                    return Err(Error::InvalidArg(
                        "metadata `contains` condition requires a string value".into(),
                    ));
                }
                Ok(())
            }
        }
    }

    pub fn evaluate(&self, metadata: &Metadata, timestamp: Timestamp) -> Result<bool> {
        match self {
            Self::All { all } => {
                for predicate in all {
                    if !predicate.evaluate(metadata, timestamp)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Self::Any { any } => {
                for predicate in any {
                    if predicate.evaluate(metadata, timestamp)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Self::Not { not } => Ok(!not.evaluate(metadata, timestamp)?),
            Self::Timestamp {
                timestamp: condition,
            } => compare_i64(
                timestamp.0,
                condition.op,
                condition.value.expect("validated timestamp condition"),
            ),
            Self::Metadata(condition) => {
                let actual = metadata.get(&condition.metadata);
                if condition.op == ComparisonOp::Exists {
                    return Ok(actual.is_some());
                }
                let Some(actual) = actual else {
                    return Ok(false);
                };
                compare_metadata(
                    actual,
                    condition.op,
                    condition
                        .value
                        .as_ref()
                        .expect("validated metadata condition"),
                )
            }
        }
    }
}

fn validate_group(name: &str, predicates: &[MetadataPredicate]) -> Result<()> {
    if predicates.is_empty() {
        return Err(Error::InvalidArg(format!(
            "metadata `{name}` condition must not be empty"
        )));
    }
    predicates.iter().try_for_each(MetadataPredicate::validate)
}

fn compare_metadata(
    actual: &MetadataValue,
    op: ComparisonOp,
    expected: &MetadataValue,
) -> Result<bool> {
    match (actual, expected) {
        (MetadataValue::Bool(left), MetadataValue::Bool(right)) => compare_ord(left, op, right),
        (MetadataValue::I64(left), MetadataValue::I64(right)) => compare_ord(left, op, right),
        (MetadataValue::F64(left), MetadataValue::F64(right)) => compare_float(*left, op, *right),
        (MetadataValue::I64(left), MetadataValue::F64(right)) => {
            compare_float(*left as f64, op, *right)
        }
        (MetadataValue::F64(left), MetadataValue::I64(right)) => {
            compare_float(*left, op, *right as f64)
        }
        (MetadataValue::String(left), MetadataValue::String(right)) => {
            if op == ComparisonOp::Contains {
                Ok(left.contains(right))
            } else {
                compare_ord(left, op, right)
            }
        }
        _ => Err(Error::InvalidArg(format!(
            "metadata comparison type mismatch for `{}`",
            op_name(op)
        ))),
    }
}

fn compare_i64(left: i64, op: ComparisonOp, right: i64) -> Result<bool> {
    compare_ord(&left, op, &right)
}

fn compare_float(left: f64, op: ComparisonOp, right: f64) -> Result<bool> {
    Ok(match op {
        ComparisonOp::Eq => left == right,
        ComparisonOp::Ne => left != right,
        ComparisonOp::Gt => left > right,
        ComparisonOp::Gte => left >= right,
        ComparisonOp::Lt => left < right,
        ComparisonOp::Lte => left <= right,
        ComparisonOp::Exists | ComparisonOp::Contains => {
            return Err(Error::InvalidArg(format!(
                "numeric comparison does not support `{}`",
                op_name(op)
            )));
        }
    })
}

fn compare_ord<T: PartialOrd + PartialEq>(left: &T, op: ComparisonOp, right: &T) -> Result<bool> {
    Ok(match op {
        ComparisonOp::Eq => left == right,
        ComparisonOp::Ne => left != right,
        ComparisonOp::Gt => left > right,
        ComparisonOp::Gte => left >= right,
        ComparisonOp::Lt => left < right,
        ComparisonOp::Lte => left <= right,
        ComparisonOp::Exists | ComparisonOp::Contains => {
            return Err(Error::InvalidArg(format!(
                "comparison does not support `{}`",
                op_name(op)
            )));
        }
    })
}

fn op_name(op: ComparisonOp) -> &'static str {
    match op {
        ComparisonOp::Eq => "eq",
        ComparisonOp::Ne => "ne",
        ComparisonOp::Gt => "gt",
        ComparisonOp::Gte => "gte",
        ComparisonOp::Lt => "lt",
        ComparisonOp::Lte => "lte",
        ComparisonOp::Exists => "exists",
        ComparisonOp::Contains => "contains",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_predicate_evaluates() {
        let predicate: MetadataPredicate = serde_yaml::from_str(
            r#"
all:
  - { metadata: confidence, op: gte, value: 0.8 }
  - any:
      - { metadata: category, op: eq, value: person }
      - timestamp: { op: gte, value: 100 }
"#,
        )
        .unwrap();
        predicate.validate().unwrap();
        let metadata = Metadata::from([
            ("confidence".into(), MetadataValue::F64(0.9)),
            ("category".into(), MetadataValue::String("vehicle".into())),
        ]);
        assert!(predicate.evaluate(&metadata, Timestamp(100)).unwrap());
    }
}
