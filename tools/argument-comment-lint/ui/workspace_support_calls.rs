#![warn(argument_comment_mismatch)]
#![warn(uncommented_anonymous_literal_argument)]
// aux-build:core_test_support_base.rs

extern crate core_test_support_base;

mod support {
    pub use core_test_support_base::Server;
}

fn main() {
    let direct = core_test_support_base::Server;
    let reexported = support::Server;
    direct.wait_for_request_count(/*timeout*/ 2);
    reexported.wait_for_request_count(/*timeout*/ 2);
    direct.wait_for_request_count(2);
    reexported.wait_for_request_count(2);
    direct.wait_for_request_count(/*count*/ 2);
    reexported.wait_for_request_count(/*count*/ 2);
    let _ = direct.enabled(false);
}
