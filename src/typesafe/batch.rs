use std::{collections::BTreeMap, marker::PhantomData, sync::Arc};

use serde::{Deserialize, de::DeserializeOwned};

use super::{
    Error, Question, Result, Usage,
    types::{WireAnswer, WireQuestion},
};

/// A heterogeneous collection of questions evaluated against the same state.
#[derive(Debug, Default)]
pub struct Batch {
    pub(super) questions: BTreeMap<String, WireQuestion>,
    scope: Arc<()>,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add<A>(
        &mut self,
        name: impl Into<String>,
        question: Question<A>,
    ) -> Result<AnswerKey<A>> {
        let name = name.into();
        if name.trim().is_empty() || self.questions.contains_key(&name) {
            return Err(Error::InvalidRequest(
                "question names must be nonempty and unique within a batch",
            ));
        }
        self.questions.insert(name.clone(), question.wire);
        Ok(AnswerKey {
            name,
            scope: self.scope.clone(),
            answer: PhantomData,
        })
    }
}

/// A typed answer key belonging to exactly one batch. Its type cannot be changed.
#[derive(Debug)]
pub struct AnswerKey<A> {
    name: String,
    scope: Arc<()>,
    answer: PhantomData<fn() -> A>,
}

impl<A> Clone for AnswerKey<A> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            scope: self.scope.clone(),
            answer: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct Evaluation<A> {
    pub answer: A,
    pub model: String,
    pub usage: Usage,
    pub request_id: Option<String>,
}

#[derive(Debug)]
pub struct BatchResponse {
    pub model: String,
    pub usage: Usage,
    pub request_id: Option<String>,
    answers: BTreeMap<String, WireAnswer>,
    scope: Arc<()>,
}

impl BatchResponse {
    pub(super) fn from_wire(
        wire: WireResponse,
        batch: &Batch,
        request_id: Option<String>,
    ) -> Result<Self> {
        if wire.model.trim().is_empty() || !wire.answers.keys().eq(batch.questions.keys()) {
            return Err(Error::InvalidResponse(
                "response must contain a model and exactly one answer per question",
            ));
        }
        for (name, question) in &batch.questions {
            wire.answers[name].validate(question)?;
        }
        Ok(Self {
            model: wire.model,
            usage: wire.usage,
            request_id,
            answers: wire.answers,
            scope: batch.scope.clone(),
        })
    }

    pub fn get<A: DeserializeOwned>(&self, key: &AnswerKey<A>) -> Result<A> {
        if !Arc::ptr_eq(&self.scope, &key.scope) {
            return Err(Error::InvalidRequest(
                "answer key belongs to a different batch",
            ));
        }
        let answer = self.answers.get(&key.name).ok_or(Error::InvalidRequest(
            "question was not part of this evaluation",
        ))?;
        let value = serde_json::to_value(answer).map_err(Error::Encode)?;
        serde_json::from_value(value).map_err(Error::Decode)
    }
}

#[derive(Deserialize)]
pub(super) struct WireResponse {
    model: String,
    answers: BTreeMap<String, WireAnswer>,
    usage: Usage,
}
