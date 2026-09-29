#![allow(clippy::expect_used)]

// Bounded integration shard. Preserve suite::v2::<module>::<test> IDs.
#[path = "suite"]
mod suite {
    #[path = "v2"]
    mod v2 {
        #[path = "account.rs"]
        mod account;
        #[path = "app_installed.rs"]
        mod app_installed;
        #[path = "app_list.rs"]
        mod app_list;
        #[path = "rate_limit_reset_credits.rs"]
        mod rate_limit_reset_credits;
        #[path = "rate_limits.rs"]
        mod rate_limits;
        #[path = "remote_control.rs"]
        mod remote_control;
    }
}
