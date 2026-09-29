#![allow(clippy::expect_used)]

// Bounded integration shard. Preserve suite::v2::<module>::<test> IDs.
#[path = "suite"]
mod suite {
    #[path = "v2"]
    mod v2 {
        #[path = "auto_env.rs"]
        mod auto_env;
        #[path = "environment_add.rs"]
        mod environment_add;
        #[path = "environment_info.rs"]
        mod environment_info;
        #[path = "exec_server_test_support.rs"]
        mod exec_server_test_support;
        #[path = "executor_skills.rs"]
        mod executor_skills;
        #[path = "fs.rs"]
        mod fs;
        #[path = "mcp_tool.rs"]
        mod mcp_tool;
        #[path = "selected_environment.rs"]
        mod selected_environment;
        #[path = "thread_shell_command.rs"]
        mod thread_shell_command;
    }
}
