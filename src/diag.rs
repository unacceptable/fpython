//! Source spans and compiler diagnostics.

use std::fmt;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone)]
pub struct Diag {
    pub file: String,
    pub span: Span,
    pub msg: String,
    pub notes: Vec<String>,
}

impl Diag {
    pub fn new(file: &str, span: Span, msg: impl Into<String>) -> Diag {
        Diag { file: file.to_string(), span, msg: msg.into(), notes: Vec::new() }
    }

    pub fn note(mut self, n: impl Into<String>) -> Diag {
        self.notes.push(n.into());
        self
    }

    /// Render with a source excerpt and caret.
    pub fn render(&self, src: Option<&str>) -> String {
        let mut out = format!("{}:{}:{}: error: {}\n", self.file, self.span.line, self.span.col, self.msg);
        if let Some(src) = src {
            if let Some(line) = src.lines().nth(self.span.line.saturating_sub(1)) {
                let num = self.span.line.to_string();
                out.push_str(&format!(" {} | {}\n", num, line));
                let pad = " ".repeat(num.len());
                let caret_col = self.span.col.saturating_sub(1);
                let width = if self.span.end > self.span.start {
                    (self.span.end - self.span.start).min(line.len().saturating_sub(caret_col)).max(1)
                } else {
                    1
                };
                out.push_str(&format!(" {} | {}{}\n", pad, " ".repeat(caret_col), "^".repeat(width)));
            }
        }
        for n in &self.notes {
            out.push_str(&format!("  note: {}\n", n));
        }
        out
    }
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}: error: {}", self.file, self.span.line, self.span.col, self.msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_and_display() {
        let d = Diag::new("f.fpy", Span { line: 2, col: 5, start: 10, end: 13 }, "bad thing").note("try this");
        assert_eq!(d.to_string(), "f.fpy:2:5: error: bad thing");
        let r = d.render(Some("first\nlet abc = 1\n"));
        assert!(r.contains(" 2 | let abc = 1\n   |     ^^^\n"), "{}", r);
        assert!(r.ends_with("  note: try this\n"), "{}", r);
        // no source, or a line past the end: just the header
        assert!(!d.render(None).contains('|'));
        let far = Diag::new("f.fpy", Span { line: 9, col: 1, start: 0, end: 0 }, "eof");
        assert!(!far.render(Some("x\n")).contains('|'));
    }
}
