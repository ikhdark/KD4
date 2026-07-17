//! Two-region streaming controllers for agent messages and proposed plans.
//!
//! Each stream partitions rendered markdown into a *stable region* (committed
//! to scrollback via the animation queue in `StreamState`) and a *tail region*
//! (mutable, displayed in the active-cell slot as a transient stream-tail cell).
//!
//! `StreamCore` owns the shared bookkeeping: source accumulation, re-rendering,
//! stable/tail partitioning, commit-animation queue management, and terminal
//! resize handling.  `StreamController` and `PlanStreamController` are thin
//! wrappers that add only their `emit()` styling and finalize return types.
//!
//! ## Table holdback
//!
//! Table rendering is inherently non-incremental: adding a new row can change
//! every column's width and reshape all prior rows.  The holdback mechanism
//! (`table_holdback_state`) detects pipe-table patterns (header + delimiter
//! pair) in the accumulated source and keeps content from the table header
//! onward as mutable tail until the stream finalizes. Holdback is enabled for
//! agent and proposed-plan streams. Lines in `Outside` and `Markdown` fence
//! contexts are scanned; lines inside non-markdown fences are skipped.
//!
//! ## Resize handling
//!
//! On terminal width change, `StreamCore::set_width` re-renders at the new
//! width and rebuilds the queued stable region from the current emitted line
//! count. This intentionally avoids byte-level remap complexity while the
//! stream is active; finalized content is canonicalized by transcript
//! consolidation into source-backed markdown cells.
//!
//! ## Invariants
//!
//! - `emitted_stable_len <= enqueued_stable_len <= rendered_lines.len()`.
//! - `raw_source` is append-only until `reset()`; never modified mid-stream.
//! - The committed tail starts at `enqueued_stable_len`; the bounded provisional partial suffix
//!   follows it and never enters the stable queue.
//! - During confirmed table streaming, only lines from the table header onward
//!   are forced into tail; pre-table lines may remain stable.

use crate::history_cell::HistoryCell;
use crate::history_cell::HistoryRenderMode;
use crate::history_cell::raw_lines_from_source;
use crate::history_cell::{self};
use crate::markdown::render_markdown_agent_with_links_and_cwd;
use crate::style::proposed_plan_style;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::plain_hyperlink_lines;
use crate::terminal_hyperlinks::prefix_hyperlink_lines;
use ratatui::prelude::Stylize;
use ratatui::text::Line;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use super::StreamState;
use super::table_holdback::TableHoldbackScanner;
use super::table_holdback::TableHoldbackState;
#[cfg(test)]
use super::table_holdback::table_holdback_state;

/// Maximum delay between provisional renders of one unterminated source line.
///
/// The first suffix is rendered immediately; only follow-up mutations are coalesced, and the
/// commit animation tick wakes the controller again within the next frame.
const LIVE_PARTIAL_RENDER_DEBOUNCE: Duration = Duration::from_millis(16);

/// Maximum raw source bytes reparsed for one mutable streaming Markdown window.
const MUTABLE_MARKDOWN_SOURCE_MAX_BYTES: usize = 16 * 1024;

/// Maximum rendered rows retained in the replaceable provisional tail.
const LIVE_PARTIAL_MAX_RENDERED_LINES: usize = 64;

// ---------------------------------------------------------------------------
// StreamCore — shared bookkeeping for both stream controllers
// ---------------------------------------------------------------------------

/// Shared state and logic for the two-region streaming model.
///
/// Both [`StreamController`] (agent messages) and [`PlanStreamController`]
/// (proposed plans) delegate their core bookkeeping here: source
/// accumulation, re-rendering, stable/tail partitioning, commit-animation
/// queue management, and terminal resize handling.
///
/// The wrapping controllers add only their own `emit()` styling and
/// finalize return types.
struct StreamCore {
    state: StreamState,
    /// Current rendering width (columns available for markdown content).
    width: Option<usize>,
    /// Accumulated raw markdown source for the current stream.
    raw_source: String,
    /// Retained completed render plus the bounded mutable suffix at `width`.
    rendered_lines: Vec<HyperlinkLine>,
    /// Lines enqueued into the commit-animation queue.
    enqueued_stable_len: usize,
    /// Lines actually emitted to scrollback.
    emitted_stable_len: usize,
    /// Session cwd used to keep local file-link display stable during stream re-renders.
    cwd: PathBuf,
    render_mode: HistoryRenderMode,
    /// Cached rendered line count for prefix-before-table keyed by source start and width.
    stable_prefix_len_cache: Option<StablePrefixLenCache>,
    /// Incremental holdback scanner state for append-only source updates.
    holdback_scanner: TableHoldbackScanner,
    /// Bounded provisional render of the collector's unterminated source suffix.
    live_partial_lines: Vec<HyperlinkLine>,
    /// Whether a follow-up partial render is waiting for the debounce boundary.
    live_partial_dirty: bool,
    /// Timestamp of the most recent provisional suffix render.
    last_live_partial_render_at: Option<Instant>,
    /// Byte offset where the currently replaceable completed-source render begins.
    mutable_source_start: usize,
    /// Rendered-line offset corresponding to `mutable_source_start`.
    mutable_render_start: usize,
    /// Earliest rendered row that must remain out of the stable queue after a bounded rotation.
    forced_tail_render_start: Option<usize>,
    /// Largest completed-source window parsed by the incremental streaming path.
    #[cfg(test)]
    max_streaming_render_source_len: usize,
}

struct StablePrefixLenCache {
    /// Byte offset of the candidate table/header start in `raw_source`.
    source_start: usize,
    /// Width that produced `stable_prefix_len`.
    width: Option<usize>,
    /// Rendered line count for `raw_source[..source_start]` at `width`.
    ///
    /// The streaming controller uses this to avoid repeatedly re-rendering the
    /// same stable prefix while a live table tail is still mutating.
    stable_prefix_len: usize,
}

impl StreamCore {
    fn new(width: Option<usize>, cwd: &Path, render_mode: HistoryRenderMode) -> Self {
        Self {
            state: StreamState::new(width, cwd),
            width,
            raw_source: String::with_capacity(1024),
            rendered_lines: Vec::with_capacity(64),
            enqueued_stable_len: 0,
            emitted_stable_len: 0,
            cwd: cwd.to_path_buf(),
            render_mode,
            stable_prefix_len_cache: None,
            holdback_scanner: TableHoldbackScanner::new(),
            live_partial_lines: Vec::new(),
            live_partial_dirty: false,
            last_live_partial_render_at: None,
            mutable_source_start: 0,
            mutable_render_start: 0,
            forced_tail_render_start: None,
            #[cfg(test)]
            max_streaming_render_source_len: 0,
        }
    }

    /// Push a streaming delta and enqueue any newly-stable rendered lines.
    ///
    /// Only newline-terminated source is committed into canonical `raw_source`. The collector's
    /// unterminated suffix is rendered separately as a bounded, replaceable live tail so it can be
    /// shown immediately without entering the stable queue.
    fn push_delta(&mut self, delta: &str) -> bool {
        self.push_delta_at(delta, Instant::now())
    }

    fn push_delta_at(&mut self, delta: &str, now: Instant) -> bool {
        let received_delta = !delta.is_empty();
        if received_delta {
            self.state.has_seen_delta = true;
        }
        self.state.collector.push_delta(delta);

        let mut enqueued = false;
        let mut committed_source = false;
        if delta.contains('\n')
            && let Some(source) = self.state.collector.commit_complete_source()
        {
            let previous_source_len = self.raw_source.len();
            self.raw_source.push_str(&source);
            self.holdback_scanner.push_source_chunk(&source);
            self.recompute_streaming_render(previous_source_len);
            enqueued = self.sync_stable_queue();
            committed_source = true;
        }

        if received_delta {
            self.live_partial_dirty = true;
            let first_partial_render = self.last_live_partial_render_at.is_none();
            self.refresh_live_partial(now, committed_source || first_partial_render);
        }

        // A non-empty delta either changed the provisional tail immediately or left it dirty for
        // the next frame. In both cases the chat widget must start or keep the commit animation so
        // the live tail becomes visible without waiting for a newline.
        enqueued || received_delta
    }

    /// Drain the collector, render the final source snapshot, and return lines not yet emitted.
    ///
    /// This intentionally re-renders from the full raw source instead of
    /// trying to stitch together queued stable lines and the current tail. The
    /// final render is the canonical transcript representation used for
    /// consolidation, so callers that skip `reset()` can accidentally replay a
    /// finished stream into the next answer.
    fn finalize_remaining(&mut self) -> Vec<HyperlinkLine> {
        let remainder_source = self.state.collector.finalize_and_drain_source();
        if !remainder_source.is_empty() {
            self.raw_source.push_str(&remainder_source);
            self.holdback_scanner.push_source_chunk(&remainder_source);
        }
        let rendered = self.render_source(&self.raw_source);
        if self.emitted_stable_len >= rendered.len() {
            Vec::new()
        } else {
            rendered[self.emitted_stable_len..].to_vec()
        }
    }

    /// Step animation: dequeue one line, update the emitted count.
    fn tick(&mut self) -> Vec<HyperlinkLine> {
        self.refresh_live_partial(Instant::now(), /*force*/ false);
        let step = self.state.step();
        self.emitted_stable_len += step.len();
        step
    }

    /// Batch drain: dequeue up to `max_lines`, update the emitted count.
    fn tick_batch(&mut self, max_lines: usize) -> Vec<HyperlinkLine> {
        self.refresh_live_partial(Instant::now(), /*force*/ false);
        if max_lines == 0 {
            return Vec::new();
        }
        let step = self.state.drain_n(max_lines);
        if step.is_empty() {
            return step;
        }
        self.emitted_stable_len += step.len();
        step
    }

    // Trivial StreamCore accessors inlined — called on every animation tick
    // and render frame during active streaming.

    #[inline]
    fn is_idle(&self) -> bool {
        self.state.is_idle() && !self.live_partial_dirty
    }

