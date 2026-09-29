//! Typed question carried in an `ask` MailboxItem payload.
//!
//! A question adds no lifecycle: its item uses the `start_request` action, and
//! the answer is the ordinary interactive reply request whose content
//! [`MailboxQuestion::reply_content`] renders.

use serde::{Deserialize, Serialize};

pub const MAILBOX_QUESTION_VERSION: u32 = 1;
pub const MAILBOX_QUESTION_MIN_OPTIONS: usize = 2;
pub const MAILBOX_QUESTION_MAX_OPTIONS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct MailboxQuestionOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct MailboxQuestion {
    pub version: u32,
    pub prompt: String,
    pub options: Vec<MailboxQuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub allow_free_text: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct MailboxQuestionAnswer {
    #[serde(default)]
    pub option_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub free_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxQuestionError {
    Version,
    EmptyPrompt,
    OptionCount,
    EmptyOption,
    DuplicateOption(String),
    UnknownOption(String),
    MultipleSelected,
    RepeatedSelection(String),
    FreeTextNotAllowed,
    Empty,
    Decode(String),
}

impl std::fmt::Display for MailboxQuestionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version => write!(f, "question version must be {MAILBOX_QUESTION_VERSION}"),
            Self::EmptyPrompt => f.write_str("question prompt must not be empty"),
            Self::OptionCount => write!(
                f,
                "question needs {MAILBOX_QUESTION_MIN_OPTIONS}-{MAILBOX_QUESTION_MAX_OPTIONS} options"
            ),
            Self::EmptyOption => f.write_str("question option ids and labels must be non-empty"),
            Self::DuplicateOption(id) => write!(f, "question option id {id:?} is duplicated"),
            Self::UnknownOption(id) => write!(f, "answer names unknown option {id:?}"),
            Self::MultipleSelected => {
                f.write_str("answer selects more than one option of a single-select question")
            }
            Self::RepeatedSelection(id) => write!(f, "answer repeats option {id:?}"),
            Self::FreeTextNotAllowed => f.write_str("question does not accept a free-text answer"),
            Self::Empty => f.write_str("answer selects nothing"),
            Self::Decode(error) => write!(f, "payload is not a question: {error}"),
        }
    }
}

impl std::error::Error for MailboxQuestionError {}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

impl MailboxQuestion {
    pub fn validate(&self) -> Result<(), MailboxQuestionError> {
        if self.version != MAILBOX_QUESTION_VERSION {
            return Err(MailboxQuestionError::Version);
        }
        if self.prompt.trim().is_empty() {
            return Err(MailboxQuestionError::EmptyPrompt);
        }
        if !(MAILBOX_QUESTION_MIN_OPTIONS..=MAILBOX_QUESTION_MAX_OPTIONS)
            .contains(&self.options.len())
        {
            return Err(MailboxQuestionError::OptionCount);
        }
        let mut seen = std::collections::BTreeSet::new();
        for option in &self.options {
            if option.id.trim().is_empty() || option.label.trim().is_empty() {
                return Err(MailboxQuestionError::EmptyOption);
            }
            if !seen.insert(option.id.trim()) {
                return Err(MailboxQuestionError::DuplicateOption(option.id.clone()));
            }
        }
        Ok(())
    }

    /// Decode and validate a stored item payload.
    pub fn from_payload(payload: &str) -> Result<Self, MailboxQuestionError> {
        let question: Self = serde_json::from_str(payload)
            .map_err(|error| MailboxQuestionError::Decode(error.to_string()))?;
        question.validate()?;
        Ok(question)
    }

