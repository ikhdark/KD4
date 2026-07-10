use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestUserInputQuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestUserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(rename = "isOther", default)]
    #[schemars(rename = "isOther")]
    #[ts(rename = "isOther")]
    pub is_other: bool,
    #[serde(rename = "isSecret", default)]
    #[schemars(rename = "isSecret")]
    #[ts(rename = "isSecret")]
    pub is_secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<RequestUserInputQuestionOption>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestUserInputArgs {
    pub questions: Vec<RequestUserInputQuestion>,
    #[serde(rename = "autoResolutionMs", skip_serializing_if = "Option::is_none")]
    #[schemars(rename = "autoResolutionMs")]
    pub auto_resolution_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for RequestUserInputArgs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawRequestUserInputQuestionOption {
            label: String,
            description: String,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawRequestUserInputQuestion {
            id: String,
            header: String,
            question: String,
            #[serde(rename = "isOther", default)]
            is_other: bool,
            #[serde(rename = "isSecret", default)]
            is_secret: bool,
            options: Option<Vec<RawRequestUserInputQuestionOption>>,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawRequestUserInputArgs {
            questions: Vec<RawRequestUserInputQuestion>,
            #[serde(rename = "autoResolutionMs")]
            auto_resolution_ms: Option<u64>,
        }

        let raw = RawRequestUserInputArgs::deserialize(deserializer)?;
        let questions = raw
            .questions
            .into_iter()
            .map(|question| RequestUserInputQuestion {
                id: question.id,
                header: question.header,
                question: question.question,
                is_other: question.is_other,
                is_secret: question.is_secret,
                options: question.options.map(|options| {
                    options
                        .into_iter()
                        .map(|option| RequestUserInputQuestionOption {
                            label: option.label,
                            description: option.description,
                        })
                        .collect()
                }),
            })
            .collect::<Vec<_>>();
        validate_request_user_input_questions(&questions).map_err(serde::de::Error::custom)?;

        Ok(Self {
            questions,
            auto_resolution_ms: raw.auto_resolution_ms,
        })
    }
}

fn validate_request_user_input_questions(
    questions: &[RequestUserInputQuestion],
) -> Result<(), &'static str> {
    if !(1..=3).contains(&questions.len()) {
        return Err("request_user_input requires 1 to 3 questions");
    }

    for question in questions {
        if question.header.chars().count() > 12 {
            return Err("question header must be 12 characters or fewer");
        }
        if let Some(options) = &question.options
            && !(2..=3).contains(&options.len())
        {
            return Err("question options must contain 2 to 3 choices");
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestUserInputAnswer {
    pub answers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestUserInputResponse {
    pub answers: HashMap<String, RequestUserInputAnswer>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestUserInputEvent {
    /// Responses API call id for the associated tool call, if available.
    pub call_id: String,
    /// Turn ID that this request belongs to.
    /// Uses `#[serde(default)]` for backwards compatibility.
    #[serde(default)]
    pub turn_id: String,
    pub questions: Vec<RequestUserInputQuestion>,
    #[serde(rename = "autoResolutionMs", skip_serializing_if = "Option::is_none")]
    #[schemars(rename = "autoResolutionMs")]
    pub auto_resolution_ms: Option<u64>,
}

#[cfg(test)]
#[path = "request_user_input_tests.rs"]
mod tests;