    #[inline]
    fn queued_lines(&self) -> usize {
        self.state.queued_len()
    }

    #[inline]
    fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.state.oldest_queued_age(now)
    }

    /// Lines that belong to the mutable tail, not yet queued for stable commit.
    ///
    /// The tail starts at `enqueued_stable_len`, so this returns the portion
    /// of the current render snapshot that is still allowed to change without
    /// violating scrollback ordering. If callers were to derive the tail from
    /// `emitted_stable_len` instead, queued-but-not-yet-emitted lines could
    /// reappear in the active cell and duplicate content on screen. The bounded
    /// provisional partial render is appended after that committed tail slice.
    #[inline]
    fn current_tail_lines(&self) -> Vec<HyperlinkLine> {
        let start = self.enqueued_stable_len.min(self.rendered_lines.len());
        let bounded_start = start.max(
            self.rendered_lines
                .len()
                .saturating_sub(LIVE_PARTIAL_MAX_RENDERED_LINES),
        );
        let mut lines = self.rendered_lines[bounded_start..].to_vec();
        lines.extend(self.live_partial_lines.iter().cloned());
        if lines.len() > LIVE_PARTIAL_MAX_RENDERED_LINES {
            lines.drain(..lines.len() - LIVE_PARTIAL_MAX_RENDERED_LINES);
        }
        lines
    }

    #[inline]
    fn has_tail(&self) -> bool {
        self.enqueued_stable_len < self.rendered_lines.len()
            || !self.state.collector.pending_source().is_empty()
    }

    /// Update rendering width and rebuild queued stable lines for the new layout.
    ///
    /// Re-renders once at the new width and rebuilds queue state from the
    /// current emitted line count.
    ///
    /// Resize is the point where source-backed rendering matters most:
    /// previously emitted prose must stay in scrollback order, while any live
    /// table tail is free to reshape at the new width. This method preserves
    /// that split without attempting byte-for-byte line remapping.
    fn set_width(&mut self, width: Option<usize>) {
        if self.width == width {
            return;
        }
        let had_pending_queue = self.state.queued_len() > 0;
        let had_live_tail = self.has_tail();
        self.width = width;
        self.state.collector.set_width(width);
        if !self.state.collector.pending_source().is_empty() {
            self.live_partial_dirty = true;
            self.refresh_live_partial(Instant::now(), /*force*/ true);
        }
        if self.raw_source.is_empty() {
            return;
        }

        self.recompute_full_render();
        self.emitted_stable_len = self.emitted_stable_len.min(self.rendered_lines.len());
        if had_pending_queue
            && self.emitted_stable_len == self.rendered_lines.len()
            && self.emitted_stable_len > 0
        {
            // If wrapped remainder compresses into fewer lines at the new width,
            // keep at least one line un-emitted so pre-resize pending content is
            // not skipped permanently.
            self.emitted_stable_len -= 1;
        }
        self.state.clear_queue();
        if self.emitted_stable_len > 0 && !had_pending_queue && !had_live_tail {
            // Avoid replaying already-emitted content after resize when no
            // stable lines were waiting in the queue and there was no mutable
            // tail to preserve.
            self.enqueued_stable_len = self.rendered_lines.len();
            return;
        }
        self.rebuild_stable_queue_from_render();
    }

    /// Clear all accumulated state for current stream.
    fn reset(&mut self) {
        self.state.clear();
        self.raw_source.clear();
        self.rendered_lines.clear();
        self.enqueued_stable_len = 0;
        self.emitted_stable_len = 0;
        self.stable_prefix_len_cache = None;
        self.holdback_scanner.reset();
        self.live_partial_lines.clear();
        self.live_partial_dirty = false;
        self.last_live_partial_render_at = None;
        self.mutable_source_start = 0;
        self.mutable_render_start = 0;
        self.forced_tail_render_start = None;
        #[cfg(test)]
        {
            self.max_streaming_render_source_len = 0;
        }
    }

    fn render_source(&self, source: &str) -> Vec<HyperlinkLine> {
        match self.render_mode {
            HistoryRenderMode::Rich => render_markdown_agent_with_links_and_cwd(
                source,
                self.width,
                Some(self.cwd.as_path()),
            ),
            HistoryRenderMode::Raw => plain_hyperlink_lines(raw_lines_from_source(source)),
        }
    }

    /// Re-render only the bounded completed-source suffix that is still allowed to change.
    ///
    /// Once the active source window would exceed the byte budget, the existing render is retained
    /// as canonical completed output and the window rotates to the newly committed source. Any
    /// output produced after that rotation remains conservatively held out of the stable queue;
    /// finalization still performs one full canonical render from `raw_source`.
    fn recompute_streaming_render(&mut self, previous_source_len: usize) {
        debug_assert!(self.mutable_source_start <= previous_source_len);
        debug_assert!(previous_source_len <= self.raw_source.len());

        if self.raw_source.len().saturating_sub(self.mutable_source_start)
            <= MUTABLE_MARKDOWN_SOURCE_MAX_BYTES
        {
            self.rendered_lines.truncate(self.mutable_render_start);
            let source = self.raw_source[self.mutable_source_start..].to_string();
            let rendered = self.render_streaming_source(&source);
            self.rendered_lines.extend(rendered);
            return;
        }

        let conservative_tail_start = self
            .rendered_lines
            .len()
            .saturating_sub(LIVE_PARTIAL_MAX_RENDERED_LINES);
        self.forced_tail_render_start = Some(
            self.forced_tail_render_start
                .map_or(conservative_tail_start, |start| {
                    start.min(conservative_tail_start)
                }),
        );

        // The previous completed render is already canonical for its source snapshot. Retain it
        // byte-for-byte and rotate only the newly appended source through bounded parser windows.
        self.mutable_source_start = previous_source_len;
        self.mutable_render_start = self.rendered_lines.len();
        let appended_source = self.raw_source[previous_source_len..].to_string();
        self.append_bounded_streaming_source(previous_source_len, &appended_source);
    }

    /// Re-render the entire canonical source for explicit resize/render-mode changes.
    fn recompute_full_render(&mut self) {
        self.rendered_lines = self.render_source(&self.raw_source);
        if self.raw_source.len() <= MUTABLE_MARKDOWN_SOURCE_MAX_BYTES {
            self.mutable_source_start = 0;
            self.mutable_render_start = 0;
            self.forced_tail_render_start = None;
        } else {
            self.mutable_source_start = self.raw_source.len();
            self.mutable_render_start = self.rendered_lines.len();
            self.forced_tail_render_start = Some(
                self.rendered_lines
                    .len()
                    .saturating_sub(LIVE_PARTIAL_MAX_RENDERED_LINES),
            );
        }
    }

    /// Render appended completed source in parser calls that never exceed the mutable byte budget.
    fn append_bounded_streaming_source(&mut self, source_start: usize, source: &str) {
        let mut consumed = 0usize;
        while source.len().saturating_sub(consumed) > MUTABLE_MARKDOWN_SOURCE_MAX_BYTES {
            let remaining = &source[consumed..];
            let split = bounded_complete_source_chunk_end(remaining);
            if split == 0 {
                // One committed logical line exceeds the parser budget. Keep all canonical bytes
                // in `raw_source`, but advance to a UTF-8-safe suffix so the live tail still shows
                // the newest portion without sending an oversized input to the parser.
                let line_end = remaining.find('\n').map_or(remaining.len(), |idx| idx + 1);
                let mut skipped = line_end.saturating_sub(MUTABLE_MARKDOWN_SOURCE_MAX_BYTES);
                while !remaining.is_char_boundary(skipped) {
                    skipped += 1;
                }
                consumed += skipped;
                self.mutable_source_start = source_start + consumed;
                self.mutable_render_start = self.rendered_lines.len();
                continue;
            }

            let rendered = self.render_streaming_source(&remaining[..split]);
            self.rendered_lines.extend(rendered);
            consumed += split;
            self.mutable_source_start = source_start + consumed;
            self.mutable_render_start = self.rendered_lines.len();
        }

        self.mutable_source_start = source_start + consumed;
        self.mutable_render_start = self.rendered_lines.len();
        let rendered = self.render_streaming_source(&source[consumed..]);
        self.rendered_lines.extend(rendered);
    }

    fn render_streaming_source(&mut self, source: &str) -> Vec<HyperlinkLine> {
        debug_assert!(source.len() <= MUTABLE_MARKDOWN_SOURCE_MAX_BYTES);
        #[cfg(test)]
        {
            self.max_streaming_render_source_len =
                self.max_streaming_render_source_len.max(source.len());
        }
        self.render_source(source)
    }

    /// Re-render the collector's provisional suffix when its bounded debounce permits.
    ///
    /// The canonical newline-stable prefix is never included in this parse. Very long
    /// unterminated lines are clipped at a UTF-8 boundary before rendering, and only a bounded
    /// number of rendered rows remain mutable in the active cell.
    fn refresh_live_partial(&mut self, now: Instant, force: bool) {
        if self.state.collector.pending_source().is_empty() {
            self.live_partial_lines.clear();
            self.live_partial_dirty = false;
            self.last_live_partial_render_at = None;
            return;
        }
        if !self.live_partial_dirty {
            return;
        }
        let debounce_elapsed = self.last_live_partial_render_at.is_none_or(|last_render| {
            now.saturating_duration_since(last_render) >= LIVE_PARTIAL_RENDER_DEBOUNCE
        });
        if !force && !debounce_elapsed {
            return;
        }

        let source = bounded_live_partial_source(self.state.collector.pending_source()).to_string();
        let mut rendered = self.render_streaming_source(&source);
        if rendered.len() > LIVE_PARTIAL_MAX_RENDERED_LINES {
            rendered.drain(..rendered.len() - LIVE_PARTIAL_MAX_RENDERED_LINES);
        }
        self.live_partial_lines = rendered;
        self.live_partial_dirty = false;
        self.last_live_partial_render_at = Some(now);
    }

    fn set_render_mode(&mut self, render_mode: HistoryRenderMode) {
        if self.render_mode == render_mode {
            return;
        }

        let had_pending_queue = self.state.queued_len() > 0;
        let had_live_tail = self.has_tail();
        self.render_mode = render_mode;
        if !self.state.collector.pending_source().is_empty() {
            self.live_partial_dirty = true;
            self.refresh_live_partial(Instant::now(), /*force*/ true);
        }
        if self.raw_source.is_empty() {
            return;
        }

        self.recompute_full_render();
        self.emitted_stable_len = self.emitted_stable_len.min(self.rendered_lines.len());
        if had_pending_queue
            && self.emitted_stable_len == self.rendered_lines.len()
            && self.emitted_stable_len > 0
        {
            self.emitted_stable_len -= 1;
        }
        self.state.clear_queue();
        if self.emitted_stable_len > 0 && !had_pending_queue && !had_live_tail {
            self.enqueued_stable_len = self.rendered_lines.len();
            return;
        }
        self.rebuild_stable_queue_from_render();
    }

    /// Compute how many rendered lines should be in the stable region.
    fn compute_target_stable_len(&mut self) -> usize {
        let tail_budget = self.active_tail_budget_lines();
        self.rendered_lines
            .len()
            .saturating_sub(tail_budget)
            .max(self.emitted_stable_len)
    }

    /// Advance `enqueued_stable_len` toward the target stable boundary and enqueue any
    /// newly-stable lines. Returns `true` if new lines were enqueued.
    fn sync_stable_queue(&mut self) -> bool {
        let target_stable_len = self.compute_target_stable_len();

        // A structural rewrite moved the stable boundary backward into enqueue-but-unemitted
        // lines. Rebuild queue from the latest snapshot.
        if target_stable_len < self.enqueued_stable_len {
            self.state.clear_queue();
            if self.emitted_stable_len < target_stable_len {
                self.state.enqueue(
                    self.rendered_lines[self.emitted_stable_len..target_stable_len].to_vec(),
                );
            }
            self.enqueued_stable_len = target_stable_len;
            return self.state.queued_len() > 0;
        }

        if target_stable_len == self.enqueued_stable_len {
            return false;
        }

        self.state
            .enqueue(self.rendered_lines[self.enqueued_stable_len..target_stable_len].to_vec());
        self.enqueued_stable_len = target_stable_len;
        true
    }

    /// Rebuild the stable queue from the current render snapshot.
    ///
    /// This is used after `set_width()`, where any queued lines were computed
    /// against the old width and can no longer be trusted to line up with the
    /// current render.
    fn rebuild_stable_queue_from_render(&mut self) {
        let target_stable_len = self.compute_target_stable_len();
        self.state.clear_queue();
        if self.emitted_stable_len < target_stable_len {
            self.state
                .enqueue(self.rendered_lines[self.emitted_stable_len..target_stable_len].to_vec());
        }
        self.enqueued_stable_len = target_stable_len;
    }

    /// How many rendered lines to withhold as mutable tail.
    ///
    /// When a table is detected (`Confirmed` or `PendingHeader`), the entire
    /// table region is held as tail because adding a row can reshape table
    /// column widths. For `PendingHeader`, only content from the speculative
    /// header line onward is kept mutable so earlier prose can continue
    /// streaming. A bounded-window rotation also forces its conservative suffix
    /// to remain mutable in both rich and raw modes. This is the core decision
    /// point for the holdback mechanism.
    fn active_tail_budget_lines(&mut self) -> usize {
        let scan_start = Instant::now();
        let holdback_state = self.holdback_scanner.state();
        let table_tail_budget = match self.render_mode {
            HistoryRenderMode::Raw => 0,
            HistoryRenderMode::Rich => match holdback_state {
                TableHoldbackState::Confirmed { table_start: start }
                | TableHoldbackState::PendingHeader {
                    header_start: start,
                } => self.tail_budget_from_source_start(start),
                TableHoldbackState::None => 0,
            },
        };
        let forced_tail_budget = self.forced_tail_render_start.map_or(0, |start| {
            self.rendered_lines.len().saturating_sub(start)
        });
        let tail_budget = table_tail_budget.max(forced_tail_budget);
        tracing::trace!(
            state = ?holdback_state,
            tail_budget,
            elapsed_us = scan_start.elapsed().as_micros(),
            "table holdback decision",
        );
        tail_budget
    }

    /// Convert a raw-source boundary into the number of rendered tail lines.
    ///
    /// The important contract here is that the holdback scanner reasons in
    /// byte offsets while the queue operates in rendered lines. This helper is
    /// the only place where those coordinate systems are bridged.
    fn tail_budget_from_source_start(&mut self, source_start: usize) -> usize {
        if source_start == 0 {
            return self.rendered_lines.len();
        }
        let source_start = source_start.min(self.raw_source.len());
        let stable_prefix_len = self.stable_prefix_len_for_source_start(source_start);
        self.rendered_lines.len().saturating_sub(stable_prefix_len)
    }

    /// Render the stable prefix before `source_start` and return its line count.
    ///
    /// This value is cached because dense table streams can call into this path
    /// for every committed line while the header/delimiter/body are still
    /// arriving incrementally.
    fn stable_prefix_len_for_source_start(&mut self, source_start: usize) -> usize {
        if let Some(cache) = &self.stable_prefix_len_cache
            && cache.source_start == source_start
            && cache.width == self.width
        {
            tracing::trace!(
                source_start,
                width = ?self.width,
                stable_prefix_len = cache.stable_prefix_len,
                "table holdback stable-prefix cache hit",
            );
            return cache.stable_prefix_len;
        }

        if source_start > MUTABLE_MARKDOWN_SOURCE_MAX_BYTES {
            let stable_prefix_len = self
                .forced_tail_render_start
                .unwrap_or(self.emitted_stable_len)
                .min(self.rendered_lines.len());
            self.stable_prefix_len_cache = Some(StablePrefixLenCache {
                source_start,
                width: self.width,
                stable_prefix_len,
            });
            return stable_prefix_len;
        }

        let render_start = Instant::now();
        let stable_prefix_render = render_markdown_agent_with_links_and_cwd(
            &self.raw_source[..source_start.min(self.raw_source.len())],
            self.width,
            Some(self.cwd.as_path()),
        );
        let stable_prefix_len = stable_prefix_render.len();
        tracing::trace!(
            source_start,
            width = ?self.width,
            stable_prefix_len,
            elapsed_us = render_start.elapsed().as_micros(),
            "table holdback stable-prefix render",
        );
        self.stable_prefix_len_cache = Some(StablePrefixLenCache {
            source_start,
            width: self.width,
            stable_prefix_len,
        });
        stable_prefix_len
    }
}

