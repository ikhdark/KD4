#![allow(clippy::expect_used)]

// Bounded integration shard. Preserve suite::v2::<module>::<test> IDs.
#[path = "suite"]
mod suite {
    #[path = "v2"]
    mod v2 {
        #[cfg(debug_assertions)]
        #[path = "remote_thread_store.rs"]
        mod remote_thread_store;
        #[path = "thread_archive.rs"]
        mod thread_archive;
        #[path = "thread_delete.rs"]
        mod thread_delete;
        #[path = "thread_inject_items.rs"]
        mod thread_inject_items;
        #[path = "thread_list.rs"]
        mod thread_list;
        #[path = "thread_loaded_list.rs"]
        mod thread_loaded_list;
        #[path = "thread_metadata_update.rs"]
        mod thread_metadata_update;
        #[path = "thread_read.rs"]
        mod thread_read;
        #[path = "thread_rollback.rs"]
        mod thread_rollback;
        #[path = "thread_settings_update.rs"]
        mod thread_settings_update;
        #[path = "thread_unarchive.rs"]
        mod thread_unarchive;
        #[path = "thread_unsubscribe.rs"]
        mod thread_unsubscribe;
    }
}