    /// The reply request's content: one line naming the item and each chosen
    /// option by label and id, then any free-text note.
    pub fn reply_content(
        &self,
        title: &str,
        answer: &MailboxQuestionAnswer,
    ) -> Result<String, MailboxQuestionError> {
        self.validate()?;
        let free_text = trimmed(answer.free_text.as_deref());
        if free_text.is_some() && !self.allow_free_text {
            return Err(MailboxQuestionError::FreeTextNotAllowed);
        }
        if answer.option_ids.len() > 1 && !self.multi_select {
            return Err(MailboxQuestionError::MultipleSelected);
        }
        let mut chosen = Vec::with_capacity(answer.option_ids.len());
        for id in &answer.option_ids {
            let option = self
                .options
                .iter()
                .find(|option| option.id.trim() == id.trim())
                .ok_or_else(|| MailboxQuestionError::UnknownOption(id.clone()))?;
            if chosen
                .iter()
                .any(|seen: &&MailboxQuestionOption| seen.id == option.id)
            {
                return Err(MailboxQuestionError::RepeatedSelection(id.clone()));
            }
            chosen.push(option);
        }
        let choice = match (chosen.is_empty(), free_text) {
            (true, None) => return Err(MailboxQuestionError::Empty),
            (true, Some(_)) => "Other".to_string(),
            (false, _) => chosen
                .iter()
                .map(|option| format!("{} ({})", option.label.trim(), option.id.trim()))
                .collect::<Vec<_>>()
                .join(", "),
        };
        let mut content = format!("Decision on {}: {choice}", title.trim());
        if let Some(note) = free_text {
            content.push_str("\n\n");
            content.push_str(note);
        }
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(multi_select: bool, allow_free_text: bool) -> MailboxQuestion {
        MailboxQuestion {
            version: MAILBOX_QUESTION_VERSION,
            prompt: "Which backend?".into(),
            options: vec![
                MailboxQuestionOption {
                    id: "local".into(),
                    label: "Local model".into(),
                    description: None,
                },
                MailboxQuestionOption {
                    id: "claude".into(),
                    label: "Claude".into(),
                    description: Some("Subscription".into()),
                },
            ],
            multi_select,
            allow_free_text,
        }
    }

    fn answer(ids: &[&str], free_text: Option<&str>) -> MailboxQuestionAnswer {
        MailboxQuestionAnswer {
            option_ids: ids.iter().map(|id| id.to_string()).collect(),
            free_text: free_text.map(str::to_owned),
        }
    }

    #[test]
    fn validation_bounds_options_and_identity() {
        assert!(question(false, false).validate().is_ok());
        let mut one = question(false, false);
        one.options.truncate(1);
        assert_eq!(one.validate(), Err(MailboxQuestionError::OptionCount));
        let mut five = question(false, false);
        for id in ["a", "b", "c"] {
            five.options.push(MailboxQuestionOption {
                id: id.into(),
                label: id.into(),
                description: None,
            });
        }
        assert_eq!(five.validate(), Err(MailboxQuestionError::OptionCount));
        let mut duplicate = question(false, false);
        duplicate.options[1].id = " local ".into();
        assert!(matches!(
            duplicate.validate(),
            Err(MailboxQuestionError::DuplicateOption(_))
        ));
        let mut blank = question(false, false);
        blank.options[0].label = " ".into();
        assert_eq!(blank.validate(), Err(MailboxQuestionError::EmptyOption));
        let mut prompt = question(false, false);
        prompt.prompt = "".into();
        assert_eq!(prompt.validate(), Err(MailboxQuestionError::EmptyPrompt));
        let mut version = question(false, false);
        version.version = 2;
        assert_eq!(version.validate(), Err(MailboxQuestionError::Version));
        assert!(MailboxQuestion::from_payload(r#"{"version":1,"prompt":"x"}"#).is_err());
        assert!(MailboxQuestion::from_payload("not json").is_err());
        let stored = serde_json::to_string(&question(true, true)).unwrap();
        assert_eq!(
            MailboxQuestion::from_payload(&stored).unwrap(),
            question(true, true)
        );
    }

    #[test]
    fn reply_content_names_choices_and_note() {
        assert_eq!(
            question(false, false)
                .reply_content("Backend", &answer(&["claude"], None))
                .unwrap(),
            "Decision on Backend: Claude (claude)"
        );
        assert_eq!(
            question(true, true)
                .reply_content("Backend", &answer(&["local", "claude"], Some(" both ")))
                .unwrap(),
            "Decision on Backend: Local model (local), Claude (claude)\n\nboth"
        );
        assert_eq!(
            question(false, true)
                .reply_content("Backend", &answer(&[], Some("Ollama")))
                .unwrap(),
            "Decision on Backend: Other\n\nOllama"
        );
    }

    #[test]
    fn reply_content_rejects_answers_outside_the_question() {
        let single = question(false, false);
        for (answer, error) in [
            (answer(&[], None), MailboxQuestionError::Empty),
            (
                answer(&["local", "claude"], None),
                MailboxQuestionError::MultipleSelected,
            ),
            (
                answer(&["gpt"], None),
                MailboxQuestionError::UnknownOption("gpt".into()),
            ),
            (
                answer(&["local"], Some("note")),
                MailboxQuestionError::FreeTextNotAllowed,
            ),
        ] {
            assert_eq!(single.reply_content("t", &answer), Err(error));
        }
        assert_eq!(
            question(true, false).reply_content("t", &answer(&["local", "local"], None)),
            Err(MailboxQuestionError::RepeatedSelection("local".into()))
        );
    }
}
