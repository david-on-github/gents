//! A proposer that asks a behavior on the served home: one rendered turn
//! per round, answered with exactly one fenced json block.

use anyhow::{anyhow, Result};
use gents::optimization::{Proposal, ProposalInput, Proposer};
use tokio::sync::Mutex;

use crate::commands::eval::init::draft::json_blocks;
use crate::commands::eval::init::turn::Turn;
use crate::commands::eval::render::percent;

pub(crate) struct BehaviorProposer<T> {
    turn: Mutex<T>,
}

impl<T: Turn + Send> BehaviorProposer<T> {
    pub(crate) fn new(turn: T) -> Self {
        Self {
            turn: Mutex::new(turn),
        }
    }
}

#[async_trait::async_trait]
impl<T: Turn + Send> Proposer for BehaviorProposer<T> {
    async fn propose(&self, input: ProposalInput) -> Result<Proposal> {
        let mut turn = self.turn.lock().await;
        let reply = turn.send(&render(&input)).await?;
        let problem = match parse_reply(&reply) {
            Ok(proposal) => return Ok(proposal),
            Err(problem) => problem,
        };
        let reply = turn
            .send(&format!(
                "Reply with exactly one fenced json block with text and rationale; {problem}"
            ))
            .await?;
        parse_reply(&reply)
            .map_err(|problem| anyhow!("the behavior's second reply is not a proposal: {problem}"))
    }
}

/// One user turn for a round, in a fixed order so a transcript is comparable
/// across rounds.
pub(crate) fn render(input: &ProposalInput) -> String {
    let mut out = format!(
        "Current instruction:\n{}\n\nFeedback from the train run:\n",
        fenced(&input.current_text)
    );
    let mut feedback: Vec<_> = input.feedback.iter().collect();
    feedback.sort_by(|a, b| a.check.cmp(&b.check));
    if feedback.is_empty() {
        out.push_str("- none\n");
    }
    for item in feedback {
        let score = item
            .score_bp
            .map_or_else(|| "no score".to_owned(), |bp| percent(Some(bp)));
        match item.feedback.as_deref().unwrap_or("no feedback") {
            text if text.contains('\n') => {
                out.push_str(&format!("- {}: {score} -\n{}\n", item.check, fenced(text)));
            }
            text => out.push_str(&format!("- {}: {score} - {text}\n", item.check)),
        }
    }
    if !input.rejections.is_empty() {
        out.push_str("\nRejected so far:\n");
        for rejection in &input.rejections {
            out.push_str(&format!(
                "- round {}, {}:\n{}\n",
                rejection.round,
                rejection.reason,
                fenced(&rejection.text)
            ));
        }
    }
    out.push_str(&format!(
        "\nRules:\n\
         - The text must differ from the current one.\n\
         - The text must be at most {} bytes.\n\
         - Keep the same audience and job.\n\
         - Do not add tools or claims the feedback does not support.\n\n\
         Reply with exactly one fenced json block: {{\"text\": ..., \"rationale\": ...}}\n",
        input.max_text_bytes
    ));
    out
}

/// `text` in a fence one backtick longer than its longest backtick run, so
/// a text holding its own fence cannot end the block early.
fn fenced(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}\n{text}\n{fence}")
}

fn parse_reply(reply: &str) -> Result<Proposal, String> {
    let blocks = json_blocks(reply);
    let block = match blocks.as_slice() {
        [block] => block,
        blocks => return Err(format!("the reply has {} fenced json blocks", blocks.len())),
    };
    serde_json::from_str(block)
        .map_err(|error| format!("the json block is not {{\"text\", \"rationale\"}}: {error}"))
}

#[cfg(test)]
mod tests {
    use gents::optimization::{CheckFeedback, ProposalInput, Proposer, Rejection};

    use super::{render, BehaviorProposer};
    use crate::commands::eval::init::turn::ScriptedTurn;

