//! Completion results and atomic, Unicode-aware application of their edits.
//! Servers describe edits against the request snapshot, never against text
//! already changed by another edit in the same completion.
use std::collections::HashMap;

use serde_json::Value;

use crate::{document::Document, function_select::Position as CharPosition, lsp::Encoding};

const MAX_SUGGESTIONS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    row: usize,
    character: usize,
}

#[derive(Clone, Copy, Debug)]
struct Range {
    start: Position,
    end: Position,
}

#[derive(Clone, Debug)]
struct Edit {
    range: Range,
    text: String,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub detail: Option<String>,
    pub filter_text: String,
    pub sort_text: String,
    text: String,
    range: Option<Range>,
    insert_range: Option<Range>,
    additional: Vec<Edit>,
    snippet: bool,
    explicit_filter: bool,
}

#[derive(Clone, Debug, Default)]
pub struct List {
    pub items: Vec<Item>,
    pub incomplete: bool,
}

fn position(value: &Value) -> Option<Position> {
    Some(Position {
        row: value["line"].as_u64()?.try_into().ok()?,
        character: value["character"].as_u64()?.try_into().ok()?,
    })
}

fn range(value: &Value) -> Option<Range> {
    let range = Range {
        start: position(&value["start"])?,
        end: position(&value["end"])?,
    };
    (range.start <= range.end).then_some(range)
}

fn edit_ranges(value: &Value) -> Option<(Range, Option<Range>)> {
    if value.get("start").is_some() {
        return Some((range(value)?, None));
    }
    let insert = range(&value["insert"])?;
    let replace = range(&value["replace"])?;
    (insert.start == replace.start && insert.end <= replace.end).then_some((replace, Some(insert)))
}