/// Return at most the configured live suffix bytes without splitting a UTF-8 scalar value.
fn bounded_live_partial_source(source: &str) -> &str {
    if source.len() <= MUTABLE_MARKDOWN_SOURCE_MAX_BYTES {
        return source;
    }

    let mut start = source.len() - MUTABLE_MARKDOWN_SOURCE_MAX_BYTES;
    while !source.is_char_boundary(start) {
        start += 1;
    }
    &source[start..]
}

/// Return a newline boundary at or below the parser byte budget, or zero for an oversized line.
fn bounded_complete_source_chunk_end(source: &str) -> usize {
    if source.len() <= MUTABLE_MARKDOWN_SOURCE_MAX_BYTES {
        return source.len();
    }

    let mut end = MUTABLE_MARKDOWN_SOURCE_MAX_BYTES;
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    source[..end].rfind('\n').map_or(0, |idx| idx + 1)
}

/// Controller for streaming agent message content with table-aware holdback.
///
/// Wraps [`StreamCore`] and adds `AgentMessageCell` emission styling.
pub(crate) struct StreamController {
    core: StreamCore,
    header_emitted: bool,
}

impl StreamController {
    /// Create a controller whose markdown renderer shortens local file links relative to `cwd`.
    ///
    /// `width` is the content width available to markdown rendering, not necessarily the full
    /// terminal width. Passing a stale width after resize will keep queued live output wrapped for
    /// the old viewport until app-level reflow repairs the finalized transcript.
    pub(crate) fn new(width: Option<usize>, cwd: &Path, render_mode: HistoryRenderMode) -> Self {
        Self {
            core: StreamCore::new(width, cwd, render_mode),
            header_emitted: false,
        }
    }

    pub(crate) fn push(&mut self, delta: &str) -> bool {
        self.core.push_delta(delta)
    }

    /// Finalize the active stream. Returns the final cell (if any remaining lines) and the raw
    /// markdown source for consolidation.
    pub(crate) fn finalize(&mut self) -> (Option<Box<dyn HistoryCell>>, Option<String>) {
        let remaining = self.core.finalize_remaining();
        if self.core.raw_source.is_empty() {
            self.core.reset();
            return (None, None);
        }

        // Move ownership — source is consumed before reset() clears it.
        let source = std::mem::take(&mut self.core.raw_source);
        let out = self.emit(remaining);
        self.core.reset();
        (out, Some(source))
    }

