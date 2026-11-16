//! Syntax highlighting with embedded Sublime/bat grammars. Enabled by default.
//! Cache parser state after each line so normal cursor movement never reparses it.

use std::path::PathBuf;

use crossterm::style::Color;
use two_face::re_exports::syntect::{
    easy::HighlightLines,
    highlighting::{HighlightState, Theme, ThemeSet},
    parsing::{ParseState, SyntaxReference, SyntaxSet},
};

use crate::document::Document;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub foreground: Color,
}

struct CachedLine {
    state: (HighlightState, ParseState),
    spans: Vec<Span>,
}

pub struct SyntaxEngine {
    syntaxes: SyntaxSet,
    theme: Theme,
    path: Option<PathBuf>,
    name: String,
    cached: Vec<CachedLine>,
    enabled: bool,
}

impl SyntaxEngine {
    pub fn new(doc: &Document) -> Self {
        let syntaxes = two_face::syntax::extra_newlines();
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .expect("embedded highlighting theme");
        let name = Self::detect(&syntaxes, doc).name.to_owned();
        Self {
            syntaxes,
            theme,
            path: doc.path.clone(),
            name,
            cached: Vec::new(),
            enabled: true,
        }
    }

    fn detect<'a>(syntaxes: &'a SyntaxSet, doc: &Document) -> &'a SyntaxReference {
        if let Some(path) = &doc.path {
            // "Dockerfile", "Makefile", ".gitignore", etc. may have no extension.
            if let Some(filename) = path.file_name().and_then(|name| name.to_str()) {
                if let Some(syntax) = syntaxes.find_syntax_by_extension(filename) {
                    return syntax;
                }
            }
            if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
                if let Some(syntax) = syntaxes.find_syntax_by_extension(extension) {
                    return syntax;
                }
            }
            // This also handles filename patterns not exposed as a normal extension.
            if let Ok(Some(syntax)) = syntaxes.find_syntax_for_file(path) {
                return syntax;
            }
        }
        let first_line: String = doc.lines[0].iter().collect();
        if let Some(syntax) = syntaxes.find_syntax_by_first_line(&first_line) {
            return syntax;
        }
        syntaxes.find_syntax_plain_text()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn refresh(&mut self, doc: &mut Document) {
        let from = doc.take_syntax_invalidation();
        if self.path != doc.path {
            self.path = doc.path.clone();
            self.name = Self::detect(&self.syntaxes, doc).name.to_owned();
            self.cached.clear();
        } else if let Some(row) = from {
            // A shebang on the first line may change the language in unnamed files.
            if row == 0 {
                let name = &Self::detect(&self.syntaxes, doc).name;
                if name != &self.name {
                    self.name = name.to_owned();
                    self.cached.clear();
                }
            }
            self.cached.truncate(row);
        }
    }

    pub fn spans(&mut self, doc: &Document, row: usize) -> Option<&[Span]> {
        if !self.enabled || row >= doc.lines.len() {
            return None;
        }
        while self.cached.len() <= row {
            let index = self.cached.len();
            let syntax = self
                .syntaxes
                .find_syntax_by_name(&self.name)
                .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());

            let mut highlighter = if let Some(previous) = self.cached.last() {
                HighlightLines::from_state(
                    &self.theme,
                    previous.state.0.clone(),
                    previous.state.1.clone(),
                )
            } else {
                HighlightLines::new(syntax, &self.theme)
            };

            // The embedded grammars expect lines terminated with '\n'.
            let mut input: String = doc.lines[index].iter().collect();
            input.push('\n');

            let mut start = 0;
            let mut spans = Vec::new();
            let len = doc.lines[index].len();
            match highlighter.highlight_line(&input, &self.syntaxes) {
                Ok(ranges) => {
                    for (style, fragment) in ranges {
                        let end = (start + fragment.chars().count()).min(len);
                        if end > start {
                            spans.push(Span {
                                start,
                                end,
                                foreground: Color::Rgb {
                                    r: style.foreground.r,
                                    g: style.foreground.g,
                                    b: style.foreground.b,
                                },
                            });
                        }
                        start += fragment.chars().count();
                    }
                }
                Err(_) => {
                    // Unusual or malformed syntax should not crash the editor.
                    spans.clear();
                }
            }
            self.cached.push(CachedLine {
                state: highlighter.state(),
                spans,
            });
        }
        Some(&self.cached[row].spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn doc_named(name: &str, content: &str) -> Document {
        let mut doc = Document::from_text(content);
        doc.path = Some(PathBuf::from(name));
        doc
    }

    #[test]
    fn common_languages_and_markdown_are_detected() {
        let cases = [
            ("README.md", "# Hello", "Markdown"),
            ("main.rs", "fn main() {}", "Rust"),
            ("main.go", "package main", "Go"),
            ("app.py", "print('hi')", "Python"),
            ("app.ts", "const x = 1;", "TypeScript"),
            ("config.toml", "[section]\na = 1", "TOML"),
            ("main.tf", "resource \"x\" \"y\" {}", "Terraform"),
            ("style.css", "body { color: red; }", "CSS"),
            ("config.yaml", "foo: bar", "YAML"),
        ];
        for (filename, body, expected) in cases {
            let engine = SyntaxEngine::new(&doc_named(filename, body));
            assert_eq!(engine.name(), expected, "file: {filename}");
        }
    }

    #[test]
    fn defaults_to_highlighting_and_parses_markdown() {
        let doc = doc_named("README.md", "# Hello\n\n**bold**\n\n```rust\nfn main() {}\n```");
        let mut engine = SyntaxEngine::new(&doc);
        assert!(engine.enabled());
        assert!(engine.spans(&doc, 5).is_some());
        assert_eq!(engine.cached.len(), 6);
        assert!(!engine.spans(&doc, 0).unwrap().is_empty());
    }

    #[test]
    fn edited_line_invalidates_the_rest_of_the_parser_cache() {
        let mut doc = doc_named("main.rs", "/* comment\nstill */\nfn main() {}");
        let mut engine = SyntaxEngine::new(&doc);
        engine.spans(&doc, 2);
        assert_eq!(engine.cached.len(), 3);
        doc.row = 1;
        doc.col = 0;
        doc.insert('x');
        engine.refresh(&mut doc);
        assert_eq!(engine.cached.len(), 1);
        engine.spans(&doc, 2);
        assert_eq!(engine.cached.len(), 3);
    }

    #[test]
    fn syntax_changes_when_file_is_named() {
        let mut doc = Document::from_text("key = 1");
        let mut engine = SyntaxEngine::new(&doc);
        assert_eq!(engine.name(), "Plain Text");
        doc.path = Some(PathBuf::from("config.toml"));
        engine.refresh(&mut doc);
        assert_eq!(engine.name(), "TOML");
        engine.set_enabled(false);
        assert!(engine.spans(&doc, 0).is_none());
    }
}
