use core_test_support::responses;
use core_test_support::responses::SequenceResponder;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

/// Same as `create_mock_responses_server_sequence` but does not enforce an
/// expectation on the number of calls.
pub async fn create_mock_responses_server_sequence_unchecked(bodies: Vec<String>) -> MockServer {
    let server = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(SequenceResponder::sse(bodies))
        .mount(&server)
        .await;
    server
}

/// Create a mock responses API server that returns the same assistant message for every request.
pub async fn create_mock_responses_server_repeating_assistant(message: &str) -> MockServer {
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", message),
        responses::ev_completed("resp-1"),
    ]);
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(responses::sse_response(body))
        .mount(&server)
        .await;
    server
}
