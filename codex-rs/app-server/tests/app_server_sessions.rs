#![allow(clippy::expect_used)]

// Bounded integration shard. Preserve suite::v2::<module>::<test> IDs.
#[path = "suite"]
mod suite {
    #[path = "v2"]
    mod v2 {
        #[path = "analytics.rs"]
        mod analytics;
        #[path = "external_agent_config.rs"]
        mod external_agent_config;
        #[path = "request_validation.rs"]
        mod request_validation;
        #[path = "thread_fork.rs"]
        mod thread_fork;
        #[path = "thread_resume.rs"]
        mod thread_resume;
        #[path = "thread_start.rs"]
        mod thread_start;
        #[path = "thread_status.rs"]
        mod thread_status;
        #[path = "turn_start.rs"]
        mod turn_start;
    }
}
