#![warn(uncommented_anonymous_literal_argument)]

fn main() {
    let line = "{\"type\":\"response_item\"}";
    let _ = line.starts_with('{');
    let _ = line.find("type");
    let parts = ["type", "response_item"];
    let _ = parts.join("\n");
    // Numeric arguments are not exempt as string/char literals are. Without
    // the external-crate filter these calls would require parameter comments.
    let mut buffer = String::from(line);
    buffer.truncate(0);
    let _ = line.splitn(2, ':');
}
