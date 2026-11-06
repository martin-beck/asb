//! Structural request comparison and bounded, Unicode-aware text alignment.

use serde_json::Value;
use std::collections::BTreeSet;

const MAX_FIELDS: usize = 50_000;
const MAX_LINES: usize = 20_000;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Status {
    Equal,
    Changed,
    Unavailable,
}

impl Status {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::Changed => "!",
            Self::Unavailable => "?",
        }
    }
}

pub(crate) struct Field {
    pub(crate) path: String,
    pub(crate) status: Status,
    pub(crate) expected: Option<Value>,
    pub(crate) actual: Option<Value>,
}

impl Field {
    pub(crate) fn kind(&self, expected: bool) -> &'static str {
        match if expected {
            &self.expected
        } else {
            &self.actual
        } {
            None => "missing",
            Some(Value::String(_)) => "string",
            Some(Value::Number(_)) => "number",
            Some(Value::Bool(_)) => "boolean",
            Some(Value::Null) => "null",
            Some(Value::Array(_)) => "array",
            Some(Value::Object(_)) => "object",
        }
    }

    pub(crate) fn text(&self, expected: bool) -> String {
        match if expected {
            &self.expected
        } else {
            &self.actual
        } {
            None => "<missing field>".into(),
            Some(Value::String(text)) => text.clone(),
            Some(value) => serde_json::to_string_pretty(value).expect("JSON value is serializable"),
        }
    }
}

fn unavailable(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|text| {
        text.eq_ignore_ascii_case("[redacted]") || text.starts_with("[omitted:")
    })
}

pub(crate) fn compare(expected: &Value, actual: &Value) -> Result<Vec<Field>, String> {
    let mut fields = Vec::new();
    visit(Some(expected), Some(actual), "$".into(), &mut fields)?;
    Ok(fields)
}

fn visit(
    expected: Option<&Value>,
    actual: Option<&Value>,
    path: String,
    fields: &mut Vec<Field>,
) -> Result<(), String> {
    if fields.len() >= MAX_FIELDS {
        return Err("comparison exceeds 50,000 fields; open a smaller diagnostic artifact".into());
    }
    if let (Some(Value::Object(left)), Some(Value::Object(right))) = (expected, actual) {
        let keys: BTreeSet<_> = left.keys().chain(right.keys()).collect();
        if !keys.is_empty() {
            for key in keys {
                let suffix = if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !key.is_empty()
                {
                    format!(".{key}")
                } else {
                    format!(
                        "[{}]",
                        serde_json::to_string(key).expect("string serialization")
                    )
                };
                visit(
                    left.get(key),
                    right.get(key),
                    format!("{path}{suffix}"),
                    fields,
                )?;
            }
            return Ok(());
        }
    }
    if let (Some(Value::Array(left)), Some(Value::Array(right))) = (expected, actual)
        && (!left.is_empty() || !right.is_empty())
    {
        for index in 0..left.len().max(right.len()) {
            visit(
                left.get(index),
                right.get(index),
                format!("{path}[{index}]"),
                fields,
            )?;
        }
        return Ok(());
    }
    let status = if unavailable(expected) || unavailable(actual) {
        Status::Unavailable
    } else if expected == actual {
        Status::Equal
    } else {
        Status::Changed
    };
    fields.push(Field {
        path,
        status,
        expected: expected.cloned(),
        actual: actual.cloned(),
    });
    Ok(())
}

/// Render file content as visible text without emitting terminal control bytes.
pub(crate) fn visible(text: &str) -> String {
    let mut result = String::new();
    for character in text.chars() {
        match character {
            '\t' => result.push_str("    "),
            c if c.is_control() => result.extend(c.escape_default()),
            c => result.push(c),
        }
    }
    result
}

#[derive(Clone)]
pub(crate) struct Segment {
    pub(crate) text: String,
    pub(crate) changed: bool,
}

pub(crate) struct Row {
    pub(crate) expected_number: Option<usize>,
    pub(crate) actual_number: Option<usize>,
    pub(crate) expected: Vec<Segment>,
    pub(crate) actual: Vec<Segment>,
    pub(crate) changed: bool,
}

pub(crate) struct TextDiff {
    pub(crate) rows: Vec<Row>,
    pub(crate) notes: Vec<String>,
}

type Alignment = Vec<(Option<usize>, Option<usize>)>;

