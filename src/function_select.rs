//! Language-aware function text objects backed by Tree-sitter.
//!
//! This parser runs only when a function-selection command is issued, never on
//! every cursor movement. Result coordinates are Unicode-char offsets, matching
//! Remacs' Document rather than Tree-sitter's UTF-8 byte offsets.
use std::path::Path;

use crate::document::Document;
use tree_sitter::{Language, Node, Parser};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextRange {
    pub start: Position,
    pub end: Position, // Exclusive
}

impl TextRange {
    pub fn columns(&self, row: usize, line_len: usize) -> Option<(usize, usize)> {
        if row < self.start.row || row > self.end.row || self.start >= self.end {
            return None;
        }
        let start = if row == self.start.row { self.start.col } else { 0 };
        let end = if row == self.end.row { self.end.col } else { line_len };
        if end > start {
            Some((start.min(line_len), end.min(line_len)))
        } else if row < self.end.row && start == line_len {
            // The selection includes the line break on this row.
            Some((line_len, line_len))
        } else {
            None
        }
    }
}

fn grammar(path: &Path) -> Option<Language> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "py" | "pyw" => tree_sitter_python::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" | "mts" | "cts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "c" | "h" => tree_sitter_c::LANGUAGE.into(),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => tree_sitter_cpp::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "sh" | "bash" | "zsh" => tree_sitter_bash::LANGUAGE.into(),
        "lua" => tree_sitter_lua::LANGUAGE.into(),
        "rb" | "rake" => tree_sitter_ruby::LANGUAGE.into(),
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        "cs" => tree_sitter_c_sharp::LANGUAGE.into(),
        "swift" => tree_sitter_swift::LANGUAGE.into(),
        _ => return None,
    })
}

fn function_kind(kind: &str) -> bool {
    matches!(kind,
        "function_item" | "function_definition" | "function_declaration" |
        "function_definition_statement" | "local_function" | "local_function_definition" |
        "method_declaration" | "method_definition" | "constructor_declaration" |
        "constructor_definition" | "generator_function_declaration" |
        "generator_function" | "function_expression" | "arrow_function" |
        "closure_expression" | "function_statement" | "method" |
        "singleton_method" | "function_declaration_statement"
    )
}

