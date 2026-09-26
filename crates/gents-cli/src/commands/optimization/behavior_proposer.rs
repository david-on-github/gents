//! A proposer that asks a behavior on the served home: one rendered turn
//! per round, answered with exactly one fenced json block.

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
- captured_rows_count: 2500 - no feedback
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
        assert!(sent[1].contains("rationale"), "{}", sent[1]);
    }
}
