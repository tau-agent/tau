//! Simple markdown rendering for assistant chat messages.
//!
//! Drives `pulldown_cmark` and translates the event stream into ratatui
//! `Line`s.  Supports a deliberately small subset (see task 832 spec):
//! bold/italic/code spans, headings, ordered & unordered lists, fenced
//! code blocks, blockquotes, thematic breaks, links, and GFM tables.
//! HTML, footnotes, etc. are not rendered.
//!
//! Streaming-safe: pulldown-cmark handles unterminated constructs
//! gracefully, so the caller can pass partial input every frame.

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// Render markdown text to a vector of styled lines, wrapped to `width`.
///
/// `width` is the full available column count.  Markdown content starts at
/// column 1 (a one-column gutter is prepended on every line, matching the
/// existing assistant rendering) so the usable width for content is
/// `width - 1`.
pub fn render(text: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let usable = (width as usize).saturating_sub(1).max(1);
    let mut renderer = Renderer::new(theme, usable, width as usize);
    for event in Parser::new_ext(text, Options::ENABLE_TABLES) {
        renderer.handle(event);
    }
    renderer.finish()
}

/// Internal state for a single render pass.
struct Renderer<'t> {
    theme: &'t Theme,
    /// Display width for wrapped content (full width minus the 1-col gutter).
    content_width: usize,
    /// Full width including the gutter — used for code-block bg padding.
    full_width: usize,

    /// Modifier stack for nested emphasis (BOLD/ITALIC).
    style_stack: Vec<Modifier>,
    /// Foreground override stack (for links / quotes).
    fg_stack: Vec<ratatui::style::Color>,
    /// Underline override stack (for links).
    underline_stack: Vec<bool>,
    /// List-item numbering: `None` for unordered, `Some(n)` for the next
    /// ordered index.  Stack since lists may (in theory) nest, although we
    /// don't render nested lists with extra indentation in v1.
    list_stack: Vec<Option<u64>>,

    /// True while inside a fenced/indented code block.
    in_code_block: bool,
    /// Buffer for code-block text (joined across multiple Text events).
    code_block_buf: String,

    /// True while inside a block quote.
    in_block_quote: bool,
    /// True if the current pending line should be prefixed with the
    /// blockquote bar on flush.
    pending_quote_prefix: bool,
    /// True if the current pending line should be prefixed with a list bullet.
    pending_list_prefix: Option<String>,

    /// Spans accumulated for the line currently being built.
    current_spans: Vec<Span<'static>>,

    /// Output blocks.  Each block is one or more lines; blank lines are
    /// inserted between blocks at finish time.
    blocks: Vec<Vec<Line<'static>>>,

    /// True while inside a GFM table.
    in_table: bool,
    /// Per-column alignment captured from `Tag::Table(alignments)`.
    table_alignments: Vec<Alignment>,
    /// True while inside the table head.
    table_in_head: bool,
    /// True while inside a table cell — inline events route into the
    /// regular `current_spans` buffer; on cell end we move the buffer
    /// into the current row.
    table_in_cell: bool,
    /// Cells of the row currently being built.
    table_current_row: Vec<Vec<Span<'static>>>,
    /// Header row (single row of cells, each a vec of styled spans).
    table_header: Vec<Vec<Span<'static>>>,
    /// Body rows.
    table_body: Vec<Vec<Vec<Span<'static>>>>,
}

impl<'t> Renderer<'t> {
    fn new(theme: &'t Theme, content_width: usize, full_width: usize) -> Self {
        Self {
            theme,
            content_width,
            full_width,
            style_stack: Vec::new(),
            fg_stack: Vec::new(),
            underline_stack: Vec::new(),
            list_stack: Vec::new(),
            in_code_block: false,
            code_block_buf: String::new(),
            in_block_quote: false,
            pending_quote_prefix: false,
            pending_list_prefix: None,
            current_spans: Vec::new(),
            blocks: Vec::new(),
            in_table: false,
            table_alignments: Vec::new(),
            table_in_head: false,
            table_in_cell: false,
            table_current_row: Vec::new(),
            table_header: Vec::new(),
            table_body: Vec::new(),
        }
    }

    /// Build the current style flags from the inline stacks.
    fn current_style(&self) -> Style {
        let mut style = Style::default();
        let mut modifier = Modifier::empty();
        for m in &self.style_stack {
            modifier |= *m;
        }
        if self.underline_stack.last().copied().unwrap_or(false) {
            modifier |= Modifier::UNDERLINED;
        }
        if !modifier.is_empty() {
            style = style.add_modifier(modifier);
        }
        if let Some(c) = self.fg_stack.last() {
            style = style.fg(*c);
        }
        style
    }

