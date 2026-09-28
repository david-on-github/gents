mod support;

#[path = "../src/lean_vocab_test/support.rs"]
mod lean_vocab_test;

#[path = "e2e_background/r6_background_recovery.rs"]
mod r6_background_recovery;
#[path = "e2e_background/r6_background_tools.rs"]
mod r6_background_tools;
#[path = "e2e_background/session_message_notification_order.rs"]
mod session_message_notification_order;
#[path = "e2e_background/session_message_turn_limit.rs"]
mod session_message_turn_limit;
