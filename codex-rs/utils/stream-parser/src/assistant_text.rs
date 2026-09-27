use crate::ProposedPlanParser;
use crate::ProposedPlanSegment;
use crate::StreamTextChunk;
use crate::StreamTextParser;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssistantTextChunk {
    pub visible_text: String,
    pub plan_segments: Vec<ProposedPlanSegment>,
}

impl AssistantTextChunk {
    pub fn is_empty(&self) -> bool {
        self.visible_text.is_empty() && self.plan_segments.is_empty()
    }
}

impl From<StreamTextChunk<ProposedPlanSegment>> for AssistantTextChunk {
    fn from(chunk: StreamTextChunk<ProposedPlanSegment>) -> Self {
        Self {
            visible_text: chunk.visible_text,
            plan_segments: chunk.extracted,
        }
    }
}

/// Parses streamed assistant text. In plan mode, strips `<proposed_plan>` blocks and emits plan
/// segments; otherwise text is visible exactly as streamed.
#[derive(Debug, Default)]
pub struct AssistantTextStreamParser {
    plan_mode: bool,
    plan: ProposedPlanParser,
}

impl AssistantTextStreamParser {
    pub fn new(plan_mode: bool) -> Self {
        Self {
            plan_mode,
            ..Self::default()
        }
    }

    pub fn push_str(&mut self, chunk: &str) -> AssistantTextChunk {
        if !self.plan_mode {
            return AssistantTextChunk {
                visible_text: chunk.to_string(),
                ..AssistantTextChunk::default()
            };
        }
        self.plan.push_str(chunk).into()
    }

    pub fn finish(&mut self) -> AssistantTextChunk {
        if !self.plan_mode {
            return AssistantTextChunk::default();
        }
        self.plan.finish().into()
    }
}

#[cfg(test)]
mod tests {
    use super::AssistantTextStreamParser;
    use crate::ProposedPlanSegment;
    use pretty_assertions::assert_eq;

    #[test]
    fn releases_malformed_tag_prefixes_as_soon_as_whitespace_disproves_them() {
        for (tag, in_plan) in [("<proposed_plan>", false), ("</proposed_plan>", true)] {
            for end in 1..tag.len() {
                for whitespace in [" ", "\t", "\r", "\u{2003}"] {
                    let mut parser = AssistantTextStreamParser::new(true);
                    if in_plan {
                        assert_eq!(
                            parser.push_str("<proposed_plan>\n").plan_segments,
                            vec![ProposedPlanSegment::ProposedPlanStart]
                        );
                    }
                    assert!(parser.push_str(&tag[..end]).is_empty());
                    let text = format!("{}{whitespace}", &tag[..end]);
                    let parsed = parser.push_str(whitespace);
                    if in_plan {
                        assert_eq!(parsed.visible_text, "");
                        assert_eq!(
                            parsed.plan_segments,
                            vec![ProposedPlanSegment::ProposedPlanDelta(text)]
                        );
                        assert_eq!(
                            parser.finish().plan_segments,
                            vec![ProposedPlanSegment::ProposedPlanEnd]
                        );
                    } else {
                        assert_eq!(parsed.visible_text, text);
                        assert_eq!(
                            parsed.plan_segments,
                            vec![ProposedPlanSegment::Normal(text)]
                        );
                        assert!(parser.finish().is_empty());
                    }
                    assert!(parser.finish().is_empty());
                    assert_eq!(parser.push_str("reused").visible_text, "reused");
                }
            }
        }
    }

    #[test]
    fn preserves_plan_content_across_every_unicode_chunk_boundary() {
        let source = "Intro 雪\r\n\u{2003}<proposed_plan> \t\r\n<proposed \u{2003}\n- 🦀\n</proposed_plan> \u{2003}\r\nOutro <proposed";
        let assert_chunks = |chunks: Vec<&str>| {
            let mut parser = AssistantTextStreamParser::new(true);
            let mut visible = String::new();
            let mut plan = String::new();
            let mut active = false;
            let mut starts = 0;
            let mut ends = 0;
            let mut outputs: Vec<_> = chunks
                .into_iter()
                .map(|chunk| parser.push_str(chunk))
                .collect();
            outputs.push(parser.finish());
            for output in outputs {
                visible.push_str(&output.visible_text);
                for segment in output.plan_segments {
                    match segment {
                        ProposedPlanSegment::Normal(_) => assert!(!active),
                        ProposedPlanSegment::ProposedPlanStart => {
                            assert!(!active);
                            active = true;
                            starts += 1;
                        }
                        ProposedPlanSegment::ProposedPlanDelta(text) => {
                            assert!(active);
                            plan.push_str(&text);
                        }
                        ProposedPlanSegment::ProposedPlanEnd => {
                            assert!(active);
                            active = false;
                            ends += 1;
                        }
                    }
                }
            }
            assert_eq!(visible, "Intro 雪\r\nOutro <proposed");
            assert_eq!(plan, "<proposed \u{2003}\n- 🦀\n");
            assert_eq!((starts, ends, active), (1, 1, false));
            assert!(parser.finish().is_empty());
        };
        for split in (0..=source.len()).filter(|&i| source.is_char_boundary(i)) {
            assert_chunks(vec![&source[..split], &source[split..]]);
        }
        assert_chunks(
            source
                .char_indices()
                .map(|(i, ch)| &source[i..i + ch.len_utf8()])
                .collect(),
        );
    }

    #[test]
    fn passes_literal_markup_through_outside_plan_mode() {
        let mut parser = AssistantTextStreamParser::new(/*plan_mode*/ false);

        let seeded = parser.push_str("hello <oai-mem-citation>doc");
        let parsed = parser.push_str("1</oai-mem-citation> world\n<proposed_plan>\n");
        let tail = parser.finish();

        assert_eq!(seeded.visible_text, "hello <oai-mem-citation>doc");
        assert_eq!(
            parsed.visible_text,
            "1</oai-mem-citation> world\n<proposed_plan>\n"
        );
        assert!(parsed.plan_segments.is_empty());
        assert!(tail.is_empty());
    }

    #[test]
    fn parses_plan_segments_across_delta_boundaries() {
        let mut parser = AssistantTextStreamParser::new(/*plan_mode*/ true);

        let seeded = parser.push_str("Intro\n<proposed");
        let parsed = parser.push_str("_plan>\n- step\n");
        let tail = parser.push_str("</proposed_plan>\nOutro");
        let finish = parser.finish();

        assert_eq!(seeded.visible_text, "Intro\n");
        assert_eq!(
            seeded.plan_segments,
            vec![ProposedPlanSegment::Normal("Intro\n".to_string())]
        );
        assert_eq!(parsed.visible_text, "");
        assert_eq!(
            parsed.plan_segments,
            vec![
                ProposedPlanSegment::ProposedPlanStart,
                ProposedPlanSegment::ProposedPlanDelta("- step\n".to_string()),
            ]
        );
        assert_eq!(tail.visible_text, "Outro");
        assert_eq!(
            tail.plan_segments,
            vec![
                ProposedPlanSegment::ProposedPlanEnd,
                ProposedPlanSegment::Normal("Outro".to_string()),
            ]
        );
        assert!(finish.is_empty());
    }
}
