use std::convert::TryFrom;
use std::time::Duration;
use std::time::Instant;

use rand::Rng as _;

use crate::frames::ALL_VARIANTS;
use crate::frames::FRAME_TICK_DEFAULT;
use crate::tui::FrameRequester;

/// Drives ASCII art animations shared across popups and onboarding widgets.
pub(crate) struct AsciiAnimation {
    request_frame: FrameRequester,
    variants: &'static [&'static [&'static str]],
    variant_idx: usize,
    frame_tick: Duration,
    start: Instant,
}

impl AsciiAnimation {
    pub(crate) fn new(request_frame: FrameRequester) -> Self {
        Self::with_variants(request_frame, ALL_VARIANTS, /*variant_idx*/ 0)
    }

    pub(crate) fn with_variants(
        request_frame: FrameRequester,
        variants: &'static [&'static [&'static str]],
        variant_idx: usize,
    ) -> Self {
        assert!(
            !variants.is_empty(),
            "AsciiAnimation requires at least one animation variant",
        );
        let clamped_idx = variant_idx.min(variants.len() - 1);
        Self {
            request_frame,
            variants,
            variant_idx: clamped_idx,
            frame_tick: FRAME_TICK_DEFAULT,
            start: Instant::now(),
        }
    }

    pub(crate) fn schedule_next_frame(&self) {
        let tick_ms = self.frame_tick.as_millis();
        if tick_ms == 0 {
            self.request_frame.schedule_frame();
            return;
        }
        let elapsed_ms = self.start.elapsed().as_millis();
        let rem_ms = elapsed_ms % tick_ms;
        let delay_ms = if rem_ms == 0 {
            tick_ms
        } else {
            tick_ms - rem_ms
        };
        if let Ok(delay_ms_u64) = u64::try_from(delay_ms) {
            self.request_frame
                .schedule_frame_in(Duration::from_millis(delay_ms_u64));
        } else {
            self.request_frame.schedule_frame();
        }
    }

    pub(crate) fn current_frame(&self) -> &'static str {
        let frames = self.frames();
        if frames.is_empty() {
            return "";
        }
        let tick_ms = self.frame_tick.as_millis();
        if tick_ms == 0 {
            return frames[0];
        }
        let elapsed_ms = self.start.elapsed().as_millis();
        let idx = ((elapsed_ms / tick_ms) % frames.len() as u128) as usize;
        frames[idx]
    }

    pub(crate) fn pick_random_variant(&mut self) -> bool {
        if self.variants.len() <= 1 {
            return false;
        }
        let mut rng = rand::rng();
        let mut next = self.variant_idx;
        while next == self.variant_idx {
            next = rng.random_range(0..self.variants.len());
        }
        self.variant_idx = next;
        self.request_frame.schedule_frame();
        true
    }

    fn frames(&self) -> &'static [&'static str] {
        self.variants[self.variant_idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_frame_advances_and_wraps() {
        let mut animation =
            AsciiAnimation::with_variants(FrameRequester::test_dummy(), &[&["first", "second"]], 0);
        animation.frame_tick = Duration::from_secs(60);
        for (elapsed_secs, expected) in [(30, "first"), (90, "second"), (150, "first")] {
            animation.start = Instant::now() - Duration::from_secs(elapsed_secs);
            assert_eq!(
                animation.current_frame(),
                expected,
                "elapsed: {elapsed_secs}s"
            );
        }
    }
}
