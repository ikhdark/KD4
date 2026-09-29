#![allow(clippy::expect_used)]

// Bounded integration shard. Preserve suite::v2::<module>::<test> IDs.
#[path = "suite"]
mod suite {
    #[path = "v2"]
    mod v2 {
        #[path = "hooks_list.rs"]
        mod hooks_list;
        #[path = "marketplace_add.rs"]
        mod marketplace_add;
        #[path = "marketplace_remove.rs"]
        mod marketplace_remove;
        #[path = "marketplace_upgrade.rs"]
        mod marketplace_upgrade;
        #[path = "plugin_install.rs"]
        mod plugin_install;
        #[path = "plugin_list.rs"]
        mod plugin_list;
        #[path = "plugin_read.rs"]
        mod plugin_read;
        #[path = "plugin_share.rs"]
        mod plugin_share;
        #[path = "plugin_test_support.rs"]
        mod plugin_test_support;
        #[path = "plugin_uninstall.rs"]
        mod plugin_uninstall;
        #[path = "recommended_plugins.rs"]
        mod recommended_plugins;
        #[path = "skills_list.rs"]
        mod skills_list;
    }
}
