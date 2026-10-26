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