    /// Append text to the current line in the active inline style.
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Split on hard newlines so they trigger a line flush.
        let mut first = true;
        for part in text.split('\n') {
            if !first {
                self.flush_line();
            }
            first = false;
            if !part.is_empty() {
                let style = self.current_style();
                self.current_spans
                    .push(Span::styled(part.to_string(), style));
            }
        }
    }

    /// Flush whatever inline content is in `current_spans` as a single
    /// (logical) line into the current block.  Width-aware wrapping is
    /// applied; the prefix (quote bar / list bullet) is repeated on
    /// continuation lines as a hanging indent.
    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.current_spans);

        // Determine prefix and continuation prefix.
        let (prefix, continuation): (Vec<Span<'static>>, Vec<Span<'static>>) =
            if self.pending_quote_prefix {
                let bar = Span::styled(
                    "│ ".to_string(),
                    Style::default().fg(self.theme.markdown_quote.to_ratatui()),
                );
                (vec![bar.clone()], vec![bar])
            } else if let Some(bullet) = self.pending_list_prefix.take() {
                // Hanging indent: continuation lines get a blank prefix of
                // the same display width as the bullet.
                let bullet_w = UnicodeWidthStr::width(bullet.as_str());
                let cont = " ".repeat(bullet_w);
                (vec![Span::raw(bullet)], vec![Span::raw(cont)])
            } else {
                (Vec::new(), Vec::new())
            };

        // Reset per-line flags (quote prefix is set fresh at quote-open).
        self.pending_quote_prefix = self.in_block_quote;

        let prefix_w = prefix.iter().map(|s| s.width()).sum::<usize>();
        let body_width = self.content_width.saturating_sub(prefix_w).max(1);

        let lines = if spans.is_empty() {
            vec![Vec::<Span<'static>>::new()]
        } else {
            wrap_spans(&spans, body_width)
        };

        let block: &mut Vec<Line<'static>> = self.ensure_open_block();
        for (i, mut row) in lines.into_iter().enumerate() {
            let mut combined: Vec<Span<'static>> = Vec::new();
            // Leading 1-column gutter (matches existing assistant rendering).
            combined.push(Span::raw(" "));
            if i == 0 {
                combined.extend(prefix.iter().cloned());
            } else {
                combined.extend(continuation.iter().cloned());
            }
            combined.append(&mut row);
            block.push(Line::from(combined));
        }
    }

    fn ensure_open_block(&mut self) -> &mut Vec<Line<'static>> {
        if self.blocks.is_empty() {
            self.blocks.push(Vec::new());
        }
        let idx = self.blocks.len() - 1;
        &mut self.blocks[idx]
    }

    /// Close the current block, opening a fresh one for the next batch of
    /// content.  Blank lines between blocks are inserted at finish time.
    fn close_block(&mut self) {
        // Drop trailing empty block to avoid duplicating blanks.
        if let Some(last) = self.blocks.last()
            && last.is_empty()
        {
            return;
        }
        self.blocks.push(Vec::new());
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if self.in_code_block {
                    self.code_block_buf.push_str(&t);
                } else {
                    self.push_text(&t);
                }
            }
            Event::Code(code) => {
                let style = Style::default()
                    .fg(self.theme.markdown_code_fg.to_ratatui())
                    .bg(self.theme.markdown_code_bg.to_ratatui());
                self.current_spans
                    .push(Span::styled(code.to_string(), style));
            }
            Event::SoftBreak => {
                // Treat soft breaks as a space within the same paragraph.
                let style = self.current_style();
                self.current_spans
                    .push(Span::styled(" ".to_string(), style));
            }
            Event::HardBreak => {
                if self.in_table && self.table_in_cell {
                    // Inside a table cell, hard breaks collapse to a space
                    // so the cell stays a single logical line.
                    let style = self.current_style();
                    self.current_spans
                        .push(Span::styled(" ".to_string(), style));
                } else {
                    self.flush_line();
                }
            }
            Event::Rule => {
                // Flush anything pending, then emit a horizontal rule line.
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                let bar: String = "─".repeat(self.content_width);
                let style = Style::default().fg(self.theme.markdown_rule.to_ratatui());
                let block = self.ensure_open_block();
                block.push(Line::from(vec![Span::raw(" "), Span::styled(bar, style)]));
                self.close_block();
            }
            // Things we deliberately drop in v1.
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                // Nothing to do — text events accumulate into current_spans.
            }
            Tag::Heading { level, .. } => {
                // Headings start fresh; emit BOLD + heading fg.
                self.style_stack.push(Modifier::BOLD);
                let heading_color = self.theme.markdown_heading.to_ratatui();
                self.fg_stack.push(heading_color);
                // Optional: prepend the leading `#` markers as a visual cue.
                let hashes = match level {
                    HeadingLevel::H1 => "# ",
                    HeadingLevel::H2 => "## ",
                    HeadingLevel::H3 => "### ",
                    HeadingLevel::H4 => "#### ",
                    HeadingLevel::H5 => "##### ",
                    HeadingLevel::H6 => "###### ",
                };
                let style = self.current_style();
                self.current_spans
                    .push(Span::styled(hashes.to_string(), style));
            }
            Tag::Emphasis => {
                self.style_stack.push(Modifier::ITALIC);
            }
            Tag::Strong => {
                self.style_stack.push(Modifier::BOLD);
            }
            Tag::Strikethrough => {
                self.style_stack.push(Modifier::CROSSED_OUT);
            }
            Tag::Link { .. } => {
                self.fg_stack.push(self.theme.markdown_link.to_ratatui());
                self.underline_stack.push(true);
            }
            Tag::Image { .. } => {
                // Skipped in v1; treat as link-like for nested text rendering
                // so alt text still appears.
            }
            Tag::CodeBlock(_kind) => {
                // Flush any open inline content first.
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                self.in_code_block = true;
                self.code_block_buf.clear();
            }
            Tag::List(start) => {
                self.list_stack.push(start);
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
            }
            Tag::Item => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                let prefix = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let s = format!(" {}. ", n);
                        *n += 1;
                        s
                    }
                    _ => " • ".to_string(),
                };
                self.pending_list_prefix = Some(prefix);
            }
            Tag::BlockQuote(_) => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                self.in_block_quote = true;
                self.pending_quote_prefix = true;
                // Italicise the body of block quotes.
                self.style_stack.push(Modifier::ITALIC);
                self.fg_stack.push(self.theme.markdown_quote.to_ratatui());
            }
            // Tags we don't render in v1.
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
            Tag::Table(alignments) => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                self.in_table = true;
                self.table_alignments = alignments;
                self.table_in_head = false;
                self.table_in_cell = false;
                self.table_current_row.clear();
                self.table_header.clear();
                self.table_body.clear();
            }
            Tag::TableHead => {
                self.table_in_head = true;
                self.table_current_row.clear();
            }
            Tag::TableRow => {
                self.table_current_row.clear();
            }
            Tag::TableCell => {
                self.table_in_cell = true;
                // Defensive: any stray inline content shouldn't accumulate
                // between cells. Drop it rather than risk leaking into the
                // next cell.
                self.current_spans.clear();
            }
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                self.close_block();
            }
            TagEnd::Heading(_) => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                self.style_stack.pop();
                self.fg_stack.pop();
                self.close_block();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.style_stack.pop();
            }
            TagEnd::Link => {
                self.fg_stack.pop();
                self.underline_stack.pop();
            }
            TagEnd::Image => {}
            TagEnd::CodeBlock => {
                let buf = std::mem::take(&mut self.code_block_buf);
                self.in_code_block = false;
                let style = Style::default()
                    .fg(self.theme.markdown_code_fg.to_ratatui())
                    .bg(self.theme.markdown_code_bg.to_ratatui());
                let content_width = self.content_width;
                let full_width = self.full_width;
                let block = self.ensure_open_block();
                // Strip a single trailing newline (pulldown emits one).
                let body = buf.strip_suffix('\n').unwrap_or(&buf);
                let lines: Vec<&str> = if body.is_empty() {
                    vec![""]
                } else {
                    body.split('\n').collect()
                };
                for raw_line in lines {
                    let mut row_w = UnicodeWidthStr::width(raw_line);
                    let mut content = raw_line.to_string();
                    // If the line is wider than the available content width,
                    // hard-truncate at the column boundary.  This is a soft
                    // wrap fallback — long code lines are ugly but never
                    // panic.
                    if row_w > content_width {
                        let mut acc = String::new();
                        let mut w = 0;
                        for ch in content.chars() {
                            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                            if w + cw > content_width {
                                break;
                            }
                            w += cw;
                            acc.push(ch);
                        }
                        content = acc;
                        row_w = w;
                    }
                    let pad = full_width.saturating_sub(row_w + 1); // +1 gutter
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(content, style));
                    if pad > 0 {
                        spans.push(Span::styled(" ".repeat(pad), style));
                    }
                    block.push(Line::from(spans));
                }
                self.close_block();
            }
            TagEnd::List(_) => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                self.list_stack.pop();
                self.close_block();
            }
            TagEnd::Item => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                // Drop any unused list prefix (empty item).
                self.pending_list_prefix = None;
            }
            TagEnd::BlockQuote(_) => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                self.style_stack.pop();
                self.fg_stack.pop();
                self.in_block_quote = false;
                self.pending_quote_prefix = false;
                self.close_block();
            }
            // No-op for v1 unsupported tags.
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.current_spans);
                self.table_current_row.push(cell);
                self.table_in_cell = false;
            }
            TagEnd::TableRow => {
                let row = std::mem::take(&mut self.table_current_row);
                if self.table_in_head {
                    self.table_header = row;
                } else {
                    self.table_body.push(row);
                }
            }
            TagEnd::TableHead => {
                // Some pulldown-cmark versions don't emit an enclosing
                // TableRow inside the head; if cells were collected
                // directly, promote them here.
                if !self.table_current_row.is_empty() {
                    self.table_header = std::mem::take(&mut self.table_current_row);
                }
                self.table_in_head = false;
            }
            TagEnd::Table => {
                self.emit_table();
                self.in_table = false;
                self.table_alignments.clear();
                self.table_in_head = false;
                self.table_in_cell = false;
                self.table_current_row.clear();
                self.table_header.clear();
                self.table_body.clear();
                self.close_block();
            }
        }
    }

    /// Render the buffered table rows as an aligned text grid into the
    /// current block.  Truncates cells with `…` when they exceed their
    /// column budget; honours per-column alignment.
    fn emit_table(&mut self) {
        // If anything is still pending in the current row (e.g. a
        // streaming-truncated table without a final TableRow end),
        // promote it to the body so the partial row still renders.
        if !self.table_current_row.is_empty() {
            let row = std::mem::take(&mut self.table_current_row);
            if self.table_in_head && self.table_header.is_empty() {
                self.table_header = row;
            } else {
                self.table_body.push(row);
            }
        }

        let header = std::mem::take(&mut self.table_header);
        let body = std::mem::take(&mut self.table_body);
        let alignments = self.table_alignments.clone();

        // Determine column count.
        let n_cols = std::iter::once(header.len())
            .chain(body.iter().map(|r| r.len()))
            .max()
            .unwrap_or(0);
        if n_cols == 0 {
            return;
        }

        // Pad rows to n_cols.
        let mut header = header;
        header.resize_with(n_cols, Vec::new);
        let body: Vec<Vec<Vec<Span<'static>>>> = body
            .into_iter()
            .map(|mut r| {
                r.resize_with(n_cols, Vec::new);
                r
            })
            .collect();

        // Per-column alignments (default to None / Left).
        let mut aligns = alignments;
        aligns.resize(n_cols, Alignment::None);

        // Natural per-column widths.
        let mut col_w: Vec<usize> = vec![1; n_cols];
        for (c, cell) in header.iter().enumerate() {
            col_w[c] = col_w[c].max(cell_display_width(cell));
        }
        for row in &body {
            for (c, cell) in row.iter().enumerate() {
                col_w[c] = col_w[c].max(cell_display_width(cell));
            }
        }
        for w in col_w.iter_mut() {
            if *w == 0 {
                *w = 1;
            }
        }

        // Total width = sum(col_w + 2 padding) + (n_cols + 1) borders.
        let content_width = self.content_width;
        let overhead = (n_cols + 1) + 2 * n_cols; // borders + 1 pad on each side
        let natural_total: usize = col_w.iter().sum::<usize>() + overhead;
        if natural_total > content_width {
            // Shrink columns proportionally to fit.
            let available = content_width.saturating_sub(overhead);
            if available < n_cols {
                // Degenerate case: not even 1 column per col.  Give each
                // column 1 char; the right border will overflow but every
                // line is still produced.
                for w in col_w.iter_mut() {
                    *w = 1;
                }
            } else {
                let total: usize = col_w.iter().sum();
                let mut new_w: Vec<usize> = col_w
                    .iter()
                    .map(|w| {
                        let scaled = (*w as u128 * available as u128) / total as u128;
                        (scaled as usize).max(1)
                    })
                    .collect();
                // Distribute remainder so the sum equals `available`.
                let mut sum: usize = new_w.iter().sum();
                let mut idx = 0;
                while sum < available && !new_w.is_empty() {
                    new_w[idx % n_cols] += 1;
                    sum += 1;
                    idx += 1;
                }
                while sum > available && !new_w.is_empty() {
                    // Trim the widest column down by 1 until we fit.
                    let mut max_i = 0;
                    for (i, w) in new_w.iter().enumerate() {
                        if *w > new_w[max_i] {
                            max_i = i;
                        }
                    }
                    if new_w[max_i] <= 1 {
                        break;
                    }
                    new_w[max_i] -= 1;
                    sum -= 1;
                }
                col_w = new_w;
            }
        }

        let border_style = Style::default().fg(self.theme.markdown_rule.to_ratatui());

        // Build border-line variants.
        let make_border = |left: char, mid: char, right: char| -> Line<'static> {
            let mut s = String::new();
            s.push(left);
            for (i, w) in col_w.iter().enumerate() {
                for _ in 0..(w + 2) {
                    s.push('─');
                }
                if i + 1 < col_w.len() {
                    s.push(mid);
                }
            }
            s.push(right);
            Line::from(vec![Span::raw(" "), Span::styled(s, border_style)])
        };

        let top = make_border('┌', '┬', '┐');
        let sep = make_border('├', '┼', '┤');
        let bot = make_border('└', '┴', '┘');

        let bar = || Span::styled("│".to_string(), border_style);
        let pad_space = || Span::raw(" ".to_string());

        // Render a single content row into a Line.
        let render_row = |row: Vec<Vec<Span<'static>>>, bold: bool| -> Line<'static> {
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::raw(" "));
            spans.push(bar());
            for (c, cell) in row.into_iter().enumerate() {
                spans.push(pad_space());
                let mut shaped = truncate_and_pad(cell, col_w[c], aligns[c]);
                if bold {
                    for span in shaped.iter_mut() {
                        let style = span.style.add_modifier(Modifier::BOLD);
                        span.style = style;
                    }
                }
                spans.append(&mut shaped);
                spans.push(pad_space());
                spans.push(bar());
            }
            Line::from(spans)
        };

        let header_line = render_row(header, true);
        let body_lines: Vec<Line<'static>> =
            body.into_iter().map(|r| render_row(r, false)).collect();

        let block = self.ensure_open_block();
        block.push(top);
        block.push(header_line);
        block.push(sep);
        for l in body_lines {
            block.push(l);
        }
        block.push(bot);
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        // Flush any trailing inline content (e.g. unterminated paragraph
        // during streaming).
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
        // Flush any unterminated code block (streaming).
        if self.in_code_block {
            // Pretend a CodeBlock end happened so the buffered content is
            // emitted.
            self.end(TagEnd::CodeBlock);
        }
        // Flush any unterminated table (streaming).
        if self.in_table {
            self.end(TagEnd::Table);
        }

        // Stitch blocks together with single blank separators.
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut first = true;
        for block in self.blocks.into_iter() {
            if block.is_empty() {
                continue;
            }
            if !first {
                out.push(Line::from(""));
            }
            first = false;
            out.extend(block);
        }
        out
    }
}