    pub(crate) fn on_commit_tick(&mut self) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.core.tick();
        (self.emit(step), self.core.is_idle())
    }

    pub(crate) fn on_commit_tick_batch(
        &mut self,
        max_lines: usize,
    ) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.core.tick_batch(max_lines);
        (self.emit(step), self.core.is_idle())
    }

    // Thin StreamController accessors inlined — one-liner delegates called
    // on every render frame and animation tick.

    #[inline]
    pub(crate) fn queued_lines(&self) -> usize {
        self.core.queued_lines()
    }

    pub(crate) fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.core.oldest_queued_age(now)
    }

    #[inline]
    pub(crate) fn current_tail_lines(&self) -> Vec<HyperlinkLine> {
        self.core.current_tail_lines()
    }

    #[inline]
    pub(crate) fn tail_starts_stream(&self) -> bool {
        !self.header_emitted && self.core.enqueued_stable_len == 0
    }

    #[inline]
    pub(crate) fn has_live_tail(&self) -> bool {
        self.core.has_tail()
    }

    pub(crate) fn clear_queue(&mut self) {
        self.core.state.clear_queue();
        self.core.enqueued_stable_len = self.core.emitted_stable_len;
    }

    pub(crate) fn set_width(&mut self, width: Option<usize>) {
        self.core.set_width(width);
    }

    pub(crate) fn set_render_mode(&mut self, render_mode: HistoryRenderMode) {
        self.core.set_render_mode(render_mode);
    }

    fn emit(&mut self, lines: Vec<HyperlinkLine>) -> Option<Box<dyn HistoryCell>> {
        if lines.is_empty() {
            return None;
        }
        Some(Box::new(
            history_cell::AgentMessageCell::new_hyperlink_lines(lines, {
                let header_emitted = self.header_emitted;
                self.header_emitted = true;
                !header_emitted
            }),
        ))
    }
}
// ---------------------------------------------------------------------------
// PlanStreamController — proposed plan streams
// ---------------------------------------------------------------------------

/// Controller that streams proposed plan markdown into a styled plan block.
///
/// Wraps [`StreamCore`] and adds plan-specific header, indentation, and
/// background styling.
pub(crate) struct PlanStreamController {
    core: StreamCore,
    header_emitted: bool,
    top_padding_emitted: bool,
}

impl PlanStreamController {
    /// Create a plan-stream controller whose markdown renderer shortens local file links relative
    /// to `cwd`.
    ///
    /// The width has the same meaning as in `StreamController`: it is the markdown body width, and
    /// callers must update it when the terminal width changes.
    pub(crate) fn new(width: Option<usize>, cwd: &Path, render_mode: HistoryRenderMode) -> Self {
        Self {
            core: StreamCore::new(width, cwd, render_mode),
            header_emitted: false,
            top_padding_emitted: false,
        }
    }

    pub(crate) fn push(&mut self, delta: &str) -> bool {
        self.core.push_delta(delta)
    }

    /// Finalize the active stream. Returns the final cell (if any remaining
    /// lines) plus raw markdown source for consolidation.
    pub(crate) fn finalize(&mut self) -> (Option<Box<dyn HistoryCell>>, Option<String>) {
        let remaining = self.core.finalize_remaining();
        if self.core.raw_source.is_empty() {
            self.core.reset();
            return (None, None);
        }

        // Move ownership — source is consumed before reset() clears it.
        let source = std::mem::take(&mut self.core.raw_source);
        let out = self.emit(remaining, /*include_bottom_padding*/ true);
        self.core.reset();
        (out, Some(source))
    }

