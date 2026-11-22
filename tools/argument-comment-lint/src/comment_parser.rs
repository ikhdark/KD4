use rustc_lexer::FrontmatterAllowed;
use rustc_lexer::TokenKind;
use rustc_lexer::is_ident;
use rustc_lexer::tokenize;

pub fn parse_argument_comment(text: &str) -> Option<&str> {
    parse_argument_comment_within(text, 0)
}

pub fn parse_argument_comment_within(text: &str, first_byte: usize) -> Option<&str> {
    let trimmed = text.trim_end();
    if !trimmed.ends_with("*/") {
        return None;
    }
    let mut offset = 0;
    for token in tokenize(trimmed, FrontmatterAllowed::No) {
        let start = offset;
        offset += token.len as usize;
        if offset == trimmed.len()
            && start >= first_byte
            && matches!(token.kind, TokenKind::BlockComment { doc_style: None, terminated: true })
        {
            let name = &trimmed[start + 2..offset - 2];
            return is_ident(name).then_some(name);
        }
    }
    None
}

pub fn parse_argument_comment_prefix(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let comment = trimmed.strip_prefix("/*")?;
    let (name, _) = comment.split_once("*/")?;
    is_ident(name).then_some(name)
}
