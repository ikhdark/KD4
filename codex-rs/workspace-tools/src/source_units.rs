//! Complete syntax units, never a heuristic brace scan through strings/comments.
use proc_macro2::Span;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use syn::spanned::Spanned;
use syn::visit::Visit;

pub fn enclosing(source: &str, line: usize) -> anyhow::Result<Value> {
    struct Finder {
        line: usize,
        best: Option<(usize, usize)>,
    }
    impl Finder {
        fn consider(&mut self, span: Span) {
            let candidate = (span.start().line, span.end().line);
            if candidate.0 <= self.line
                && self.line <= candidate.1
                && self
                    .best
                    .is_none_or(|old| candidate.1 - candidate.0 < old.1 - old.0)
            {
                self.best = Some(candidate);
            }
        }
    }
    impl<'ast> Visit<'ast> for Finder {
        fn visit_item(&mut self, item: &'ast syn::Item) {
            self.consider(item.span());
            syn::visit::visit_item(self, item);
        }
        fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
            self.consider(item.span());
            syn::visit::visit_impl_item(self, item);
        }
        fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
            self.consider(item.span());
            syn::visit::visit_trait_item(self, item);
        }
    }
    let syntax = syn::parse_file(source)?;
    let mut finder = Finder { line, best: None };
    finder.visit_file(&syntax);
    let (start, end) = finder
        .best
        .ok_or_else(|| anyhow::anyhow!("position is outside a parsed Rust item"))?;
    let text = source
        .split_inclusive('\n')
        .skip(start - 1)
        .take(end - start + 1)
        .collect::<String>();
    let hash = format!("{:x}", Sha256::digest(source.as_bytes()));
    Ok(
        json!({"start_line":start,"end_line":end,"text":text,"complete":true,
        "source_sha256":hash,"edit_handle":format!("@@ codex-range {start}:{end} sha256:{hash}")}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_method_survives_strings_and_nested_blocks() {
        let text = "struct A;\nimpl A {\n fn f() {\n  let s = \"}\";\n  if true { println!(\"{s}\"); }\n }\n fn g() {}\n}\n";
        let unit = enclosing(text, 4).unwrap();
        assert_eq!(unit["start_line"], 3);
        assert_eq!(unit["end_line"], 6);
        assert!(!unit["text"].as_str().unwrap().contains("fn g"));
        assert_eq!(unit["complete"], true);
        assert!(enclosing("fn broken(", 1).is_err());
    }
}