    pub(crate) fn on_commit_tick(&mut self) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.core.tick();
        (
            self.emit(step, /*include_bottom_padding*/ false),
            self.core.is_idle(),
        )
    }

    pub(crate) fn on_commit_tick_batch(
        &mut self,
        max_lines: usize,
    ) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.core.tick_batch(max_lines);
        (
            self.emit(step, /*include_bottom_padding*/ false),
            self.core.is_idle(),
        )
    }

    #[inline]
    pub(crate) fn queued_lines(&self) -> usize {
        self.core.queued_lines()
    }

    #[inline]
    pub(crate) fn has_live_tail(&self) -> bool {
        self.core.has_tail()
    }

    #[inline]
    pub(crate) fn current_tail_lines(&self) -> Vec<HyperlinkLine> {
        self.core.current_tail_lines()
    }

    #[inline]
    pub(crate) fn tail_starts_stream(&self) -> bool {
        !self.header_emitted && self.core.enqueued_stable_len == 0
    }

    pub(crate) fn current_tail_display_lines(&self) -> Vec<HyperlinkLine> {
        let lines = self.current_tail_lines();
        if lines.is_empty() {
            return Vec::new();
        }
        self.render_display_lines(lines, /*include_bottom_padding*/ false)
    }

    pub(crate) fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.core.oldest_queued_age(now)
    }

    pub(crate) fn clear_queue(&mut self) {
        self.core.state.clear_queue();
        self.core.enqueued_stable_len = self.core.emitted_stable_len;
    }

    pub(crate) fn set_width(&mut self, width: Option<usize>) {
        self.core.set_width(width);
    }

    pub(crate) fn set_render_mode(&mut self, render_mode: HistoryRenderMode) {
        self.core.set_render_mode(render_mode);
    }

    fn emit(
        &mut self,
        lines: Vec<HyperlinkLine>,
        include_bottom_padding: bool,
    ) -> Option<Box<dyn HistoryCell>> {
        if lines.is_empty() && !include_bottom_padding {
            return None;
        }

        let is_stream_continuation = self.header_emitted;
        let out_lines = self.render_display_lines(lines, include_bottom_padding);
        self.header_emitted = true;
        self.top_padding_emitted = true;

        Some(Box::new(history_cell::new_proposed_plan_stream(
            out_lines,
            is_stream_continuation,
        )))
    }

    fn render_display_lines(
        &self,
        lines: Vec<HyperlinkLine>,
        include_bottom_padding: bool,
    ) -> Vec<HyperlinkLine> {
        let mut out_lines: Vec<HyperlinkLine> = Vec::with_capacity(/*capacity*/ 4);
        if !self.header_emitted {
            out_lines.push(HyperlinkLine::new(
                vec!["• ".dim(), "Proposed Plan".bold()].into(),
            ));
            out_lines.push(HyperlinkLine::new(Line::from(" ")));
        }

        let mut plan_lines: Vec<HyperlinkLine> = Vec::with_capacity(/*capacity*/ 4);
        if !self.top_padding_emitted {
            plan_lines.push(HyperlinkLine::new(Line::from(" ")));
        }
        plan_lines.extend(lines);
        if include_bottom_padding {
            plan_lines.push(HyperlinkLine::new(Line::from(" ")));
        }

        let plan_style = proposed_plan_style();
        let plan_lines = prefix_hyperlink_lines(plan_lines, "  ".into(), "  ".into())
            .into_iter()
            .map(|line| line.style(plan_style))
            .collect::<Vec<_>>();
        out_lines.extend(plan_lines);
        out_lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_hyperlinks::visible_lines;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn test_cwd() -> PathBuf {
        // These tests only need a stable absolute cwd; using temp_dir() avoids baking Unix- or
        // Windows-specific root semantics into the fixtures.
        std::env::temp_dir()
    }

    fn stream_controller(width: Option<usize>) -> StreamController {
        StreamController::new(width, &test_cwd(), HistoryRenderMode::Rich)
    }

    fn plan_stream_controller(width: Option<usize>) -> PlanStreamController {
        PlanStreamController::new(width, &test_cwd(), HistoryRenderMode::Rich)
    }

    fn lines_to_plain_strings(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    fn hyperlink_lines_to_plain_strings(lines: &[HyperlinkLine]) -> Vec<String> {
        lines_to_plain_strings(&visible_lines(lines.to_vec()))
    }

    fn collect_streamed_lines(deltas: &[&str], width: Option<usize>) -> Vec<String> {
        let mut ctrl = stream_controller(width);
        let mut lines = Vec::new();
        for d in deltas {
            ctrl.push(d);
            while let (Some(cell), idle) = ctrl.on_commit_tick() {
                lines.extend(cell.transcript_lines(u16::MAX));
                if idle {
                    break;
                }
            }
        }
        if let (Some(cell), _source) = ctrl.finalize() {
            lines.extend(cell.transcript_lines(u16::MAX));
        }
        lines_to_plain_strings(&lines)
            .into_iter()
            .map(|s| s.chars().skip(2).collect::<String>())
            .collect()
    }

    fn collect_plan_streamed_lines(deltas: &[&str], width: Option<usize>) -> Vec<String> {
        let mut ctrl = plan_stream_controller(width);
        let mut lines = Vec::new();
        for d in deltas {
            ctrl.push(d);
            while let (Some(cell), idle) = ctrl.on_commit_tick() {
                lines.extend(cell.transcript_lines(u16::MAX));
                if idle {
                    break;
                }
            }
        }
        if let (Some(cell), _source) = ctrl.finalize() {
            lines.extend(cell.transcript_lines(u16::MAX));
        }
        lines_to_plain_strings(&lines)
    }

    #[test]
    fn controller_set_width_rebuilds_queued_lines() {
        let mut ctrl = stream_controller(Some(120));
        let delta = "This is a long line that should wrap into multiple rows when resized.\n";
        assert!(ctrl.push(delta));
        assert_eq!(ctrl.queued_lines(), 1);

        ctrl.set_width(Some(24));
        let (cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        let rendered = lines_to_plain_strings(
            &cell
                .expect("expected resized queued lines")
                .transcript_lines(u16::MAX),
        );

        assert!(idle);
        assert!(
            rendered.len() > 1,
            "expected resized content to occupy multiple lines, got {rendered:?}",
        );
    }

    #[test]
    fn controller_set_width_no_duplicate_after_emit() {
        let mut ctrl = stream_controller(Some(120));
        let line =
            "This is a long line that definitely wraps when the terminal shrinks to 24 columns.\n";
        ctrl.push(line);
        let (cell, _) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(cell.is_some(), "expected emitted cell");
        assert_eq!(ctrl.queued_lines(), 0);

        ctrl.set_width(Some(24));

        assert_eq!(
            ctrl.queued_lines(),
            0,
            "already-emitted content must not be re-queued after resize",
        );
    }

    #[test]
    fn controller_tick_batch_zero_is_noop() {
        let mut ctrl = stream_controller(Some(80));
        assert!(ctrl.push("line one\n"));
        assert_eq!(ctrl.queued_lines(), 1);

        let (cell, idle) = ctrl.on_commit_tick_batch(/*max_lines*/ 0);
        assert!(cell.is_none(), "batch size 0 should not emit lines");
        assert!(!idle, "batch size 0 should not drain queued lines");
        assert_eq!(
            ctrl.queued_lines(),
            1,
            "queue depth should remain unchanged"
        );
    }

    #[test]
    fn controller_has_live_tail_reflects_tail_presence() {
        let mut ctrl = stream_controller(Some(80));
        assert!(!ctrl.has_live_tail());

        ctrl.core.rendered_lines = vec![Line::from("tail line").into()];
        ctrl.core.enqueued_stable_len = 0;
        assert!(ctrl.has_live_tail());

        ctrl.core.enqueued_stable_len = 1;
        assert!(!ctrl.has_live_tail());
    }

    #[test]
    fn plan_controller_has_live_tail_reflects_tail_presence() {
        let mut ctrl = plan_stream_controller(Some(80));
        assert!(!ctrl.has_live_tail());

        ctrl.core.rendered_lines = vec![Line::from("tail line").into()];
        ctrl.core.enqueued_stable_len = 0;
        assert!(ctrl.has_live_tail());

        ctrl.core.enqueued_stable_len = 1;
        assert!(!ctrl.has_live_tail());
    }

    #[test]
    fn controller_live_tail_exposes_uncommitted_table_cell_immediately() {
        let mut ctrl = stream_controller(Some(80));
        ctrl.push("| A | B |\n");
        ctrl.push("| --- | --- |\n");
        ctrl.push("| partial");

        let tail = hyperlink_lines_to_plain_strings(&ctrl.current_tail_lines()).join("\n");
        assert!(
            tail.contains("partial"),
            "expected unterminated table content in the live tail: {tail:?}",
        );
    }

    #[test]
    fn controller_live_tail_exposes_plain_unterminated_line_immediately() {
        let mut ctrl = stream_controller(Some(80));
        assert!(ctrl.push("plain text without newline"));

        let tail = hyperlink_lines_to_plain_strings(&ctrl.current_tail_lines()).join("\n");
        assert!(tail.contains("plain text without newline"), "tail: {tail:?}");
        assert!(ctrl.has_live_tail());
    }

    #[test]
    fn controller_debounces_follow_up_partial_renders_to_one_frame() {
        let mut ctrl = stream_controller(Some(80));
        let started_at = Instant::now();
        ctrl.core.push_delta_at("hel", started_at);
        let first = hyperlink_lines_to_plain_strings(&ctrl.current_tail_lines()).join("\n");
        assert!(first.contains("hel"), "first partial render: {first:?}");

        assert!(ctrl.core.push_delta_at("lo", started_at + Duration::from_millis(1)));
        let debounced = hyperlink_lines_to_plain_strings(&ctrl.current_tail_lines()).join("\n");
        assert_eq!(debounced, first, "follow-up mutation should be coalesced");
        assert!(!ctrl.core.is_idle(), "dirty tail must keep the frame tick alive");

        ctrl.core.refresh_live_partial(
            started_at + LIVE_PARTIAL_RENDER_DEBOUNCE,
            /*force*/ false,
        );
        let refreshed = hyperlink_lines_to_plain_strings(&ctrl.current_tail_lines()).join("\n");
        assert!(refreshed.contains("hello"), "refreshed tail: {refreshed:?}");
        assert!(ctrl.core.is_idle());
    }

    #[test]
    fn live_partial_source_is_bounded_at_utf8_boundary() {
        let oversized = format!(
            "prefix{}tail",
            "é".repeat(MUTABLE_MARKDOWN_SOURCE_MAX_BYTES)
        );
        let bounded = bounded_live_partial_source(&oversized);

        assert!(bounded.len() <= MUTABLE_MARKDOWN_SOURCE_MAX_BYTES);
        assert!(bounded.ends_with("tail"));
        assert!(!bounded.starts_with("prefix"));
    }

    #[test]
    fn completed_render_prefix_is_retained_when_mutable_window_rotates() {
        let mut ctrl =
            StreamController::new(Some(80), &test_cwd(), HistoryRenderMode::Raw);
        let prefix = (0..700)
            .map(|line| format!("prefix line {line:04}\n"))
            .collect::<String>();
        assert!(prefix.len() < MUTABLE_MARKDOWN_SOURCE_MAX_BYTES);
        assert!(ctrl.push(&prefix));
        let retained_prefix = hyperlink_lines_to_plain_strings(&ctrl.core.rendered_lines[..8]);

        let suffix = (0..1_000)
            .map(|line| format!("suffix line {line:04}\n"))
            .collect::<String>();
        assert!(suffix.len() > MUTABLE_MARKDOWN_SOURCE_MAX_BYTES);
        assert!(ctrl.push(&suffix));

        assert_eq!(
            hyperlink_lines_to_plain_strings(&ctrl.core.rendered_lines[..8]),
            retained_prefix,
        );
        assert!(ctrl.core.forced_tail_render_start.is_some());
        assert!(
            ctrl.core.max_streaming_render_source_len <= MUTABLE_MARKDOWN_SOURCE_MAX_BYTES,
            "largest parser window was {} bytes",
            ctrl.core.max_streaming_render_source_len,
        );
        let (first_cell, idle) = ctrl.on_commit_tick();
        let first_cell = first_cell.expect("expected retained stable prefix to remain queued");
        let first_line = lines_to_plain_strings(&first_cell.transcript_lines(u16::MAX));
        assert!(
            first_line
                .first()
                .is_some_and(|line| line.contains("prefix line 0000")),
            "stable queue order changed after rotation: {first_line:?}",
        );
        assert!(!idle, "retained prefix should continue draining in order");
    }

    #[test]
    fn bounded_streaming_windows_preserve_large_final_fence_and_table_render() {
        let code = format!(
            "const HUGE: &str = \"{}\";\n",
            "é".repeat(MUTABLE_MARKDOWN_SOURCE_MAX_BYTES)
        );
        let source = format!(
            "```rust\n{code}```\n\n| Name | Value |\n| --- | --- |\n| final | exact |\n"
        );
        assert!(source.len() > MUTABLE_MARKDOWN_SOURCE_MAX_BYTES);

        let mut expected = Vec::new();
        crate::markdown::append_markdown_agent(&source, Some(80), &mut expected);
        let expected = lines_to_plain_strings(&expected);

        let mut ctrl = stream_controller(Some(80));
        assert!(ctrl.push(&source));
        assert_eq!(ctrl.queued_lines(), 0, "rotated output must remain mutable");
        assert!(
            ctrl.core.max_streaming_render_source_len <= MUTABLE_MARKDOWN_SOURCE_MAX_BYTES,
            "largest parser window was {} bytes",
            ctrl.core.max_streaming_render_source_len,
        );

        let (cell, finalized_source) = ctrl.finalize();
        assert_eq!(finalized_source.as_deref(), Some(source.as_str()));
        let finalized = lines_to_plain_strings(
            &cell
                .expect("expected canonical final cell")
                .transcript_lines(u16::MAX),
        )
        .into_iter()
        .map(|line| line.chars().skip(2).collect::<String>())
        .collect::<Vec<_>>();
        assert_eq!(finalized, expected);
    }

    #[test]
    fn controller_live_tail_rerenders_table_tail_after_resize() {
        let mut ctrl = stream_controller(Some(96));
        ctrl.push("| # | Feature | Details | Link |\n");
        ctrl.push("| --- | --- | --- | --- |\n");
        ctrl.push(
            "| 1 | RESIZE_REPRO_SENTINEL | long wrapped content that should be reflowed | https://example.com/resize |\n",
        );

        for width in [48, 104, 56] {
            ctrl.set_width(Some(width));
            let tail = hyperlink_lines_to_plain_strings(&ctrl.current_tail_lines());

            let mut expected = Vec::new();
            crate::markdown::append_markdown_agent(
                &ctrl.core.raw_source,
                Some(width),
                &mut expected,
            );
            let expected = lines_to_plain_strings(&expected);

            assert_eq!(
                tail, expected,
                "expected live table tail to be rerendered at width {width}",
            );
        }
    }

    #[test]
    fn controller_set_width_partial_drain_no_lost_lines() {
        let mut ctrl = stream_controller(Some(40));
        ctrl.push("AAAA BBBB CCCC DDDD EEEE FFFF GGGG HHHH IIII JJJJ\n");
        ctrl.push("second line\n");

        let (cell, idle) = ctrl.on_commit_tick();
        assert!(cell.is_some(), "expected 1 emitted line");
        assert!(!idle, "queue should still have lines");
        let remaining_before = ctrl.queued_lines();
        assert!(remaining_before > 0, "should have queued lines left");

        ctrl.set_width(Some(20));

        let (cell, source) = ctrl.finalize();
        let final_lines = cell
            .map(|c| lines_to_plain_strings(&c.transcript_lines(u16::MAX)))
            .unwrap_or_default();

        assert!(
            final_lines.iter().any(|l| l.contains("second line")),
            "un-emitted 'second line' was lost after resize; got: {final_lines:?}",
        );
        assert!(source.is_some(), "expected source from finalize");
    }

    #[test]
    fn controller_set_width_partial_drain_keeps_pending_queue() {
        let mut ctrl = stream_controller(Some(40));
        ctrl.push("AAAA BBBB CCCC DDDD EEEE FFFF GGGG HHHH IIII JJJJ\n");
        ctrl.push("second line\n");

        let (cell, idle) = ctrl.on_commit_tick();
        assert!(cell.is_some(), "expected 1 emitted line");
        assert!(!idle, "queue should still have lines");
        assert!(ctrl.queued_lines() > 0, "expected pending queued lines");

        ctrl.set_width(Some(20));

        assert!(
            ctrl.queued_lines() > 0,
            "resize must preserve pending queued lines"
        );

        let mut drained = Vec::new();
        for _ in 0..64 {
            let (cell, is_idle) = ctrl.on_commit_tick();
            if let Some(cell) = cell {
                drained.extend(lines_to_plain_strings(&cell.transcript_lines(u16::MAX)));
            }
            if is_idle {
                break;
            }
        }

        assert!(
            drained.iter().any(|l| l.contains("second line")),
            "pending lines should continue draining after resize; got {drained:?}",
        );
    }

    #[test]
    fn controller_set_width_preserves_in_flight_tail() {
        let mut ctrl = stream_controller(Some(80));
        ctrl.push("tail without newline");
        ctrl.set_width(Some(24));

        let (cell, _source) = ctrl.finalize();
        let rendered = lines_to_plain_strings(
            &cell
                .expect("expected finalized tail")
                .transcript_lines(u16::MAX),
        );

        assert_eq!(rendered, vec!["• tail without newline".to_string()]);
    }

    #[test]
    fn controller_set_width_preserves_table_tail_when_queue_is_empty() {
        let mut ctrl = stream_controller(Some(80));
        ctrl.push("intro line\n");

        let (_cell, idle) = ctrl.on_commit_tick();
        assert!(idle, "intro line should fully drain");
        assert_eq!(ctrl.queued_lines(), 0, "expected empty queue before table");

        ctrl.push("| A | B |\n");
        assert_eq!(
            ctrl.queued_lines(),
            0,
            "pending table header should remain mutable tail, not queued",
        );
        assert!(ctrl.has_live_tail(), "expected live tail before resize");

        ctrl.set_width(Some(24));

        let tail_after = hyperlink_lines_to_plain_strings(&ctrl.current_tail_lines());
        assert!(
            !tail_after.is_empty(),
            "resize must keep mutable tail when queue is empty",
        );
        let joined = tail_after.join(" ");
        assert!(
            joined.contains('A') && joined.contains('B'),
            "expected table header content to remain in tail after resize: {tail_after:?}",
        );
    }

    #[test]
    fn plan_controller_set_width_preserves_in_flight_tail() {
        let mut ctrl = plan_stream_controller(Some(80));
        ctrl.push("1. Item without newline");
        ctrl.set_width(Some(24));

        let rendered = lines_to_plain_strings(
            &(ctrl
                .finalize()
                .0
                .expect("expected finalized tail")
                .transcript_lines(u16::MAX)),
        );

        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Item without newline")),
            "expected finalized plan content after resize, got {rendered:?}",
        );
    }

    #[test]
    fn plan_controller_holds_table_header_as_live_tail() {
        let mut ctrl = plan_stream_controller(Some(80));
        assert!(ctrl.push("Intro\n"));
        let (_cell, idle) = ctrl.on_commit_tick_batch(usize::MAX);
        assert!(idle, "intro line should fully drain");

        assert!(!ctrl.push("| Step | Owner |\n"));
        assert!(
            ctrl.has_live_tail(),
            "expected plan table header to be held"
        );
    }

    #[test]
    fn controller_loose_vs_tight_with_commit_ticks_matches_full() {
        let mut ctrl = stream_controller(/*width*/ None);
        let mut lines = Vec::new();

        let deltas = vec![
            "\n\n",
            "Loose",
            " vs",
            ".",
            " tight",
            " list",
            " items",
            ":\n",
            "1",
            ".",
            " Tight",
            " item",
            "\n",
            "2",
            ".",
            " Another",
            " tight",
            " item",
            "\n\n",
            "1",
            ".",
            " Loose",
            " item",
            " with",
            " its",
            " own",
            " paragraph",
            ".\n\n",
            "  ",
            " This",
            " paragraph",
            " belongs",
            " to",
            " the",
            " same",
            " list",
            " item",
            ".\n\n",
            "2",
            ".",
            " Second",
            " loose",
            " item",
            " with",
            " a",
            " nested",
            " list",
            " after",
            " a",
            " blank",
            " line",
            ".\n\n",
            "  ",
            " -",
            " Nested",
            " bullet",
            " under",
            " a",
            " loose",
            " item",
            "\n",
            "  ",
            " -",
            " Another",
            " nested",
            " bullet",
            "\n\n",
        ];

        for d in deltas.iter() {
            ctrl.push(d);
            while let (Some(cell), idle) = ctrl.on_commit_tick() {
                lines.extend(cell.transcript_lines(u16::MAX));
                if idle {
                    break;
                }
            }
        }
        if let (Some(cell), _source) = ctrl.finalize() {
            lines.extend(cell.transcript_lines(u16::MAX));
        }

        let streamed: Vec<_> = lines_to_plain_strings(&lines)
            .into_iter()
            .map(|s| s.chars().skip(2).collect::<String>())
            .collect();

        let source: String = deltas.iter().copied().collect();
        let mut rendered: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown_agent(&source, /*width*/ None, &mut rendered);
        let rendered_strs = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, rendered_strs);

        let expected = vec![
            "Loose vs. tight list items:".to_string(),
            "".to_string(),
            "1. Tight item".to_string(),
            "2. Another tight item".to_string(),
            "3. Loose item with its own paragraph.".to_string(),
            "".to_string(),
            "   This paragraph belongs to the same list item.".to_string(),
            "".to_string(),
            "4. Second loose item with a nested list after a blank line.".to_string(),
            "    - Nested bullet under a loose item".to_string(),
            "    - Another nested bullet".to_string(),
        ];
        assert_eq!(
            streamed, expected,
            "expected exact rendered lines for loose/tight section"
        );
    }

    #[test]
    fn controller_streamed_table_matches_full_render_widths() {
        let deltas = vec![
            "| Key | Description |\n",
            "| --- | --- |\n",
            "| -v | Enable very verbose logging output for debugging |\n",
            "\n",
        ];

        let streamed = collect_streamed_lines(&deltas, Some(80));

        let source: String = deltas.iter().copied().collect();
        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(&source, /*width*/ Some(80), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, expected);
    }

    #[test]
    fn controller_holds_blockquoted_table_tail_until_stable() {
        let deltas = vec![
            "> | A | B |\n",
            "> | --- | --- |\n",
            "> | longvalue | ok |\n",
            "\n",
        ];

        let streamed = collect_streamed_lines(&deltas, Some(80));

        let source: String = deltas.iter().copied().collect();
        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(&source, /*width*/ Some(80), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, expected);
    }

    #[test]
    fn controller_keeps_pre_table_lines_queued_when_table_is_confirmed() {
        let mut ctrl = stream_controller(Some(80));

        ctrl.push("Intro line before table.\n");
        assert_eq!(ctrl.queued_lines(), 1);

        ctrl.push("| Key | Value |\n");
        ctrl.push("| --- | --- |\n");
        assert_eq!(
            ctrl.queued_lines(),
            1,
            "pre-table line should remain queued after table confirmation",
        );

        let (cell, idle) = ctrl.on_commit_tick();
        let committed = cell
            .map(|cell| lines_to_plain_strings(&cell.transcript_lines(u16::MAX)))
            .unwrap_or_default();
        assert!(
            committed
                .iter()
                .any(|line| line.contains("Intro line before table.")),
            "expected pre-table line to commit independently: {committed:?}",
        );
        assert!(idle, "only pre-table content should have been queued");
    }

    #[test]
    fn controller_set_width_during_confirmed_table_stream_matches_finalize_render() {
        let mut ctrl = stream_controller(Some(120));
        let deltas = [
            "| Key | Description |\n",
            "| --- | --- |\n",
            "| one | value that should wrap after resize |\n",
        ];
        for delta in deltas {
            ctrl.push(delta);
        }
        assert_eq!(
            ctrl.queued_lines(),
            0,
            "confirmed table should remain mutable"
        );

        ctrl.set_width(Some(32));

        let (cell, source) = ctrl.finalize();
        let source = source.expect("expected finalized source");
        let streamed = lines_to_plain_strings(
            &cell
                .expect("expected finalized table")
                .transcript_lines(u16::MAX),
        )
        .into_iter()
        .map(|line| line.chars().skip(2).collect::<String>())
        .collect::<Vec<_>>();

        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(&source, /*width*/ Some(32), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);
        assert_eq!(streamed, expected);
    }

    #[test]
    fn controller_does_not_hold_back_pipe_prose_without_table_delimiter() {
        let mut ctrl = stream_controller(Some(80));

        ctrl.push("status | owner | note\n");
        let (_first_commit, first_idle) = ctrl.on_commit_tick();
        assert!(first_idle);

        ctrl.push("next line\n");
        let (second_commit, _second_idle) = ctrl.on_commit_tick();
        assert!(
            second_commit.is_some(),
            "expected prose lines to be released once no table delimiter follows"
        );
    }

    #[test]
    fn controller_does_not_stall_repeated_pipe_prose_paragraphs() {
        let mut ctrl = stream_controller(Some(80));

        ctrl.push("alpha | beta\n\n");
        let (_first_commit, first_idle) = ctrl.on_commit_tick();
        assert!(first_idle);

        ctrl.push("gamma | delta\n\n");
        let (second_commit, _second_idle) = ctrl.on_commit_tick();
        let second_lines = second_commit
            .map(|cell| lines_to_plain_strings(&cell.transcript_lines(u16::MAX)))
            .unwrap_or_default();

        assert!(
            second_lines
                .iter()
                .any(|line| line.contains("alpha | beta")),
            "expected the first pipe-prose paragraph to stream before finalize; got {second_lines:?}",
        );
    }

    #[test]
    fn controller_handles_table_immediately_after_heading() {
        let deltas = vec![
            "### 1) Basic table\n",
            "| Name | Role | Status |\n",
            "|---|---|---|\n",
            "| Alice | Admin | Active |\n",
            "| Bob | Editor | Pending |\n",
            "\n",
        ];

        let streamed = collect_streamed_lines(&deltas, Some(100));

        let source: String = deltas.iter().copied().collect();
        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(&source, /*width*/ Some(100), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, expected);
    }

    #[test]
    fn controller_renders_separators_for_multi_table_response_shape() {
        let source = "Absolutely. Here are several different Markdown table patterns you can use for rendering tests.\n\n| Name  | Role      |
  Location |\n|-------|-----------|----------|\n| Ava   | Engineer  | NYC      |\n| Malik | Designer  | Berlin   |\n| Priya | PM        | Remote
  |\n\n| Item        | Qty | Price | In Stock |\n|:------------|----:|------:|:--------:|\n| Keyboard    |   2 | 49.99 |    Yes   |\n| Mouse       |  10
   | 19.50 |    Yes   |\n| Monitor     |   1 | 219.0 |    No    |\n\n| Field         | Example                         | Notes
  |\n|---------------|----------------------------------|--------------------------|\n| Escaped pipe  | `foo \\| bar`                    | Should stay
  in one cell  |\n| Inline code   | `let x = value;`                | Monospace inline content |\n| Link          | [OpenAI](https://openai.com)    |
  Standard markdown link   |\n";

        let chunked = source
            .split_inclusive('\n')
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let deltas = chunked.iter().map(String::as_str).collect::<Vec<_>>();
        let streamed = collect_streamed_lines(&deltas, Some(120));
        assert!(
            streamed.iter().any(|line| line.contains('━')),
            "expected table separator in streamed output: {streamed:?}"
        );
    }

    #[test]
    fn controller_renders_separators_for_no_outer_pipes_table_shape() {
        let source = "### 1) Basic\n\n| Name | Role | Active |\n|---|---|---|\n| Alice | Engineer | Yes |\n| Bob | Designer | No |\n\n### 2) No outer
  pipes\n\nCol A | Col B | Col C\n--- | --- | ---\nx | y | z\n10 | 20 | 30\n\n### 3) Another table\n\n| Key | Value |\n|---|---|\n| a | b |\n";

        let chunked = source
            .split_inclusive('\n')
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let deltas = chunked.iter().map(String::as_str).collect::<Vec<_>>();
        let streamed = collect_streamed_lines(&deltas, Some(100));

        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(source, /*width*/ Some(100), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, expected);
        let has_raw_no_outer_header = streamed
            .iter()
            .any(|line| line.trim() == "Col A | Col B | Col C");
        assert!(
            !has_raw_no_outer_header,
            "no-outer-pipes header should not remain raw in final streamed output: {streamed:?}"
        );
        assert!(
            streamed.iter().any(|line| line.contains('━')),
            "expected table separator in final streamed output: {streamed:?}"
        );
    }

    #[test]
    fn controller_stabilizes_first_no_outer_pipes_table_in_response() {
        let deltas = vec![
            "### No outer pipes first\n\n",
            "Col A | Col B | Col C\n",
            "--- | --- | ---\n",
            "x | y | z\n",
            "10 | 20 | 30\n",
            "\n",
            "After table paragraph.\n",
        ];
        let streamed = collect_streamed_lines(&deltas, Some(100));

        let source: String = deltas.iter().copied().collect();
        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(&source, /*width*/ Some(100), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, expected);
        assert!(
            streamed.iter().any(|line| line.contains('━')),
            "expected table separator for no-outer-pipes streaming: {streamed:?}"
        );
        assert!(
            !streamed
                .iter()
                .any(|line| line.trim() == "Col A | Col B | Col C"),
            "did not expect raw no-outer-pipes header in final streamed output: {streamed:?}"
        );
    }

    #[test]
    fn controller_stabilizes_two_column_no_outer_table_in_response() {
        let deltas = vec![
            "A | B\n",
            "--- | ---\n",
            "left | right\n",
            "\n",
            "After table paragraph.\n",
        ];
        let streamed = collect_streamed_lines(&deltas, Some(80));

        let source: String = deltas.iter().copied().collect();
        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(&source, /*width*/ Some(80), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, expected);
        assert!(
            streamed.iter().any(|line| line.contains('━')),
            "expected table separator for two-column no-outer table: {streamed:?}"
        );
        assert!(
            !streamed.iter().any(|line| line.trim() == "A | B"),
            "did not expect raw two-column no-outer header in final streamed output: {streamed:?}"
        );
    }

    #[test]
    fn controller_converts_no_outer_table_between_preboxed_sections() {
        let source = "  ┌───────┬──────────┬────────┐\n  │ Name  │ Role     │ Active │\n  ├───────┼──────────┼────────┤\n  │ Alice │ Engineer │ Yes    │\n  │ Bob   │ Designer │ No     │\n  │ Cara  │ PM       │ Yes    │\n  └───────┴──────────┴────────┘\n\n  ### 3) No outer pipes\n\n  Col A | Col B | Col C\n  --- | --- | ---\n  x | y | z\n  10 | 20 | 30\n\n  ┌─────────────────┬────────┬────────────────────────┐\n  │ Example         │ Output │ Notes                  │\n  ├─────────────────┼────────┼────────────────────────┤\n  │ a | b           │ `a     │ b`                     │\n  │ npm run test    │ ok     │ Inline code formatting │\n  │ SELECT * FROM t │ 3 rows │ SQL snippet            │\n  └─────────────────┴────────┴────────────────────────┘\n";

        let deltas = source
            .split_inclusive('\n')
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let streamed = collect_streamed_lines(
            &deltas.iter().map(String::as_str).collect::<Vec<_>>(),
            Some(100),
        );

        let has_raw_no_outer_header = streamed
            .iter()
            .any(|line| line.trim() == "Col A | Col B | Col C");
        assert!(
            !has_raw_no_outer_header,
            "no-outer table header remained raw in streamed output: {streamed:?}"
        );
        assert!(
            streamed
                .iter()
                .any(|line| line.contains(" Col A    Col B    Col C")),
            "expected converted no-outer table header in streamed output: {streamed:?}"
        );
    }

    #[test]
    fn controller_keeps_markdown_fenced_tables_mutable_until_finalize() {
        let source = "```md\n| A | B |\n|---|---|\n| 1 | 2 |\n```\n";
        let deltas = vec![
            "```md\n",
            "| A | B |\n",
            "|---|---|\n",
            "| 1 | 2 |\n",
            "```\n",
        ];
        let streamed = collect_streamed_lines(&deltas, Some(80));

        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(source, /*width*/ Some(80), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, expected);
        assert!(
            streamed.iter().any(|line| line.contains('━')),
            "expected table separator in streamed output: {streamed:?}"
        );
        assert!(
            !streamed.iter().any(|line| line.trim() == "| A | B |"),
            "did not expect raw table header line after finalize: {streamed:?}"
        );
    }

    #[test]
    fn controller_keeps_markdown_fenced_no_outer_tables_mutable_until_finalize() {
        let source =
            "```md\nCol A | Col B | Col C\n--- | --- | ---\nx | y | z\n10 | 20 | 30\n```\n";
        let deltas = vec![
            "```md\n",
            "Col A | Col B | Col C\n",
            "--- | --- | ---\n",
            "x | y | z\n",
            "10 | 20 | 30\n",
            "```\n",
        ];
        let streamed = collect_streamed_lines(&deltas, Some(100));

        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(source, /*width*/ Some(100), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, expected);
        assert!(
            streamed.iter().any(|line| line.contains('━')),
            "expected table separator in streamed output: {streamed:?}"
        );
        assert!(
            !streamed
                .iter()
                .any(|line| line.trim() == "Col A | Col B | Col C"),
            "did not expect raw no-outer-pipes header line after finalize: {streamed:?}"
        );
    }

    #[test]
    fn controller_live_view_matches_render_during_interleaved_table_streaming() {
        let source = "Project updates are easier to scan when narrative and structured data alternate.\n\n| Focus Area | Owner | Priority | Status |\n|---|---|---|---|\n| Authentication cleanup | Maya | High | 80% |\n| CLI error messages | Jordan | Medium | 55% |\n| Docs refresh | Lee | Low | 30% |\n\nThe first checkpoint shows progress, but we still have open risks.\n\n| Task | Command / Artifact | Due | State |\n|---|---|---|---|\n| Run unit tests | `cargo test -p codex-core` | Today | ✅ |\n| Snapshot review | `cargo insta pending-snapshots -p codex-tui` | Today | ⏳ |\n| Changelog draft | Release template (https://replacechangelog.com/) | Tomorrow | 📝 |\n\nFinal sign-off criteria are summarized below.\n";
        let width = Some(72usize);
        let mut ctrl = stream_controller(width);
        let mut emitted_lines: Vec<Line<'static>> = Vec::new();

        for delta in source.split_inclusive('\n') {
            ctrl.push(delta);
            loop {
                let (cell, idle) = ctrl.on_commit_tick();
                if let Some(cell) = cell {
                    emitted_lines.extend(cell.transcript_lines(u16::MAX).into_iter().map(|line| {
                        let plain: String = line
                            .spans
                            .iter()
                            .map(|s| s.content.clone())
                            .collect::<Vec<_>>()
                            .join("");
                        Line::from(plain.chars().skip(2).collect::<String>())
                    }));
                }
                if idle {
                    break;
                }
            }

            let mut visible = emitted_lines.clone();
            visible.extend(visible_lines(ctrl.current_tail_lines()));
            let visible_plain = lines_to_plain_strings(&visible);

            let mut expected = Vec::new();
            crate::markdown::append_markdown_agent(
                &ctrl.core.raw_source,
                /*width*/ width,
                &mut expected,
            );
            let expected_plain = lines_to_plain_strings(&expected);

            assert_eq!(
                visible_plain, expected_plain,
                "live view diverged after delta: {delta:?}"
            );
        }
    }

    #[test]
    fn finalized_stream_table_preserves_semantic_url_fragments() {
        let destination = "https://example.com/a/very/long/path/to/a/table/artifact";
        let source = format!("| Item | URL |\n| --- | --- |\n| report | {destination} |\n");
        let mut ctrl = stream_controller(/*width*/ Some(32));
        ctrl.push(&source);

        let (cell, _) = ctrl.finalize();
        let lines = cell
            .expect("final stream table cell")
            .display_hyperlink_lines(/*width*/ 32);
        let linked_rows = lines
            .iter()
            .filter(|line| !line.hyperlinks.is_empty())
            .collect::<Vec<_>>();

        assert!(linked_rows.len() > 1);
        assert!(linked_rows.iter().all(|line| {
            line.hyperlinks
                .iter()
                .all(|link| link.destination == destination)
        }));
    }

    #[test]
    fn controller_keeps_non_markdown_fenced_tables_as_code() {
        let source = "```sh\n| A | B |\n|---|---|\n| 1 | 2 |\n```\n";
        let deltas = vec![
            "```sh\n",
            "| A | B |\n",
            "|---|---|\n",
            "| 1 | 2 |\n",
            "```\n",
        ];
        let streamed = collect_streamed_lines(&deltas, Some(80));

        let mut rendered = Vec::new();
        crate::markdown::append_markdown_agent(source, /*width*/ Some(80), &mut rendered);
        let expected = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, expected);
        assert!(
            streamed.iter().any(|line| line.trim() == "| A | B |"),
            "expected code-fenced pipe line to remain raw: {streamed:?}"
        );
        assert!(
            !streamed
                .iter()
                .any(|line| line.contains('━') || line.contains('─')),
            "did not expect a table separator for non-markdown fence: {streamed:?}"
        );
    }

    #[test]
    fn plan_controller_streamed_table_matches_final_render() {
        let deltas = vec![
            "## Build plan\n\n",
            "| Step | Owner |\n",
            "|---|---|\n",
            "| Write tests | Agent |\n",
            "| Verify output | User |\n",
            "\n",
        ];
        let streamed = collect_plan_streamed_lines(&deltas, Some(80));

        let source: String = deltas.iter().copied().collect();
        let baseline = collect_plan_streamed_lines(&[source.as_str()], Some(80));

        assert_eq!(streamed, baseline);
        assert!(
            streamed.iter().any(|line| line.contains('━')),
            "expected table separators in plan streamed output: {streamed:?}"
        );
        assert!(
            !streamed
                .iter()
                .any(|line| line.trim() == "| Step | Owner |"),
            "did not expect raw table header line in plan output: {streamed:?}"
        );
    }

    #[test]
    fn finalized_plan_stream_preserves_semantic_url_fragments() {
        let destination = "https://example.com/a/very/long/path/to/a/table/artifact";
        let source = format!("| Step | URL |\n| --- | --- |\n| Verify | {destination} |\n");
        let mut ctrl = PlanStreamController::new(
            /*width*/ Some(32),
            &test_cwd(),
            HistoryRenderMode::Rich,
        );
        ctrl.push(&source);

        let (cell, _) = ctrl.finalize();
        let lines = cell
            .expect("final plan stream table cell")
            .display_hyperlink_lines(/*width*/ 32);
        let linked_rows = lines
            .iter()
            .filter(|line| !line.hyperlinks.is_empty())
            .collect::<Vec<_>>();

        assert!(linked_rows.len() > 1);
        assert!(linked_rows.iter().all(|line| {
            line.hyperlinks
                .iter()
                .all(|link| link.destination == destination)
        }));
    }

    #[test]
    fn plan_controller_streamed_markdown_fenced_table_matches_final_render() {
        let deltas = vec![
            "## Build plan\n\n",
            "```md\n",
            "| Step | Owner |\n",
            "|---|---|\n",
            "| Write tests | Agent |\n",
            "| Verify output | User |\n",
            "```\n",
            "\n",
        ];
        let streamed = collect_plan_streamed_lines(&deltas, Some(80));

        let source: String = deltas.iter().copied().collect();
        let baseline = collect_plan_streamed_lines(&[source.as_str()], Some(80));

        assert_eq!(streamed, baseline);
        assert!(
            streamed.iter().any(|line| line.contains('━')),
            "expected table separators in fenced plan output: {streamed:?}"
        );
        assert!(
            !streamed
                .iter()
                .any(|line| line.trim() == "| Step | Owner |"),
            "did not expect raw table header line in fenced plan output: {streamed:?}"
        );
    }

    #[test]
    fn table_holdback_state_detects_header_plus_delimiter() {
        let source = "| Key | Description |\n| --- | --- |\n";
        assert!(matches!(
            table_holdback_state(source),
            TableHoldbackState::Confirmed { .. }
        ));
    }

    #[test]
    fn table_holdback_state_detects_single_column_header_plus_delimiter() {
        let source = "| Only |\n| --- |\n";
        assert!(matches!(
            table_holdback_state(source),
            TableHoldbackState::Confirmed { .. }
        ));
    }

    #[test]
    fn table_holdback_state_ignores_table_like_lines_inside_unclosed_long_fence() {
        let source = "````sh\n```cmd\n| Key | Description |\n| --- | --- |\n````\n";
        assert!(
            matches!(table_holdback_state(source), TableHoldbackState::None),
            "table holdback should ignore pipe lines inside an open non-markdown fence",
        );
    }

    #[test]
    fn table_holdback_state_treats_indented_fence_text_as_plain_content() {
        let source = "    ```sh\n| Key | Description |\n| --- | --- |\n";
        assert!(
            matches!(
                table_holdback_state(source),
                TableHoldbackState::Confirmed { .. }
            ),
            "indented fence-like text should not open a fence and should not block table detection",
        );
    }

    #[test]
    fn table_holdback_state_ignores_table_like_lines_inside_blockquoted_other_fence() {
        let source = "> ```sh\n> | Key | Value |\n> | --- | --- |\n> ```\n";
        assert!(
            matches!(table_holdback_state(source), TableHoldbackState::None),
            "table holdback should ignore pipe lines inside non-markdown blockquoted fences",
        );
    }

    #[test]
    fn incremental_holdback_matches_stateless_scan_per_chunk() {
        let chunks = [
            "status | owner\n",
            "\n",
            "> ```sh\n",
            "> | A | B |\n",
            "> | --- | --- |\n",
            "> ```\n",
            "> | Key | Value |\n",
            "> | --- | --- |\n",
        ];

        let mut scanner = TableHoldbackScanner::new();
        let mut source = String::new();
        for chunk in chunks {
            source.push_str(chunk);
            scanner.push_source_chunk(chunk);
            assert_eq!(
                scanner.state(),
                table_holdback_state(&source),
                "scanner mismatch after chunk: {chunk:?}\nsource:\n{source}",
            );
        }
    }

    #[test]
    fn incremental_holdback_detects_header_delimiter_across_chunk_boundary() {
        let mut scanner = TableHoldbackScanner::new();
        scanner.push_source_chunk("| A | B |\n");
        assert_eq!(
            scanner.state(),
            TableHoldbackState::PendingHeader { header_start: 0 }
        );
        scanner.push_source_chunk("| --- | --- |\n");
        assert_eq!(
            scanner.state(),
            TableHoldbackState::Confirmed { table_start: 0 }
        );
    }

    #[test]
    fn controller_set_width_after_first_line_emit_does_not_requeue_first_line() {
        let mut ctrl = stream_controller(Some(120));
        ctrl.push(
            "FIRSTTOKEN contains enough words to wrap once the width is reduced dramatically.\n",
        );
        ctrl.push("second line remains pending\n");

        let (first_emit, _) = ctrl.on_commit_tick();
        assert!(first_emit.is_some(), "expected first line emission");

        ctrl.set_width(Some(20));

        let (cell, _source) = ctrl.finalize();
        let remaining = cell
            .map(|cell| lines_to_plain_strings(&cell.transcript_lines(u16::MAX)))
            .unwrap_or_default()
            .into_iter()
            .map(|line| line.chars().skip(2).collect::<String>())
            .collect::<Vec<_>>();
        assert!(
            !remaining.iter().any(|line| line.contains("FIRSTTOKEN")),
            "first line should not be re-queued after resize: {remaining:?}",
        );
        assert!(
            remaining.iter().any(|line| line.contains("second line")),
            "expected pending second line after resize: {remaining:?}",
        );
    }

    #[test]
    fn controller_set_width_partial_wrapped_emit_preserves_remaining_content() {
        let mut ctrl = stream_controller(Some(20));
        ctrl.push("The quick brown fox jumps over the lazy dog near the riverbank.\n");
        ctrl.push("tail line\n");

        let (first_emit, idle) = ctrl.on_commit_tick();
        assert!(first_emit.is_some(), "expected first wrapped line emission");
        assert!(!idle, "expected remaining queued content after one tick");
        assert!(
            ctrl.queued_lines() > 0,
            "expected non-empty queue before resize"
        );

        ctrl.set_width(Some(120));

        let (cell, _source) = ctrl.finalize();
        let remaining = cell
            .map(|c| lines_to_plain_strings(&c.transcript_lines(u16::MAX)))
            .unwrap_or_default()
            .into_iter()
            .map(|line| line.chars().skip(2).collect::<String>())
            .collect::<Vec<_>>();
        assert!(
            remaining.iter().any(|line| line.contains("tail line")),
            "un-emitted content should remain after resize remap: {remaining:?}",
        );
    }

    #[test]
    fn controller_set_width_partial_wrapped_emit_keeps_wrapped_remainder() {
        let mut ctrl = stream_controller(Some(18));
        ctrl.push("alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu\n");

        let (first_emit, idle) = ctrl.on_commit_tick();
        assert!(first_emit.is_some(), "expected first wrapped line emission");
        assert!(!idle, "expected remaining wrapped content after one tick");
        assert!(
            ctrl.queued_lines() > 0,
            "expected queued wrapped remainder before resize"
        );

        ctrl.set_width(Some(80));

        let (cell, _source) = ctrl.finalize();
        let remaining = cell
            .map(|c| lines_to_plain_strings(&c.transcript_lines(u16::MAX)))
            .unwrap_or_default();
        let joined = remaining.join(" ");
        assert!(
            joined.contains("kappa") || joined.contains("lambda") || joined.contains("mu"),
            "wrapped remainder from partially emitted source line was lost after resize: {remaining:?}",
        );
    }
}
