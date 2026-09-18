use std::{collections::BTreeMap, marker::PhantomData};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use super::{Error, Result};

/// Supported instructions and rubric descriptions, including structured JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Description {
    Text(String),
    Object(Map<String, Value>),
    Array(Vec<Value>),
    Null,
}

impl From<&str> for Description {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}

impl From<String> for Description {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl TryFrom<Value> for Description {
    type Error = Error;

    fn try_from(value: Value) -> Result<Self> {
        match value {
            Value::String(value) => Ok(Self::Text(value)),
            Value::Object(value) => Ok(Self::Object(value)),
            Value::Array(value) => Ok(Self::Array(value)),
            Value::Null => Ok(Self::Null),
            _ => Err(Error::InvalidRequest(
                "descriptions must be strings, objects, arrays, or null",
            )),
        }
    }
}

/// A finite probability in the inclusive range 0..=1, checked on deserialization.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Probability(f64);

impl Probability {
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Probability {
    type Error = &'static str;

    fn try_from(value: f64) -> std::result::Result<Self, Self::Error> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err("probability must be finite and between zero and one")
        }
    }
}

impl From<Probability> for f64 {
    fn from(value: Probability) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoulAnswer {
    pub noul: Probability,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(deserialize = "C: Ord + Deserialize<'de>"))]