/// Sum the display width (`UnicodeWidthStr`) of every span's content.
fn cell_display_width(cell: &[Span<'_>]) -> usize {
    cell.iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// Truncate `cell` to `col_w` display columns (appending `…` if it
/// overflowed) and pad with spaces to exactly `col_w` columns according
/// to `align`.  Styles on the original spans are preserved through the
/// truncation; padding spaces are unstyled.
fn truncate_and_pad(
    cell: Vec<Span<'static>>,
    col_w: usize,
    align: Alignment,
) -> Vec<Span<'static>> {
    if col_w == 0 {
        return Vec::new();
    }

    // Flatten into (char, width, style) cells.
    #[derive(Clone)]
    struct Cell {
        ch: char,
        w: usize,
        style: Style,
    }
    let mut cells: Vec<Cell> = Vec::new();
    for span in &cell {
        let style = span.style;
        for ch in span.content.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            cells.push(Cell { ch, w, style });
        }
    }

    let total_w: usize = cells.iter().map(|c| c.w).sum();
    let kept = if total_w <= col_w {
        cells
    } else {
        // Need to leave room for an ellipsis (width 1).
        let budget = col_w.saturating_sub(1);
        let mut acc_w = 0usize;
        let mut taken = 0usize;
        for c in &cells {
            if acc_w + c.w > budget {
                break;
            }
            acc_w += c.w;
            taken += 1;
        }
        let mut kept: Vec<Cell> = cells.into_iter().take(taken).collect();
        let ellipsis_style = kept.last().map(|c| c.style).unwrap_or_default();
        kept.push(Cell {
            ch: '…',
            w: 1,
            style: ellipsis_style,
        });
        kept
    };

    // Coalesce contiguous cells with equal style into spans.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut current_style: Option<Style> = None;
    let mut content_w = 0usize;
    for c in kept {
        content_w += c.w;
        match current_style {
            Some(s) if s == c.style => buf.push(c.ch),
            _ => {
                if let Some(s) = current_style.take() {
                    spans.push(Span::styled(std::mem::take(&mut buf), s));
                }
                buf.push(c.ch);
                current_style = Some(c.style);
            }
        }
    }
    if let Some(s) = current_style {
        spans.push(Span::styled(buf, s));
    }

    // Pad to col_w according to alignment.
    let pad_total = col_w.saturating_sub(content_w);
    if pad_total > 0 {
        match align {
            Alignment::Right => {
                let mut out: Vec<Span<'static>> = Vec::new();
                out.push(Span::raw(" ".repeat(pad_total)));
                out.extend(spans);
                return out;
            }
            Alignment::Center => {
                let left = pad_total / 2;
                let right = pad_total - left;
                let mut out: Vec<Span<'static>> = Vec::new();
                if left > 0 {
                    out.push(Span::raw(" ".repeat(left)));
                }
                out.extend(spans);
                if right > 0 {
                    out.push(Span::raw(" ".repeat(right)));
                }
                return out;
            }
            Alignment::Left | Alignment::None => {
                spans.push(Span::raw(" ".repeat(pad_total)));
            }
        }
    }
    spans
}

