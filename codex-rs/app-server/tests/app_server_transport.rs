#![allow(clippy::expect_used)]

// Bounded integration shard. Preserve suite::v2::<module>::<test> IDs.
#[path = "suite"]
mod suite {
    #[path = "v2"]
    mod v2 {
        #[path = "command_exec.rs"]
        mod command_exec;
        #[path = "connection_handling_websocket.rs"]
        mod connection_handling_websocket;
        #[path = "mcp_server_elicitation.rs"]
        mod mcp_server_elicitation;
        #[path = "process_exec.rs"]
        mod process_exec;
        #[path = "thread_name_websocket.rs"]
        mod thread_name_websocket;
    }
}