    fn input() -> ProposalInput {
        ProposalInput {
            round: 2,
            current_text: "Answer briefly.".into(),
            feedback: vec![
                CheckFeedback {
                    check: "tone".into(),
                    score_bp: None,
                    feedback: Some("too curt".into()),
                },
                CheckFeedback {
                    check: "captured_rows_count".into(),
                    score_bp: Some(2500),
                    feedback: None,
                },
            ],
            rejections: vec![Rejection {
                round: 1,
                text: "Answer.".into(),
                rationale: "shorter".into(),
                reason: "duplicate of the checkpoint".into(),
            }],
            max_text_bytes: 4096,
        }
    }

    const EXPECTED: &str = "Current instruction:
```
Answer briefly.
```

Feedback from the train run:
- captured_rows_count: 25.00% - no feedback
- tone: no score - too curt

Rejected so far:
- round 1, duplicate of the checkpoint:
```
Answer.
```

Rules:
- The text must differ from the current one.
- The text must be at most 4096 bytes.
- Keep the same audience and job.
- Do not add tools or claims the feedback does not support.

Reply with exactly one fenced json block: {\"text\": ..., \"rationale\": ...}
";

    const GOOD: &str = "Here you go.\n```json\n{\"text\": \"Answer in two sentences.\", \"rationale\": \"tone\"}\n```\n";

    #[test]
    fn the_turn_is_rendered_deterministically() {
        assert_eq!(render(&input()), EXPECTED);
    }

    #[test]
    fn a_fence_outgrows_the_longest_backtick_run_in_what_it_holds() {
        let mut input = input();
        input.current_text = "Reply in a ```json block.".into();
        input.feedback[0].feedback = Some("first line\nsecond ````` line".into());
        input.rejections[0].text = "Use ``` fences.".into();
        let rendered = render(&input);
        assert!(
            rendered.contains("Current instruction:\n````\nReply in a ```json block.\n````\n"),
            "{rendered}"
        );
        assert!(
            rendered
                .contains("- tone: no score -\n``````\nfirst line\nsecond ````` line\n``````\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("duplicate of the checkpoint:\n````\nUse ``` fences.\n````\n"),
            "{rendered}"
        );
    }

    #[test]
    fn no_feedback_renders_none() {
        let mut input = input();
        input.feedback.clear();
        assert!(
            render(&input).contains("Feedback from the train run:\n- none\n\nRejected"),
            "{}",
            render(&input)
        );
    }

    #[tokio::test]
    async fn a_good_reply_is_the_proposal() {
        let proposer = BehaviorProposer::new(ScriptedTurn::new([GOOD]));
        let proposal = proposer.propose(input()).await.unwrap();
        assert_eq!(proposal.text, "Answer in two sentences.");
        assert_eq!(proposal.rationale, "tone");
        let sent = proposer.turn.into_inner().sent;
        assert_eq!(sent, vec![EXPECTED.to_owned()]);
    }

    #[tokio::test]
    async fn a_reply_without_a_block_gets_one_corrective_turn() {
        let proposer = BehaviorProposer::new(ScriptedTurn::new(["Which tone?", GOOD]));
        let proposal = proposer.propose(input()).await.unwrap();
        assert_eq!(proposal.text, "Answer in two sentences.");
        let sent = proposer.turn.into_inner().sent;
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[1],
            "Reply with exactly one fenced json block with text and rationale; \
             the reply has 0 fenced json blocks"
        );
    }

    #[tokio::test]
    async fn two_bad_replies_are_an_error_naming_the_problem() {
        let two = format!("{GOOD}{GOOD}");
        let proposer = BehaviorProposer::new(ScriptedTurn::new(["no", two.as_str()]));
        let error = proposer.propose(input()).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("2 fenced json blocks"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn a_block_without_rationale_is_bad() {
        let missing = "```json\n{\"text\": \"Answer in two sentences.\"}\n```\n";
        let proposer = BehaviorProposer::new(ScriptedTurn::new([missing, GOOD]));
        proposer.propose(input()).await.unwrap();
        let sent = proposer.turn.into_inner().sent;
        assert!(sent[1].contains("missing field `rationale`"), "{}", sent[1]);
    }
}