/// Wrap a sequence of styled spans to `width` display columns, splitting
/// spans at wrap points so each span's `Style` is preserved on both sides.
///
/// Tries word-boundary breaks first (split on the trailing space of a
/// word), falls back to a hard break inside a word.  Never produces a
/// zero-width row when the input contains a single character wider than
/// `width`.
pub fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    if width == 0 {
        return vec![spans.to_vec()];
    }

    // Flatten the spans into a sequence of (char, style) pairs so we can
    // do width math, then re-coalesce into spans per output row.
    #[derive(Clone)]
    struct Cell {
        ch: char,
        w: usize,
        style: Style,
    }

    let mut cells: Vec<Cell> = Vec::new();
    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            cells.push(Cell { ch, w, style });
        }
    }

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut i = 0;
    while i < cells.len() {
        let mut acc_w = 0usize;
        let mut last_space: Option<usize> = None; // index *after* the space
        let mut cut = cells.len();
        let mut j = i;
        while j < cells.len() {
            let cw = cells[j].w;
            if acc_w + cw > width {
                cut = j;
                break;
            }
            acc_w += cw;
            if cells[j].ch == ' ' {
                last_space = Some(j + 1);
            }
            j += 1;
        }
        let end = if cut == cells.len() {
            cells.len()
        } else {
            // Try word-boundary break first.
            match last_space {
                Some(s) if s > i => s,
                _ => {
                    // Hard break — but ensure progress.
                    if cut == i { i + 1 } else { cut }
                }
            }
        };
        rows.push(cells[i..end].to_vec());
        i = end;
    }

    // Re-coalesce contiguous cells with equal style into spans.
    rows.into_iter()
        .map(|row| {
            let mut out: Vec<Span<'static>> = Vec::new();
            let mut buf = String::new();
            let mut current_style: Option<Style> = None;
            for cell in row {
                match current_style {
                    Some(s) if s == cell.style => buf.push(cell.ch),
                    _ => {
                        if let Some(s) = current_style.take() {
                            out.push(Span::styled(std::mem::take(&mut buf), s));
                        }
                        buf.push(cell.ch);
                        current_style = Some(cell.style);
                    }
                }
            }
            if let Some(s) = current_style {
                out.push(Span::styled(buf, s));
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use ratatui::style::Modifier;

    fn theme() -> crate::theme::Theme {
        theme::dark()
    }

    /// Concatenate the textual content of all spans on a line.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Find the first span on `line` whose content contains `needle`.
    fn span_with<'a, 'b>(line: &'a Line<'b>, needle: &str) -> Option<&'a Span<'b>> {
        line.spans.iter().find(|s| s.content.contains(needle))
    }

    // --- Inline constructs -------------------------------------------------

    #[test]
    fn bold_emits_bold_span() {
        let lines = render("**bold**", 40, &theme());
        assert!(!lines.is_empty());
        let span = span_with(&lines[0], "bold").expect("bold span");
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn italic_emits_italic_span() {
        let lines = render("*italic*", 40, &theme());
        let span = span_with(&lines[0], "italic").expect("italic span");
        assert!(span.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn code_span_uses_code_colors() {
        let t = theme();
        let lines = render("`code`", 40, &t);
        let span = span_with(&lines[0], "code").expect("code span");
        assert_eq!(span.style.fg, Some(t.markdown_code_fg.to_ratatui()));
        assert_eq!(span.style.bg, Some(t.markdown_code_bg.to_ratatui()));
    }

    #[test]
    fn link_underlined_with_link_color() {
        let t = theme();
        let lines = render("[click](http://x)", 40, &t);
        let span = span_with(&lines[0], "click").expect("link span");
        assert!(span.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(span.style.fg, Some(t.markdown_link.to_ratatui()));
        // URL must not appear.
        let text = line_text(&lines[0]);
        assert!(
            !text.contains("http://x"),
            "url leaked into output: {:?}",
            text
        );
    }

    #[test]
    fn nested_bold_italic_combines_modifiers() {
        let lines = render("**bold *and italic***", 40, &theme());
        // Find the span for "and italic"
        let span = span_with(&lines[0], "and italic").expect("inner span");
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert!(span.style.add_modifier.contains(Modifier::ITALIC));
    }

    // --- Block constructs --------------------------------------------------

    #[test]
    fn heading_is_bold() {
        let lines = render("# Title", 40, &theme());
        assert!(!lines.is_empty());
        let span = span_with(&lines[0], "Title").expect("title span");
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn unordered_list_uses_bullet() {
        let lines = render("- one\n- two", 40, &theme());
        // Find the lines containing the bullet characters.
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts.iter().any(|t| t.contains(" • ") && t.contains("one")),
            "expected bullet+one in {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| t.contains(" • ") && t.contains("two")),
            "expected bullet+two in {:?}",
            texts
        );
    }

    #[test]
    fn ordered_list_numbers_items() {
        let lines = render("1. first\n2. second", 40, &theme());
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains(" 1. ") && t.contains("first")),
            "missing '1. first': {:?}",
            texts
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains(" 2. ") && t.contains("second")),
            "missing '2. second': {:?}",
            texts
        );
    }

    #[test]
    fn fenced_code_block_pads_to_width() {
        let t = theme();
        let width = 20u16;
        let lines = render("```\nhi\n```", width, &t);
        // First non-empty line should be the code line, padded to full width.
        let code_line = lines
            .iter()
            .find(|l| line_text(l).contains("hi"))
            .expect("code line");
        assert_eq!(
            code_line.width(),
            width as usize,
            "code line not padded to width: {:?}",
            line_text(code_line)
        );
        // The "hi" span should carry the code bg.
        let span = span_with(code_line, "hi").expect("hi span");
        assert_eq!(span.style.bg, Some(t.markdown_code_bg.to_ratatui()));
    }

    #[test]
    fn blockquote_has_left_bar_and_italic() {
        let t = theme();
        let lines = render("> quoted text", 40, &t);
        let q_line = lines
            .iter()
            .find(|l| line_text(l).contains("quoted text"))
            .expect("quoted line");
        let text = line_text(q_line);
        assert!(text.contains("│"), "expected quote bar in {:?}", text);
        let span = span_with(q_line, "quoted text").expect("body span");
        assert!(span.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(span.style.fg, Some(t.markdown_quote.to_ratatui()));
    }

    #[test]
    fn thematic_break_emits_rule() {
        let t = theme();
        let width = 30u16;
        let lines = render("---", width, &t);
        let rule_line = lines
            .iter()
            .find(|l| line_text(l).contains("─"))
            .expect("rule line");
        let span = span_with(rule_line, "─").expect("rule span");
        assert_eq!(span.style.fg, Some(t.markdown_rule.to_ratatui()));
    }

    // --- Streaming-style unterminated input --------------------------------

    #[test]
    fn unterminated_bold_does_not_panic() {
        let lines = render("hello **wo", 40, &theme());
        assert!(!lines.is_empty());
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("hello"));
    }

    #[test]
    fn unterminated_code_fence_does_not_panic() {
        let lines = render("```\ncode without close\n", 40, &theme());
        assert!(!lines.is_empty());
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("code without close"));
    }

    // --- Width-aware wrapping with styles ----------------------------------

    #[test]
    fn long_bold_wraps_and_preserves_style() {
        // Width 20 (so usable=19) — the bold sentence will wrap.
        let text = "**alpha beta gamma delta epsilon zeta eta theta**";
        let lines = render(text, 20, &theme());
        // At least two output lines.
        assert!(lines.len() >= 2, "expected wrap, got {} lines", lines.len());
        // Every span containing letters from our sentence should be BOLD.
        for line in &lines {
            for span in &line.spans {
                if span.content.chars().any(|c| c.is_ascii_alphabetic()) {
                    assert!(
                        span.style.add_modifier.contains(Modifier::BOLD),
                        "span lost bold: {:?}",
                        span.content
                    );
                }
            }
        }
    }

    #[test]
    fn paragraphs_separated_by_blank_line() {
        let lines = render("first paragraph\n\nsecond paragraph", 40, &theme());
        // Find indexes of the two paragraph lines.
        let first = lines
            .iter()
            .position(|l| line_text(l).contains("first paragraph"))
            .expect("first");
        let second = lines
            .iter()
            .position(|l| line_text(l).contains("second paragraph"))
            .expect("second");
        assert!(second > first + 1, "expected blank between paragraphs");
        // The line between is blank-ish (no visible content).
        let between = &lines[first + 1];
        assert_eq!(between.width(), 0);
    }

    #[test]
    fn style_does_not_leak_between_paragraphs() {
        let lines = render("**bold**\n\nplain text", 40, &theme());
        let plain_line = lines
            .iter()
            .find(|l| line_text(l).contains("plain text"))
            .expect("plain line");
        let span = span_with(plain_line, "plain text").expect("plain span");
        assert!(
            !span.style.add_modifier.contains(Modifier::BOLD),
            "bold leaked into next paragraph"
        );
    }

    // --- wrap_spans direct tests -------------------------------------------

    #[test]
    fn wrap_spans_word_boundary() {
        let spans = vec![Span::styled(
            "hello world foo".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        let rows = wrap_spans(&spans, 11);
        assert_eq!(rows.len(), 2);
        // Both rows preserve BOLD.
        for row in &rows {
            for s in row {
                if !s.content.trim().is_empty() {
                    assert!(s.style.add_modifier.contains(Modifier::BOLD));
                }
            }
        }
    }

    #[test]
    fn wrap_spans_progresses_on_wide_char() {
        // CJK char (width 2) at width 1: must still progress one char at a time.
        let spans = vec![Span::raw("\u{4e00}\u{4e8c}")];
        let rows = wrap_spans(&spans, 1);
        assert_eq!(rows.len(), 2);
        // Each row has at least the one char.
        for row in &rows {
            assert!(!row.is_empty());
        }
    }

    #[test]
    fn wrap_spans_splits_styled_span_on_boundary() {
        // Single bold span with a space at position that triggers wrap.
        let spans = vec![Span::styled(
            "abcd efgh ijkl".to_string(),
            Style::default().add_modifier(Modifier::ITALIC),
        )];
        let rows = wrap_spans(&spans, 5);
        assert!(rows.len() >= 2);
        for row in &rows {
            for s in row {
                if !s.content.trim().is_empty() {
                    assert!(s.style.add_modifier.contains(Modifier::ITALIC));
                }
            }
        }
    }

    // --- Tables -------------------------------------------------------------

    #[test]
    fn table_basic_renders_grid() {
        let lines = render("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |", 40, &theme());
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let joined = texts.join("\n");
        // Border glyphs.
        assert!(joined.contains('┌'), "missing top-left in {:?}", joined);
        assert!(joined.contains('┐'), "missing top-right in {:?}", joined);
        assert!(joined.contains('└'), "missing bot-left in {:?}", joined);
        assert!(joined.contains('┘'), "missing bot-right in {:?}", joined);
        assert!(joined.contains('├'), "missing left-tee in {:?}", joined);
        assert!(joined.contains('┴'), "missing bot-tee in {:?}", joined);
        // Header cells must be present and BOLD; body cells present and not
        // bold.
        let header_a = lines
            .iter()
            .find_map(|l| span_with(l, "a").map(|s| s.style.clone()))
            .expect("header span 'a'");
        assert!(header_a.add_modifier.contains(Modifier::BOLD));
        let body_one = lines
            .iter()
            .find_map(|l| span_with(l, "1").map(|s| s.style.clone()))
            .expect("body span '1'");
        assert!(!body_one.add_modifier.contains(Modifier::BOLD));
        // 4 must also appear.
        assert!(
            joined.contains('4'),
            "missing body cell '4' in {:?}",
            joined
        );
    }

    #[test]
    fn table_alignment_markers_respected() {
        // Three columns: left, center, right.  Cell contents are single
        // chars in a column wider than the content so the padding
        // direction is unambiguous.
        let src = "\
| LH | CH | RH |\n\
|:---|:---:|---:|\n\
| L | C | R |";
        let lines = render(src, 60, &theme());
        // Pick the body row line (contains 'L', 'C', 'R' single-letter cells).
        let body_line = lines
            .iter()
            .find(|l| {
                let t = line_text(l);
                // Body row, not header (which contains "LH").
                t.contains(" L ") || t.contains(" L ") && !t.contains("LH")
            })
            .expect("body row");
        let text = line_text(body_line);
        // Split on the vertical bar and inspect each cell's padding.
        let parts: Vec<&str> = text.split('│').collect();
        // parts[0] is the leading gutter, parts[1..] are cells (last empty).
        // Each cell content is `" " + padded + " "`.
        // Left-aligned: data char comes right after the leading pad-space.
        let left_cell = parts.get(1).expect("left cell");
        // " L    " or similar: trim the outer single-space padding then
        // confirm 'L' is the first non-space char.
        let trimmed = left_cell.trim_start_matches(' ');
        assert!(
            trimmed.starts_with('L'),
            "left cell not left-aligned: {:?}",
            left_cell
        );
        // Right-aligned: 'R' should be the last non-space char before the
        // trailing single-space padding.
        let right_cell = parts.get(3).expect("right cell");
        let trimmed_r = right_cell.trim_end_matches(' ');
        assert!(
            trimmed_r.ends_with('R'),
            "right cell not right-aligned: {:?}",
            right_cell
        );
        // Center-aligned: 'C' should have whitespace on both sides within
        // the cell padding.
        let center_cell = parts.get(2).expect("center cell");
        let inner = center_cell.trim_matches(' ');
        // After trimming the outer single-space pad, there should still be
        // padding on at least one side (header "CH" forces col_w >= 2).
        // For "C" in a >=2-wide column, expect leading or trailing space
        // around the 'C' inside the cell content.
        let _ = inner;
        let chars: Vec<char> = center_cell.chars().collect();
        // Find the index of 'C'.
        let c_idx = chars.iter().position(|c| *c == 'C').expect("C present");
        // There must be at least one space before AND after C inside the cell.
        assert!(
            c_idx > 0 && chars.get(c_idx - 1) == Some(&' '),
            "center cell missing left padding: {:?}",
            center_cell
        );
        assert!(
            chars.get(c_idx + 1) == Some(&' '),
            "center cell missing right padding: {:?}",
            center_cell
        );
    }

    #[test]
    fn table_wider_than_width_does_not_panic() {
        let src = "\
| really long header A | really long header B | really long header C |\n\
|---|---|---|\n\
| aaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbb | cccccccccccccc |\n\
\nafter table";
        let width: u16 = 20;
        let lines = render(src, width, &theme());
        assert!(!lines.is_empty(), "expected output");
        // Every line's display width <= width.
        for line in &lines {
            assert!(
                line.width() <= width as usize,
                "line wider than width: width={} line={:?}",
                line.width(),
                line_text(line),
            );
        }
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains('…'),
            "expected ellipsis truncation marker in {:?}",
            joined
        );
        // The trailing paragraph still renders.
        assert!(
            joined.contains("after table"),
            "trailing paragraph missing in {:?}",
            joined
        );
    }

    #[test]
    fn table_inline_styles_preserved() {
        let src = "| **bold** | *italic* | `code` |\n|---|---|---|\n| a | b | c |";
        let t = theme();
        let lines = render(src, 60, &t);
        // Header span 'bold' should be BOLD (header overlay BOLD also).
        let bold_span = lines
            .iter()
            .find_map(|l| span_with(l, "bold"))
            .expect("bold header span");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
        let italic_span = lines
            .iter()
            .find_map(|l| span_with(l, "italic"))
            .expect("italic header span");
        assert!(italic_span.style.add_modifier.contains(Modifier::ITALIC));
        let code_span = lines
            .iter()
            .find_map(|l| span_with(l, "code"))
            .expect("code header span");
        assert_eq!(code_span.style.fg, Some(t.markdown_code_fg.to_ratatui()));
        assert_eq!(code_span.style.bg, Some(t.markdown_code_bg.to_ratatui()));
    }

    #[test]
    fn table_followed_by_paragraph_separator() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n\nafter";
        let lines = render(src, 40, &theme());
        let bottom_idx = lines
            .iter()
            .position(|l| line_text(l).contains('└'))
            .expect("bottom border");
        let after_idx = lines
            .iter()
            .position(|l| line_text(l).contains("after"))
            .expect("after paragraph");
        assert!(
            after_idx > bottom_idx + 1,
            "expected blank line between table and paragraph (bottom={}, after={})",
            bottom_idx,
            after_idx,
        );
        let between = &lines[bottom_idx + 1];
        assert_eq!(between.width(), 0);
    }
}