fn item(value: &Value, defaults: &Value) -> Option<Item> {
    let label = value["label"].as_str()?.to_owned();
    if label.is_empty() {
        return None;
    }
    let (text, main_range, insert_range) = if let Some(edit) = value.get("textEdit") {
        let ranges = if let Some(value) = edit.get("range") {
            (range(value)?, None)
        } else {
            edit_ranges(edit)?
        };
        (
            edit["newText"].as_str()?.to_owned(),
            Some(ranges.0),
            ranges.1,
        )
    } else if let Some(value_range) = defaults.get("editRange") {
        let (range, insert_range) = edit_ranges(value_range)?;
        (
            value["textEditText"].as_str().unwrap_or(&label).to_owned(),
            Some(range),
            insert_range,
        )
    } else {
        (
            value["insertText"].as_str().unwrap_or(&label).to_owned(),
            None,
            None,
        )
    };
    let additional = if let Some(edits) = value.get("additionalTextEdits") {
        edits
            .as_array()?
            .iter()
            .map(|edit| {
                Some(Edit {
                    range: range(&edit["range"])?,
                    text: edit["newText"].as_str()?.to_owned(),
                })
            })
            .collect::<Option<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Some(Item {
        filter_text: value["filterText"].as_str().unwrap_or(&label).to_owned(),
        sort_text: value["sortText"].as_str().unwrap_or(&label).to_owned(),
        detail: value["detail"]
            .as_str()
            .or_else(|| value["labelDetails"]["description"].as_str())
            .map(str::to_owned),
        label,
        text,
        range: main_range,
        insert_range,
        additional,
        snippet: value["insertTextFormat"]
            .as_u64()
            .or_else(|| defaults["insertTextFormat"].as_u64())
            == Some(2),
        explicit_filter: value["filterText"].is_string(),
    })
}

pub fn parse(result: &Value) -> List {
    let values = result.as_array().or_else(|| result["items"].as_array());
    List {
        items: values
            .into_iter()
            .flatten()
            .filter_map(|value| item(value, &result["itemDefaults"]))
            .collect(),
        incomplete: result["isIncomplete"].as_bool().unwrap_or(false),
    }
}

/// Honor server filtering for explicit edits. Plain insertions use the
/// identifier prefix; filterText overrides that behavior for either form.
pub fn suggestions(list: List, doc: &Document, encoding: Encoding) -> Vec<Item> {
    let Some(line) = doc.lines.get(doc.row).filter(|line| doc.col <= line.len()) else {
        return Vec::new();
    };
    let prefix = identifier_prefix(line, doc.col).to_lowercase();
    let mut items: Vec<_> = list
        .items
        .into_iter()
        .filter(|item| {
            let server_prefix = if let Some(range) = item.range {
                if !item.explicit_filter {
                    return true;
                }
                // A server edit may replace a qualified expression, not just
                // the final identifier guessed by the editor.
                if range.start.row != doc.row {
                    return false;
                }
                let Ok(start) = char_column(line, range.start.character, encoding) else {
                    return false;
                };
                if start > doc.col {
                    return false;
                }
                Some(
                    line[start..doc.col]
                        .iter()
                        .collect::<String>()
                        .to_lowercase(),
                )
            } else {
                None
            };
            item.filter_text
                .to_lowercase()
                .starts_with(server_prefix.as_deref().unwrap_or(&prefix))
        })
        .collect();
    items.sort_by(|left, right| {
        left.sort_text
            .cmp(&right.sort_text)
            .then_with(|| left.label.cmp(&right.label))
    });
    items.truncate(MAX_SUGGESTIONS);
    items
}

fn identifier_start(line: &[char], col: usize) -> usize {
    line[..col]
        .iter()
        .rposition(|ch| !ch.is_alphanumeric() && *ch != '_')
        .map_or(0, |column| column + 1)
}

fn identifier_prefix(line: &[char], col: usize) -> String {
    line[identifier_start(line, col)..col].iter().collect()
}

fn char_column(line: &[char], character: usize, encoding: Encoding) -> Result<usize, String> {
    let mut offset = 0;
    for (column, ch) in line.iter().enumerate() {
        if offset == character {
            return Ok(column);
        }
        offset += match encoding {
            Encoding::Utf8 => ch.len_utf8(),
            Encoding::Utf16 => ch.len_utf16(),
            Encoding::Utf32 => 1,
        };
        if offset > character {
            return Err("Completion edit splits a Unicode character".into());
        }
    }
    if offset == character {
        Ok(line.len())
    } else {
        Err("Completion edit is outside the document".into())
    }
}

fn edit_positions(
    doc: &Document,
    range: Range,
    encoding: Encoding,
) -> Result<(CharPosition, CharPosition), String> {
    let start_line = doc
        .lines
        .get(range.start.row)
        .ok_or("Completion edit is outside the document")?;
    let end_line = doc
        .lines
        .get(range.end.row)
        .ok_or("Completion edit is outside the document")?;
    let start = CharPosition {
        row: range.start.row,
        col: char_column(start_line, range.start.character, encoding)?,
    };
    let end = CharPosition {
        row: range.end.row,
        col: char_column(end_line, range.end.character, encoding)?,
    };
    if start > end {
        return Err("Completion edit has an invalid range".into());
    }
    Ok((start, end))
}

fn normalized(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

struct PlannedEdit {
    start: CharPosition,
    end: CharPosition,
    lines: Vec<Vec<char>>,
    caret: Option<CharPosition>,
}

fn relative_position(text: &str, offset: usize) -> CharPosition {
    let mut position = CharPosition { row: 0, col: 0 };
    for ch in text.chars().take(offset) {
        if ch == '\n' {
            position.row += 1;
            position.col = 0;
        } else {
            position.col += 1;
        }
    }
    position
}

fn inserted_position(start: CharPosition, relative: CharPosition) -> CharPosition {
    CharPosition {
        row: start.row + relative.row,
        col: if relative.row == 0 {
            start.col + relative.col
        } else {
            relative.col
        },
    }
}

/// Positions after an earlier edit keep their suffix column when the edit
/// joins their line, or only shift rows when they are on a later line.
fn shifted_position(
    position: CharPosition,
    previous_end: CharPosition,
    replacement_end: CharPosition,
) -> CharPosition {
    if position.row == previous_end.row {
        CharPosition {
            row: replacement_end.row,
            col: replacement_end.col + position.col - previous_end.col,
        }
    } else {
        CharPosition {
            row: position.row - previous_end.row + replacement_end.row,
            col: position.col,
        }
    }
}

/// Validate every edit before changing the document. The caller captures one
/// undo snapshot for the entire completion, including any imports it adds.
pub fn apply(doc: &mut Document, item: &Item, encoding: Encoding) -> Result<(), String> {
    let line = doc
        .lines
        .get(doc.row)
        .filter(|line| doc.col <= line.len())
        .ok_or("Completion cursor is outside the document")?;
    let cursor = CharPosition {
        row: doc.row,
        col: doc.col,
    };
    let (start, end) = if let Some(range) = item.range {
        let positions = edit_positions(doc, range, encoding)?;
        if range.start.row != range.end.row || !(positions.0..=positions.1).contains(&cursor) {
            return Err("Completion replacement must contain the cursor on one line".into());
        }
        if let Some(insert) = item.insert_range {
            let (insert_start, insert_end) = edit_positions(doc, insert, encoding)?;
            if insert.start.row != insert.end.row
                || insert_start != positions.0
                || insert_end > positions.1
                || !(insert_start..=insert_end).contains(&cursor)
            {
                return Err("Completion has an invalid insert/replace range".into());
            }
        }
        positions
    } else {
        (
            CharPosition {
                row: doc.row,
                col: identifier_start(line, doc.col),
            },
            cursor,
        )
    };
    let (text, stop) = if item.snippet {
        flatten_snippet(&item.text)?
    } else {
        (item.text.clone(), None)
    };
    let text = normalized(&text);
    let mut edits = vec![PlannedEdit {
        start,
        end,
        caret: Some(relative_position(&text, stop.unwrap_or(usize::MAX))),
        lines: text
            .split('\n')
            .map(|line| line.chars().collect())
            .collect(),
    }];
    for edit in &item.additional {
        let (start, end) = edit_positions(doc, edit.range, encoding)?;
        edits.push(PlannedEdit {
            start,
            end,
            lines: normalized(&edit.text)
                .split('\n')
                .map(|line| line.chars().collect())
                .collect(),
            caret: None,
        });
    }
    edits.sort_by_key(|edit| (edit.start, edit.end));
    if edits
        .windows(2)
        .any(|pair| pair[1].start < pair[0].end || pair[1].start == pair[0].start)
    {
        return Err("Completion edits overlap".into());
    }
    let first_changed = edits[0].start.row;
    let mut caret = None;
    // Validate against the original snapshot, then splice from the end so
    // remaining original coordinates stay valid. Untouched lines retain
    // their allocations instead of being copied for a one-word completion.
    for edit in edits.into_iter().rev() {
        let replacement_end = inserted_position(
            edit.start,
            CharPosition {
                row: edit.lines.len() - 1,
                col: edit.lines.last().unwrap().len(),
            },
        );
        caret = if let Some(relative) = edit.caret {
            Some(inserted_position(edit.start, relative))
        } else {
            caret.map(|position| shifted_position(position, edit.end, replacement_end))
        };
        let tail = doc.lines[edit.end.row].split_off(edit.end.col);
        doc.lines[edit.start.row].truncate(edit.start.col);
        let mut inserted = edit.lines.into_iter();
        doc.lines[edit.start.row].extend(inserted.next().unwrap());
        let mut following: Vec<_> = inserted.collect();
        if let Some(last) = following.last_mut() {
            last.extend(tail);
        } else {
            doc.lines[edit.start.row].extend(tail);
        }
        doc.lines
            .splice(edit.start.row + 1..edit.end.row + 1, following);
    }
    let caret = caret.expect("a completion always contains its primary edit");
    doc.row = caret.row;
    doc.col = caret.col;
    doc.dirty = true;
    doc.invalidate_syntax_from(first_changed);
    Ok(())
}

// Snippets are not advertised. This conservative fallback handles common
// placeholders from servers that send them regardless, without inserting
// raw tab-stop markers. Transforms/variables are rejected before mutation.
fn flatten_snippet(text: &str) -> Result<(String, Option<usize>), String> {
    let chars: Vec<char> = normalized(text).chars().collect();
    let mut index = 0;
    let mut values = HashMap::new();
    let (output, stops) = snippet_fragment(&chars, &mut index, false, 0, &mut values)?;
    let stop = stops
        .iter()
        .min_by_key(|(number, _)| if *number == 0 { u32::MAX } else { *number })
        .map(|(_, offset)| *offset);
    Ok((output, stop))
}

type SnippetFragment = (String, Vec<(u32, usize)>);

fn snippet_fragment(
    chars: &[char],
    index: &mut usize,
    braced: bool,
    depth: usize,
    values: &mut HashMap<u32, String>,
) -> Result<SnippetFragment, String> {
    if depth > 16 {
        return Err("Completion snippet is too deeply nested".into());
    }
    let mut output = String::new();
    let mut stops = Vec::new();
    let mut offset = 0;
    while let Some(&ch) = chars.get(*index) {
        *index += 1;
        if ch == '}' && braced {
            return Ok((output, stops));
        }
        if ch == '\\'
            && chars
                .get(*index)
                .is_some_and(|ch| matches!(ch, '$' | '}' | '\\'))
        {
            output.push(chars[*index]);
            *index += 1;
            offset += 1;
            continue;
        }
        if ch != '$' {
            output.push(ch);
            offset += 1;
            continue;
        }
        let has_brace = chars.get(*index) == Some(&'{');
        if has_brace {
            *index += 1;
        }
        let begin = *index;
        while chars.get(*index).is_some_and(char::is_ascii_digit) {
            *index += 1;
        }
        if begin == *index {
            if has_brace
                || chars
                    .get(*index)
                    .is_some_and(|ch| ch.is_alphabetic() || *ch == '_')
            {
                return Err("Completion snippet variables are not supported".into());
            }
            output.push('$');
            offset += 1;
            continue;
        }
        let number = chars[begin..*index]
            .iter()
            .collect::<String>()
            .parse::<u32>()
            .map_err(|_| "Invalid completion snippet tab stop")?;
        let mut nested_stops = Vec::new();
        let value = if has_brace {
            match chars.get(*index) {
                Some('}') => {
                    *index += 1;
                    values.get(&number).cloned().unwrap_or_default()
                }
                Some(':') => {
                    *index += 1;
                    let (value, nested) = snippet_fragment(chars, index, true, depth + 1, values)?;
                    nested_stops = nested;
                    values.insert(number, value.clone());
                    value
                }
                Some('|') => {
                    *index += 1;
                    let mut first = String::new();
                    let mut first_choice = true;
                    let mut closed = false;
                    while let Some(&choice) = chars.get(*index) {
                        *index += 1;
                        if choice == '|' && chars.get(*index) == Some(&'}') {
                            *index += 1;
                            closed = true;
                            break;
                        }
                        if choice == '\\'
                            && chars
                                .get(*index)
                                .is_some_and(|ch| matches!(ch, ',' | '|' | '\\'))
                        {
                            if first_choice {
                                first.push(chars[*index]);
                            }
                            *index += 1;
                        } else if choice == ',' {
                            first_choice = false;
                        } else if first_choice {
                            first.push(choice);
                        }
                    }
                    if !closed {
                        return Err("Unclosed completion snippet choice".into());
                    }
                    values.insert(number, first.clone());
                    first
                }
                _ => return Err("Completion snippet transforms are not supported".into()),
            }
        } else {
            values.get(&number).cloned().unwrap_or_default()
        };
        stops.push((number, offset));
        stops.extend(
            nested_stops
                .into_iter()
                .map(|(number, stop)| (number, offset + stop)),
        );
        offset += value.chars().count();
        output.push_str(&value);
    }
    if braced {
        return Err("Unclosed completion snippet placeholder".into());
    }
    Ok((output, stops))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(value: Value) -> Item {
        parse(&json!([value])).items.remove(0)
    }

    #[test]
    fn parses_plain_arrays_lists_and_defaults_with_item_overrides() {
        assert!(parse(&Value::Null).items.is_empty());
        let list = parse(&json!({
            "isIncomplete": true,
            "itemDefaults": {
                "editRange": {"start":{"line":0,"character":0},"end":{"line":0,"character":2}},
                "insertTextFormat":2
            },
            "items":[
                {"label":"function","textEditText":"function($0)"},
                {"label":"format","textEditText":"format!","insertTextFormat":1}
            ]
        }));
        assert!(list.incomplete);
        assert_eq!(list.items.len(), 2);
        let mut doc = Document::from_text("fu tail");
        doc.col = 2;
        apply(&mut doc, &list.items[0], Encoding::Utf16).unwrap();
        assert_eq!(doc.lsp_text(), "function() tail");
        assert_eq!(doc.col, 9);
        let mut doc = Document::from_text("fo");
        doc.col = 2;
        apply(&mut doc, &list.items[1], Encoding::Utf16).unwrap();
        assert_eq!(doc.lsp_text(), "format!");
    }

    #[test]
    fn plain_insertions_replace_only_the_identifier_prefix() {
        let item = one(json!({"label":"println!","insertText":"println!()"}));
        let mut doc = Document::from_text("let result = prin tail");
        doc.col = 17;
        apply(&mut doc, &item, Encoding::Utf16).unwrap();
        assert_eq!(doc.lsp_text(), "let result = println!() tail");
        assert_eq!(doc.col, 23);
        assert!(doc.dirty);
        assert_eq!(doc.revision, 1);
    }

    #[test]
    fn explicit_edits_and_imports_use_original_unicode_coordinates() {
        for (encoding, start, end) in [
            (Encoding::Utf8, 5, 8),
            (Encoding::Utf16, 3, 6),
            (Encoding::Utf32, 2, 5),
        ] {
            let item = one(json!({
                "label":"thing",
                "textEdit":{"range":{"start":{"line":1,"character":start},"end":{"line":1,"character":end}},"newText":"thing"},
                "additionalTextEdits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"use crate::thing;\r\n"}]
            }));
            let mut doc = Document::from_text("// header\n😀 thi!");
            doc.row = 1;
            doc.col = 5;
            apply(&mut doc, &item, encoding).unwrap();
            assert_eq!(doc.lsp_text(), "use crate::thing;\n// header\n😀 thing!");
            assert_eq!((doc.row, doc.col), (2, 7));
            assert_eq!(doc.revision, 1);
        }
    }

    #[test]
    fn insert_replace_edit_replaces_the_existing_word_suffix() {
        let item = one(json!({"label":"alphabet","textEdit":{
            "insert":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},
            "replace":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},
            "newText":"alphabet"
        }}));
        let mut doc = Document::from_text("alpha()");
        doc.col = 2;
        apply(&mut doc, &item, Encoding::Utf16).unwrap();
        assert_eq!(doc.lsp_text(), "alphabet()");
        assert_eq!(doc.col, 8);
    }

    #[test]
    fn invalid_ranges_and_overlapping_edits_leave_document_untouched() {
        for additional in [
            json!({"start":{"line":0,"character":1},"end":{"line":0,"character":2}}),
            json!({"start":{"line":9,"character":0},"end":{"line":9,"character":0}}),
        ] {
            let item = one(
                json!({"label":"abc", "additionalTextEdits":[{"range":additional,"newText":"bad"}]}),
            );
            let mut doc = Document::from_text("ab");
            doc.col = 2;
            assert!(apply(&mut doc, &item, Encoding::Utf16).is_err());
            assert_eq!(doc.lsp_text(), "ab");
            assert_eq!((doc.row, doc.col), (0, 2));
            assert_eq!(doc.revision, 0);
            assert!(!doc.dirty);
        }
        let item = one(
            json!({"label":"x", "textEdit":{"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}},"newText":"x"}}),
        );
        let mut doc = Document::from_text("😀ab");
        doc.col = 1;
        assert!(apply(&mut doc, &item, Encoding::Utf16).is_err());
        assert_eq!(doc.lsp_text(), "😀ab");
    }

    #[test]
    fn snippets_expand_defaults_mirrors_choices_and_unicode_caret() {
        let item = one(json!({"label":"call", "insertTextFormat":2,
            "insertText":"call(${1:世界}, $1, ${2|yes,no|})$0"}));
        let mut doc = Document::from_text("ca");
        doc.col = 2;
        apply(&mut doc, &item, Encoding::Utf16).unwrap();
        assert_eq!(doc.lsp_text(), "call(世界, 世界, yes)");
        assert_eq!(doc.col, 5);
        let malformed =
            one(json!({"label":"x", "insertTextFormat":2,"insertText":"${1/foo/bar/}"}));
        let before = doc.lsp_text();
        assert!(apply(&mut doc, &malformed, Encoding::Utf16).is_err());
        assert_eq!(doc.lsp_text(), before);
    }

    #[test]
    fn filters_unicode_prefix_and_honors_server_sort_text() {
        let list = parse(&json!([
            {"label":"Éclair","sortText":"2"},
            {"label":"École","sortText":"1"},
            {"label":"unrelated"},
            {"label":"alias","filterText":"écho","sortText":"3"}
        ]));
        let mut doc = Document::from_text("Éc");
        doc.col = 2;
        let labels: Vec<_> = suggestions(list, &doc, Encoding::Utf16)
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert_eq!(labels, ["École", "Éclair", "alias"]);
        let list = parse(&json!([{"label":"server controlled", "textEdit":{
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}, "newText":"x"
        }}]));
        let mut doc = Document::from_text("unknown");
        doc.col = 7;
        assert_eq!(suggestions(list, &doc, Encoding::Utf16).len(), 1);
    }

    #[test]
    fn qualified_filter_text_uses_the_server_replacement_prefix() {
        let list = parse(&json!([{
            "label":"Vec",
            "filterText":"std::vec::Vec",
            "textEdit":{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":7}},
                "newText":"std::vec::Vec"
            }
        }]));
        let mut doc = Document::from_text("std::ve");
        doc.col = 7;
        let items = suggestions(list, &doc, Encoding::Utf16);
        assert_eq!(items.len(), 1);
        apply(&mut doc, &items[0], Encoding::Utf16).unwrap();
        assert_eq!(doc.lsp_text(), "std::vec::Vec");
    }

    #[test]
    fn additional_edits_before_and_after_the_main_edit_preserve_its_caret() {
        let item = one(json!({"label":"thing","textEdit":{
            "range":{"start":{"line":0,"character":3},"end":{"line":0,"character":6}},
            "newText":"thing"
        },"additionalTextEdits":[
            {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"newText":"/*prefix*/"},
            {"range":{"start":{"line":0,"character":7},"end":{"line":0,"character":11}},"newText":"suffix"}
        ]}));
        let mut doc = Document::from_text("// thi tail");
        doc.col = 6;
        apply(&mut doc, &item, Encoding::Utf16).unwrap();
        assert_eq!(doc.lsp_text(), "/*prefix*/ thing suffix");
        assert_eq!((doc.row, doc.col), (0, 16));
        assert_eq!(doc.revision, 1);
    }

    #[test]
    fn multiline_additional_replacement_shifts_caret_rows_without_copying_text() {
        let item = one(json!({"label":"thing","textEdit":{
            "range":{"start":{"line":2,"character":12},"end":{"line":2,"character":15}},
            "newText":"thing"
        },"additionalTextEdits":[
            {"range":{"start":{"line":0,"character":0},"end":{"line":1,"character":5}},"newText":"use thing;"}
        ]}));
        let mut doc = Document::from_text("remove\nthese\nfn body() { thi }\nkeep");
        doc.row = 2;
        doc.col = 15;
        apply(&mut doc, &item, Encoding::Utf16).unwrap();
        assert_eq!(doc.lsp_text(), "use thing;\nfn body() { thing }\nkeep");
        assert_eq!((doc.row, doc.col), (1, 17));
        assert_eq!(doc.revision, 1);
    }
}
