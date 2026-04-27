//! Simple markdown rendering for assistant chat messages.
//!
//! Drives `pulldown_cmark` and translates the event stream into ratatui
//! `Line`s.  Supports a deliberately small subset (see task 832 spec):
//! bold/italic/code spans, headings, ordered & unordered lists, fenced
//! code blocks, blockquotes, thematic breaks, and links.  No tables,
//! HTML, footnotes, etc.
//!
//! Streaming-safe: pulldown-cmark handles unterminated constructs
//! gracefully, so the caller can pass partial input every frame.

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
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
    for event in Parser::new(text) {
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
                self.flush_line();
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
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
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
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
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
}
