//! Argument and envelope types for the R4c agent-facing background-work tools.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// Page sizes and read budgets below are protocol bounds of the agent-facing
// list/read tools, not per-behavior settings: every page reports where to
// resume, so a smaller or larger page changes cost, never what can be read.
const DEFAULT_LIST_LIMIT: u32 = 20;
const MAX_LIST_LIMIT: u32 = 50;

/// Token budget for `read_process`, using the local `chars ≈ 4 × tokens`
/// approximation.
pub(crate) const CHARS_PER_TOKEN_ESTIMATE: u32 = 4;
const DEFAULT_READ_PROCESS_MAX_TOKENS: u32 = 4096;
const MAX_READ_PROCESS_MAX_TOKENS: u32 = 65536;
const MIN_READ_PROCESS_MAX_TOKENS: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ListStatusFilter {
    #[default]
    Running,
    Terminal,
    All,
}

fn default_list_limit() -> u32 {
    DEFAULT_LIST_LIMIT
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ListBackgroundToolsArgs {
    #[serde(default)]
    pub(crate) status: ListStatusFilter,
    #[serde(default = "default_list_limit")]
    pub(crate) limit: u32,
}

impl ListBackgroundToolsArgs {
    pub(crate) fn validated_limit(&self) -> u32 {
        self.limit.clamp(1, MAX_LIST_LIMIT)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ReadToolOutputArgs {
    pub(crate) tool_call_id: String,
    /// Byte cursor into the captured combined output (default 0 = from start).
    /// Reads forward from `offset`; pages are contiguous (no head/tail drop).
    #[serde(default)]
    pub(crate) offset: u64,
    /// Token budget for the returned slice. The byte budget is derived via the
    /// codebase-wide `chars ≈ 4 × tokens` approximation.
    #[serde(default = "default_read_process_max_tokens")]
    pub(crate) max_tokens: u32,
}

fn default_read_process_max_tokens() -> u32 {
    DEFAULT_READ_PROCESS_MAX_TOKENS
}

impl ReadToolOutputArgs {
    pub(crate) fn validated_max_tokens(&self) -> u32 {
        self.max_tokens
            .clamp(MIN_READ_PROCESS_MAX_TOKENS, MAX_READ_PROCESS_MAX_TOKENS)
    }

    /// Byte budget derived from the token budget via the codebase-wide
    /// `chars ≈ 4 × tokens` approximation.
    pub(crate) fn validated_max_bytes(&self) -> usize {
        (self.validated_max_tokens() as usize).saturating_mul(CHARS_PER_TOKEN_ESTIMATE as usize)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ListBackgroundToolsEntry {
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    pub(crate) deployment_id: String,
    pub(crate) await_mode: String,
    pub(crate) status: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) last_update: DateTime<Utc>,
    pub(crate) stdout_bytes: u64,
    pub(crate) stderr_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ListBackgroundToolsResponse {
    pub(crate) read_at: DateTime<Utc>,
    pub(crate) truncated: bool,
    pub(crate) entries: Vec<ListBackgroundToolsEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReadToolOutputResponse {
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    pub(crate) status: String,
    /// The contiguous output slice starting at the requested `offset`. The
    /// captured stdout and stderr are concatenated in capture order behind a
    /// single byte cursor (stdout first, then a labeled `\n--- stderr ---\n`
    /// boundary, then stderr) so an orchestrator pages through ALL output
    /// gap-free with one cursor.
    pub(crate) output: String,
    /// Resume cursor = `offset` + bytes returned in `output`. Pass as `offset`
    /// on the next read to continue with no gap and no overlap.
    pub(crate) next_offset: u64,
    /// Earliest byte offset still available to read. 0 for finished tools
    /// (their full output is persisted, nothing is ever dropped). For a
    /// running tool the live buffer retains only the most recent output, so
    /// this can be > 0; if it exceeds your requested `offset`, the bytes in
    /// between were produced but evicted before you read them.
    pub(crate) first_available_offset: u64,
    /// Total bytes captured so far across the combined stdout/stderr buffer.
    pub(crate) total_bytes: u64,
    /// True when `next_offset < total_bytes` (more output remains to be paged).
    pub(crate) has_more: bool,
    /// True when the process has reached a terminal state (finished).
    pub(crate) exited: bool,
    pub(crate) exit_code: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_background_tools_args_round_trip_and_defaults() {
        let defaults: ListBackgroundToolsArgs =
            serde_json::from_value(json!({})).expect("parse defaults");
        assert_eq!(defaults.status, ListStatusFilter::Running);
        assert_eq!(defaults.limit, DEFAULT_LIST_LIMIT);

        let explicit: ListBackgroundToolsArgs = serde_json::from_value(json!({
            "status": "terminal",
            "limit": 51
        }))
        .expect("parse explicit");
        assert_eq!(explicit.status, ListStatusFilter::Terminal);
        assert_eq!(explicit.validated_limit(), MAX_LIST_LIMIT);
    }

    #[test]
    fn read_tool_output_args_round_trip_and_defaults() {
        let defaults: ReadToolOutputArgs = serde_json::from_value(json!({
            "tool_call_id": "tool-1"
        }))
        .expect("parse defaults");
        assert_eq!(defaults.tool_call_id, "tool-1");
        assert_eq!(defaults.offset, 0);
        assert_eq!(defaults.max_tokens, DEFAULT_READ_PROCESS_MAX_TOKENS);
        assert_eq!(
            defaults.validated_max_bytes(),
            DEFAULT_READ_PROCESS_MAX_TOKENS as usize * CHARS_PER_TOKEN_ESTIMATE as usize
        );

        let explicit: ReadToolOutputArgs = serde_json::from_value(json!({
            "tool_call_id": "tool-2",
            "offset": 512,
            "max_tokens": 1
        }))
        .expect("parse explicit");
        assert_eq!(explicit.tool_call_id, "tool-2");
        assert_eq!(explicit.offset, 512);
        // Token floor applies.
        assert_eq!(explicit.validated_max_tokens(), MIN_READ_PROCESS_MAX_TOKENS);
        assert_eq!(
            explicit.validated_max_bytes(),
            MIN_READ_PROCESS_MAX_TOKENS as usize * CHARS_PER_TOKEN_ESTIMATE as usize
        );

        let capped: ReadToolOutputArgs = serde_json::from_value(json!({
            "tool_call_id": "tool-3",
            "max_tokens": 999999999
        }))
        .expect("parse capped");
        assert_eq!(capped.validated_max_tokens(), MAX_READ_PROCESS_MAX_TOKENS);
    }
}