fn align<T: Eq>(left: &[T], right: &[T], max_cells: usize) -> Option<Alignment> {
    let width = right.len().checked_add(1)?;
    let cells = left.len().checked_add(1)?.checked_mul(width)?;
    if cells > max_cells {
        return None;
    }
    let mut lengths = vec![0u32; cells];
    for i in (0..left.len()).rev() {
        for j in (0..right.len()).rev() {
            lengths[i * width + j] = if left[i] == right[j] {
                1 + lengths[(i + 1) * width + j + 1]
            } else {
                lengths[(i + 1) * width + j].max(lengths[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut result = Vec::new();
    while i < left.len() || j < right.len() {
        if i < left.len() && j < right.len() && left[i] == right[j] {
            result.push((Some(i), Some(j)));
            i += 1;
            j += 1;
        } else if i < left.len()
            && (j == right.len() || lengths[(i + 1) * width + j] >= lengths[i * width + j + 1])
        {
            result.push((Some(i), None));
            i += 1;
        } else {
            result.push((None, Some(j)));
            j += 1;
        }
    }
    Some(result)
}

fn segment(segments: &mut Vec<Segment>, character: char, changed: bool) {
    let text = visible(&character.to_string());
    if let Some(last) = segments.last_mut().filter(|last| last.changed == changed) {
        last.text.push_str(&text);
    } else {
        segments.push(Segment { text, changed });
    }
}

fn inline(left: &str, right: &str) -> (Vec<Segment>, Vec<Segment>, bool) {
    if left == right {
        let segment = Segment {
            text: visible(left),
            changed: false,
        };
        return (vec![segment.clone()], vec![segment], false);
    }
    let a: Vec<_> = left.chars().collect();
    let b: Vec<_> = right.chars().collect();
    let mut expected = Vec::new();
    let mut actual = Vec::new();
    if let Some(alignment) = align(&a, &b, 65_536) {
        for (i, j) in alignment {
            let changed = i.zip(j).is_none();
            if let Some(i) = i {
                segment(&mut expected, a[i], changed);
            }
            if let Some(j) = j {
                segment(&mut actual, b[j], changed);
            }
        }
        (expected, actual, false)
    } else {
        let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
        let suffix = a[prefix..]
            .iter()
            .rev()
            .zip(b[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        for (index, c) in a.iter().enumerate() {
            segment(
                &mut expected,
                *c,
                index >= prefix && index < a.len() - suffix,
            );
        }
        for (index, c) in b.iter().enumerate() {
            segment(&mut actual, *c, index >= prefix && index < b.len() - suffix);
        }
        (expected, actual, true)
    }
}

impl TextDiff {
    pub(crate) fn new(left: &str, right: &str) -> Self {
        let mut clipped = false;
        let mut preview = |text: &str| {
            text.split('\n')
                .take(MAX_LINES + 1)
                .map(|line| {
                    let mut chars = line.chars();
                    let prefix: String = chars.by_ref().take(8192).collect();
                    clipped |= chars.next().is_some();
                    prefix
                })
                .collect::<Vec<_>>()
        };
        let a = preview(left);
        let b = preview(right);
        let truncated = a.len() > MAX_LINES || b.len() > MAX_LINES;
        let a = &a[..a.len().min(MAX_LINES)];
        let b = &b[..b.len().min(MAX_LINES)];
        let mut notes = Vec::new();
        if clipped {
            notes.push("Long lines clipped to 8192 characters; changes beyond the preview may not be visible".into());
        }
        if truncated {
            notes.push(
                "Preview limited to 20,000 lines per side; original evidence is unchanged".into(),
            );
        }
        let alignment = align(a, b, 1_000_000).unwrap_or_else(|| {
            notes.push("Large value: positional line alignment (insertions may shift rows)".into());
            (0..a.len().max(b.len()))
                .map(|i| ((i < a.len()).then_some(i), (i < b.len()).then_some(i)))
                .collect()
        });
        let mut pairs = Vec::new();
        let mut deleted = Vec::new();
        let mut inserted = Vec::new();
        let flush = |pairs: &mut Alignment, deleted: &mut Vec<usize>, inserted: &mut Vec<usize>| {
            for i in 0..deleted.len().max(inserted.len()) {
                pairs.push((deleted.get(i).copied(), inserted.get(i).copied()));
            }
            deleted.clear();
            inserted.clear();
        };
        for (i, j) in alignment {
            match (i, j) {
                (Some(i), Some(j)) => {
                    flush(&mut pairs, &mut deleted, &mut inserted);
                    pairs.push((Some(i), Some(j)));
                }
                (Some(i), None) => deleted.push(i),
                (None, Some(j)) => inserted.push(j),
                _ => unreachable!(),
            }
        }
        flush(&mut pairs, &mut deleted, &mut inserted);
        let mut coarse = false;
        let rows = pairs
            .into_iter()
            .map(|(i, j)| {
                let left = i.map(|i| a[i].as_str());
                let right = j.map(|j| b[j].as_str());
                let (expected, actual) = match (left, right) {
                    (Some(left), Some(right)) => {
                        let (expected, actual, fallback) = inline(left, right);
                        coarse |= fallback;
                        (expected, actual)
                    }
                    _ => (
                        left.map(|text| {
                            vec![Segment {
                                text: visible(text),
                                changed: true,
                            }]
                        })
                        .unwrap_or_default(),
                        right
                            .map(|text| {
                                vec![Segment {
                                    text: visible(text),
                                    changed: true,
                                }]
                            })
                            .unwrap_or_default(),
                    ),
                };
                Row {
                    expected_number: i.map(|i| i + 1),
                    actual_number: j.map(|j| j + 1),
                    expected,
                    actual,
                    changed: left != right,
                }
            })
            .collect();
        if coarse {
            notes.push(
                "Long lines: highlights cover the changed range between common prefix and suffix"
                    .into(),
            );
        }
        Self { rows, notes }
    }

    /// Collapse unchanged runs except for three context lines beside changes.
    pub(crate) fn displayed(&self, all: bool) -> Vec<Option<usize>> {
        if all || self.rows.iter().all(|row| !row.changed) {
            return (0..self.rows.len()).map(Some).collect();
        }
        let mut shown = vec![false; self.rows.len()];
        for (index, _) in self.rows.iter().enumerate().filter(|(_, row)| row.changed) {
            for show in &mut shown[index.saturating_sub(3)..(index + 4).min(self.rows.len())] {
                *show = true;
            }
        }
        let mut result = Vec::new();
        for (index, show) in shown.into_iter().enumerate() {
            if show {
                result.push(Some(index));
            } else if result.last() != Some(&None) {
                result.push(None);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reports_all_fields_and_distinguishes_missing_null_and_redaction() {
        let fields = compare(
            &json!({"a":null,"b":"[redacted]","c":1,"d":1}),
            &json!({"b":"[redacted]","c":2,"d":1}),
        )
        .unwrap();
        assert_eq!(fields.len(), 4);
        assert!(fields[0].status == Status::Changed && fields[0].actual.is_none());
        assert!(fields[1].status == Status::Unavailable);
        assert!(fields[2].status == Status::Changed);
        assert!(fields[3].status == Status::Equal);
    }

    #[test]
    fn aligns_insertions_without_marking_following_matching_lines_changed() {
        let diff = TextDiff::new("first\nlast", "first\ninserted\nlast");
        assert_eq!(diff.rows.len(), 3);
        assert!(diff.rows[1].expected_number.is_none());
        assert!(!diff.rows[2].changed);
    }

    #[test]
    fn highlights_only_the_changed_duration_character() {
        let diff = TextDiff::new("Ran 6 tests in 0.001s", "Ran 6 tests in 0.002s");
        let changed = |segments: &[Segment]| {
            segments
                .iter()
                .filter(|s| s.changed)
                .map(|s| s.text.as_str())
                .collect::<String>()
        };
        assert_eq!(changed(&diff.rows[0].expected), "1");
        assert_eq!(changed(&diff.rows[0].actual), "2");
    }

    #[test]
    fn handles_unicode_and_neutralizes_terminal_control_sequences() {
        let diff = TextDiff::new("é🙂\u{1b}[2J", "é🚀\u{1b}[2J");
        assert!(
            diff.rows[0]
                .expected
                .iter()
                .any(|s| s.changed && s.text == "🙂")
        );
        assert!(
            diff.rows[0]
                .actual
                .iter()
                .any(|s| s.changed && s.text == "🚀")
        );
        assert!(!visible("\u{1b}[2J\r").contains('\u{1b}'));
    }

    #[test]
    fn detects_trailing_newlines_and_empty_array_insertions() {
        let diff = TextDiff::new("text", "text\n");
        assert!(diff.rows.iter().any(|r| r.changed));
        let fields = compare(&json!([]), &json!([null])).unwrap();
        assert!(fields[0].status == Status::Changed && fields[0].expected.is_none());
    }
}