fn selection_node(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        if function_kind(node.kind()) {
            // Python decorators belong to the declaration.
            if node.kind() == "function_definition" {
                if let Some(parent) = node.parent() {
                    if parent.kind() == "decorated_definition" {
                        return Some(parent);
                    }
                }
            }
            // Include "const foo = " when selecting a JS/TS arrow-function
            // assigned to a single variable, not just the function expression.
            if node.kind() == "arrow_function" {
                if let Some(parent) = node.parent() {
                    if parent.kind() == "variable_declarator" {
                        if let Some(declaration) = parent.parent() {
                            if matches!(declaration.kind(), "lexical_declaration" | "variable_declaration")
                                && declaration.named_child_count() == 1
                            {
                                return Some(declaration);
                            }
                        }
                    }
                }
            }
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// A declaration can start with visibility/modifier tokens that the grammar
/// represents as siblings rather than children of the function node. When
/// the cursor is on those tokens (or indentation before the declaration),
/// the usual descendant/ancestor lookup won't reach the function.
fn header_function<'a>(
    root: Node<'a>,
    source: &str,
    row: usize,
    cursor_byte: usize,
) -> Option<(Node<'a>, usize)> {
    let line_start = source[..cursor_byte].rfind('\n').map_or(0, |i| i + 1);
    let mut stack = vec![root];
    let mut result: Option<(Node<'a>, usize)> = None;

    while let Some(node) = stack.pop() {
        let start = node.start_position();
        let end = node.end_position();
        if start.row > row || end.row < row {
            continue;
        }

        if function_kind(node.kind()) && start.row == row && node.start_byte() >= cursor_byte {
            let declaration_prefix = &source[line_start..node.start_byte()];
            let leading = declaration_prefix.trim_start();
            // Do not accidentally select a function from a preceding
            // unrelated statement on the same line.
            let is_declaration_prefix = leading.is_empty()
                || ["pub ", "pub(", "pub", "async ", "unsafe ", "extern ",
                    "default ", "export ", "const ", "let ", "var "]
                    .iter().any(|keyword| leading.starts_with(keyword));
            if is_declaration_prefix && declaration_prefix.len() < 128 {
                let leading_bytes = declaration_prefix.len() - leading.len();
                let prefix_start = line_start + leading_bytes;
                let normalized = selection_node(node).unwrap_or(node);
                if result.is_none_or(|(previous, _)| {
                    normalized.end_byte() - normalized.start_byte()
                        < previous.end_byte() - previous.start_byte()
                }) {
                    result = Some((normalized, prefix_start.min(normalized.start_byte())));
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.start_position().row <= row && child.end_position().row >= row {
                stack.push(child);
            }
        }
    }
    result
}

fn source_position(doc: &Document, row: usize, byte_col: usize) -> Position {
    let Some(line) = doc.lines.get(row) else {
        return Position { row: doc.lines.len() - 1, col: doc.lines.last().map_or(0, Vec::len) };
    };
    let mut count = 0usize;
    let mut bytes = 0usize;
    for ch in line {
        if bytes + ch.len_utf8() > byte_col { break; }
        bytes += ch.len_utf8();
        count += 1;
    }
    Position { row, col: count }
}

fn byte_offset(doc: &Document) -> usize {
    let mut offset = 0;
    for line in &doc.lines[..doc.row] {
        offset += line.iter().map(|ch| ch.len_utf8()).sum::<usize>() + 1;
    }
    offset + doc.lines[doc.row][..doc.col.min(doc.lines[doc.row].len())]
        .iter().map(|ch| ch.len_utf8()).sum::<usize>()
}

/// Find the *innermost* function surrounding the cursor.
pub fn around_function(doc: &Document) -> Result<Option<TextRange>, String> {
    let Some(path) = doc.path.as_ref() else {
        return Err("Function selection needs a filename to detect the language".into());
    };
    let Some(language) = grammar(path) else {
        return Err("No Tree-sitter function grammar for this file type yet".into());
    };
    // Parse on demand. Don't temporarily block editing on enormous files.
    let source = doc.lines.iter().map(|row| row.iter().collect::<String>())
        .collect::<Vec<_>>().join("\n");
    if source.len() > 12 * 1024 * 1024 {
        return Err("Function selection is limited to files up to 12 MiB for now".into());
    }
    if source.is_empty() { return Ok(None); }
    let mut parser = Parser::new();
    parser.set_language(&language).map_err(|e| format!("Tree-sitter language error: {e}"))?;
    let tree = parser.parse(&source, None)
        .ok_or_else(|| "Tree-sitter could not parse the document".to_string())?;
    let byte = byte_offset(doc).min(source.len().saturating_sub(1));
    let root = tree.root_node();
    let node = root.descendant_for_byte_range(byte, byte + 1);
    let regular = node.and_then(selection_node);
    let fallback = if regular.is_none() {
        header_function(root, &source, doc.row, byte)
    } else {
        None
    };
    let Some(function) = regular.or_else(|| fallback.map(|(node, _)| node)) else {
        return Ok(None);
    };
    let start = function.start_position();
    let end = function.end_position();
    let mut from = source_position(doc, start.row, start.column);
    if let Some((_, start_byte)) = fallback {
        // If the grammar starts at "fn" but the declaration begins with
        // "pub", "async" etc., include those modifiers in the selection.
        if start_byte < function.start_byte() {
            let line_start = source[..start_byte].rfind('
').map_or(0, |i| i + 1);
            from = source_position(doc, start.row, start_byte - line_start);
        }
    }
    let selection = TextRange {
        start: from,
        end: source_position(doc, end.row, end.column),
    };
    if selection.start >= selection.end { Ok(None) } else { Ok(Some(selection)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn named(path: &str, text: &str, needle: &str) -> Document {
        let mut doc = Document::from_text(text);
        doc.path = Some(PathBuf::from(path));
        let (row, col) = doc.lines.iter().enumerate().find_map(|(row, line)| {
            let string: String = line.iter().collect();
            string.find(needle).map(|offset| (row, string[..offset].chars().count()))
        }).expect("needle");
        doc.row = row;
        doc.col = col;
        doc
    }


    #[test]
    fn rust_function_header_is_selectable_at_every_position() {
        // Regression: vaf used to work only after entering the function body.
        let text = "pub fn from_text(text: &str) -> Self {\n    let newline = if text.contains(\"\\r\\n\") { \"\\r\\n\" } else { \"\\n\" };\n    let lines = text.split('\\n').collect();\n    Self { lines }\n}\n";
        for needle in ["pub", "fn", "from_text", "text:", "&str", "Self", "{"] {
            let doc = named("src/document.rs", text, needle);
            let selected = around_function(&doc).unwrap()
                .unwrap_or_else(|| panic!("no function found at {needle}"));
            assert_eq!(selected.start, Position { row: 0, col: 0 }, "{needle}");
            assert_eq!(selected.end, Position { row: 4, col: 1 }, "{needle}");
        }
    }

    #[test]
    fn indented_and_multiline_declarations_are_selectable() {
        let text = "impl Example {\n    pub fn build(\n        input: &str,\n    ) -> Self {\n        Self {}\n    }\n}\n";
        for needle in ["pub", "fn", "build", "input:", "Self {"] {
            let doc = named("src/lib.rs", text, needle);
            let selected = around_function(&doc).unwrap().unwrap();
            assert_eq!(selected.start, Position { row: 1, col: 4 }, "{needle}");
            assert_eq!(selected.end, Position { row: 5, col: 5 }, "{needle}");
        }
    }

    #[test]
    fn selecting_a_function_from_preceding_unrelated_line_is_not_allowed() {
        let doc = named("src/lib.rs", "const ANSWER: i32 = 42;\n\npub fn answer() -> i32 { ANSWER }\n", "ANSWER:");
        assert!(around_function(&doc).unwrap().is_none());
    }

    #[test]
    fn rust_nested_function_and_nested_braces() {
        let text = "fn outer() {\n  let a = { 1 };\n  fn inner() {\n    if true { println!(\"hi\"); }\n  }\n}\n";
        let doc = named("main.rs", text, "println");
        let selection = around_function(&doc).unwrap().unwrap();
        assert_eq!(selection.start, Position { row: 2, col: 2 });
        assert_eq!(selection.end, Position { row: 4, col: 3 });
        let doc = named("main.rs", text, "let a");
        let selection = around_function(&doc).unwrap().unwrap();
        assert_eq!(selection.start.row, 0);
        assert_eq!(selection.end.row, 5);
    }

    #[test]
    fn rust_function_with_public_modifier_and_unicode() {
        let doc = named("main.rs", "pub fn from_text(text: &str) -> Self {\n    let café = { 42 };\n    Self { café }\n}\n", "café");
        let range = around_function(&doc).unwrap().unwrap();
        assert_eq!(range.start, Position { row: 0, col: 0 });
        assert_eq!(range.end, Position { row: 3, col: 1 });
    }

    #[test]
    fn python_and_go_and_typescript_and_cpp() {
        for (path, source, needle, start) in [
            ("m.py", "@decorator\ndef hello():\n    return 42\n", "return", 0),
            ("m.go", "package main\nfunc say() {\n  println(\"ok\")\n}\n", "println", 1),
            ("m.ts", "const say = (name: string) => {\n return name;\n};\n", "return", 0),
            ("m.cpp", "int say(int x) {\n return x + 1;\n}\n", "return", 0),
        ] {
            let doc = named(path, source, needle);
            let range = around_function(&doc).unwrap().unwrap();
            assert_eq!(range.start.row, start, "{path}");
            assert!(range.end.row >= doc.row, "{path}");
        }
    }

    #[test]
    fn plain_text_does_not_guess_a_function() {
        let doc = named("README.md", "### Hello\nA { brace }", "brace");
        assert!(around_function(&doc).is_err());
    }

    #[test]
    fn no_function_outside_definition() {
        let doc = named("main.rs", "const A: usize = 2;\nfn f() { println!(\"x\"); }\n", "const");
        assert!(around_function(&doc).unwrap().is_none());
    }
}
