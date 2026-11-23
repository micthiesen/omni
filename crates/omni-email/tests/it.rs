//! One integration-test binary for this crate; each module is a former test file.
mod activity_logs;
mod common;
mod dispatcher;
mod email_reprocess;
mod feedback;
mod html_to_text_golden;
mod link_metadata;
mod mcp_tools;
mod prod_copy;
mod retry_task;
mod routes;
mod sender_rules;
mod systemic_replay;
mod triage;