pub struct ChoiceAnswer<C: Ord = String> {
    pub choice: C,
    pub probabilities: BTreeMap<C, Probability>,
    pub confidence: Probability,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScoreAnswer {
    /// Probability-weighted score, not an integer or a normalized probability.
    pub score: f64,
    #[serde(deserialize_with = "deserialize_levels")]
    pub legend: BTreeMap<u32, Description>,
    #[serde(deserialize_with = "deserialize_levels")]
    pub probabilities: BTreeMap<u32, Probability>,
    pub confidence: Probability,
}

fn deserialize_levels<'de, D, T>(deserializer: D) -> std::result::Result<BTreeMap<u32, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    // Internally tagged enums buffer their payload before deserializing it, so
    // serde_json's special numeric-map-key conversion is no longer available.
    let values = BTreeMap::<String, T>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|(key, value)| {
            let index = key
                .parse::<u32>()
                .map_err(|_| serde::de::Error::custom("invalid score level index"))?;
            if key != index.to_string() {
                return Err(serde::de::Error::custom(
                    "score level indices must be canonical nonnegative integers",
                ));
            }
            Ok((index, value))
        })
        .collect()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Model {
    pub name: String,
    pub description: String,
    pub release_date: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelsResponse {
    pub models: Vec<Model>,
    #[serde(skip)]
    pub request_id: Option<String>,
}

/// A question whose answer type is fixed by its constructor.
///
/// ```compile_fail
/// use jeeves::typesafe::{NoulAnswer, Question, ScoreAnswer};
/// let question: Question<ScoreAnswer> = Question::noul("Is this spam?");
/// ```
#[derive(Debug)]
pub struct Question<A> {
    pub(super) wire: WireQuestion,
    answer: PhantomData<fn() -> A>,
}

impl<A> Clone for Question<A> {
    fn clone(&self) -> Self {
        Self {
            wire: self.wire.clone(),
            answer: PhantomData,
        }
    }
}

impl<A> Question<A> {
    fn new(wire: WireQuestion) -> Self {
        Self {
            wire,
            answer: PhantomData,
        }
    }

    /// Compact JSON bytes, including instructions and criteria, not a token count.
    pub(crate) fn json_size(&self) -> Result<usize> {
        serde_json::to_vec(&self.wire)
            .map(|bytes| bytes.len())
            .map_err(Error::Encode)
    }
}

impl Question<NoulAnswer> {
    pub fn noul(instructions: impl Into<Description>) -> Self {
        Self::new(WireQuestion::Noul {
            instructions: instructions.into(),
            criteria: None,
        })
    }

    pub fn with_criteria(
        mut self,
        yes: impl Into<Description>,
        no: impl Into<Description>,
    ) -> Self {
        if let WireQuestion::Noul { criteria, .. } = &mut self.wire {
            *criteria = Some(NoulCriteria {
                yes: yes.into(),
                no: no.into(),
            });
        }
        self
    }
}

impl<C: Ord + Serialize + DeserializeOwned> Question<ChoiceAnswer<C>> {
    /// Choice labels may be strings or serde unit enums (including renamed variants).
    pub fn choice<D: Into<Description>>(
        instructions: impl Into<Description>,
        options: impl IntoIterator<Item = (C, D)>,
    ) -> Result<Self> {
        let mut criteria = BTreeMap::new();
        for (label, description) in options {
            let Value::String(key) = serde_json::to_value(&label).map_err(Error::Encode)? else {
                return Err(Error::InvalidRequest(
                    "choice labels must serialize as strings",
                ));
            };
            // Verify labels can be decoded back into the caller's chosen Rust type.
            let decoded: C = serde_json::from_value(Value::String(key.clone())).map_err(|_| {
                Error::InvalidRequest("choice labels must deserialize back into their Rust type")
            })?;
            if decoded != label
                || key.trim().is_empty()
                || criteria.insert(key, description.into()).is_some()
            {
                return Err(Error::InvalidRequest(
                    "choice labels must be nonempty, distinct, and round-trip correctly",
                ));
            }
        }
        if criteria.is_empty() {
            return Err(Error::InvalidRequest("a choice needs at least one option"));
        }
        Ok(Self::new(WireQuestion::Choice {
            instructions: instructions.into(),
            criteria,
        }))
    }
}

impl Question<ScoreAnswer> {
    pub fn score<D: Into<Description>>(
        instructions: impl Into<Description>,
        levels: impl IntoIterator<Item = D>,
    ) -> Result<Self> {
        let criteria: Vec<_> = levels.into_iter().map(Into::into).collect();
        if criteria.len() < 2 || criteria.len() > u32::MAX as usize {
            return Err(Error::InvalidRequest("a score needs at least two levels"));
        }
        Ok(Self::new(WireQuestion::Score {
            instructions: instructions.into(),
            criteria,
        }))
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct NoulCriteria {
    #[serde(rename = "true")]
    yes: Description,
    #[serde(rename = "false")]
    no: Description,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum WireQuestion {
    Noul {
        instructions: Description,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    Choice {
        instructions: Description,
        criteria: BTreeMap<String, Description>,
    },
    Score {
        instructions: Description,
        criteria: Vec<Description>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum WireAnswer {
    Noul(NoulAnswer),
    Choice(ChoiceAnswer),
    Score(ScoreAnswer),
}

impl WireAnswer {
    pub(super) fn validate(&self, question: &WireQuestion) -> Result<()> {
        let valid = match (self, question) {
            (Self::Noul(_), WireQuestion::Noul { .. }) => true,
            (Self::Choice(answer), WireQuestion::Choice { criteria, .. }) => {
                criteria.contains_key(&answer.choice)
                    && answer.probabilities.keys().eq(criteria.keys())
                    && valid_distribution(answer.probabilities.values())
            }
            (Self::Score(answer), WireQuestion::Score { criteria, .. }) => {
                answer.score.is_finite()
                    && (0.0..=(criteria.len() - 1) as f64).contains(&answer.score)
                    && answer.legend.keys().copied().eq(0..criteria.len() as u32)
                    && answer.legend.values().eq(criteria.iter())
                    && answer.probabilities.keys().eq(answer.legend.keys())
                    && valid_distribution(answer.probabilities.values())
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(Error::InvalidResponse(
                "answer type, options, rubric, or probability distribution does not match its question",
            ))
        }
    }
}

fn valid_distribution<'a>(values: impl Iterator<Item = &'a Probability>) -> bool {
    // Permit ordinary floating-point rounding, but reject incomplete distributions.
    (values.map(|value| value.get()).sum::<f64>() - 1.0).abs() <= 0.001
}
