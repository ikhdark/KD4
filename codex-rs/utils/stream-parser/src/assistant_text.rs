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
