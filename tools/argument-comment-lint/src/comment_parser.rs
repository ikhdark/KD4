use rustc_lexer::is_ident;

pub fn parse_argument_comment(text: &str) -> Option<&str> {
    let trimmed = text.trim_end();
    let comment_start = trimmed.rfind("/*")?;
    let comment = &trimmed[comment_start..];
    let name = comment.strip_prefix("/*")?.strip_suffix("*/")?;
    is_ident(name).then_some(name)
}

pub fn parse_argument_comment_prefix(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let comment = trimmed.strip_prefix("/*")?;
    let (name, _) = comment.split_once("*/")?;
    is_ident(name).then_some(name)
}
