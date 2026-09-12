//! The file syntax, before any key means anything (systemd.syntax(7)):
//!
//! - `[Section]` headers, then `Key=Value` lines; whitespace around the key
//!   and the value is dropped.
//! - Lines starting with `#` or `;` are comments, as are blank lines.
//! - A line ending in `\` continues on the next; the backslash becomes a
//!   space, and comment lines inside a continued line are skipped.
//! - Keys may repeat; the order is kept, and what a repeat means is up to the
//!   key (see the service module).
//!
//! The continuation rule is systemd's and so is its trap on Windows: a value
//! that *ends* in a backslash (`WorkingDirectory=C:\Users\`) continues onto the
//! next line. Leave the trailing backslash off.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
    /// 1-based line the entry starts on.
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub line: usize,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitFile {
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for SyntaxError {}

fn is_comment(trimmed: &str) -> bool {
    trimmed.starts_with('#') || trimmed.starts_with(';')
}

pub fn parse(text: &str) -> Result<UnitFile, SyntaxError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut file = UnitFile::default();
    // A logical line being assembled from continued physical lines.
    let mut pending: Option<(String, usize)> = None;

    for (index, raw) in text.split('\n').enumerate() {
        let number = index + 1;
        let trimmed = raw.trim();

        let (mut buffer, start) = match pending.take() {
            Some(continued) => {
                if is_comment(trimmed) {
                    pending = Some(continued);
                    continue;
                }
                continued
            }
            None => {
                if trimmed.is_empty() || is_comment(trimmed) {
                    continue;
                }
                (String::new(), number)
            }
        };

        match trimmed.strip_suffix('\\') {
            Some(head) => {
                buffer.push_str(head);
                buffer.push(' ');
                pending = Some((buffer, start));
            }
            None => {
                buffer.push_str(trimmed);
                logical_line(&mut file, &buffer, start)?;
            }
        }
    }
    // The last line ended in a backslash: take what there is.
    if let Some((buffer, start)) = pending {
        logical_line(&mut file, &buffer, start)?;
    }
    Ok(file)
}

fn logical_line(file: &mut UnitFile, line: &str, number: usize) -> Result<(), SyntaxError> {
    let line = line.trim();
    let error = |message: String| SyntaxError {
        line: number,
        message,
    };

    if let Some(rest) = line.strip_prefix('[') {
        let name = rest
            .strip_suffix(']')
            .ok_or_else(|| error(format!("section header {line:?} has no closing ']'")))?;
        if name.is_empty() || name.contains(['[', ']']) {
            return Err(error(format!("invalid section header {line:?}")));
        }
        file.sections.push(Section {
            name: name.to_owned(),
            line: number,
            entries: Vec::new(),
        });
        return Ok(());
    }

    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| error(format!("expected Key=Value or [Section], found {line:?}")))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(error(format!("assignment with no key: {line:?}")));
    }
    let section = file
        .sections
        .last_mut()
        .ok_or_else(|| error(format!("{key}= appears before any [Section]")))?;
    section.entries.push(Entry {
        key: key.to_owned(),
        value: value.trim().to_owned(),
        line: number,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(file: &UnitFile, section: &str) -> Vec<(String, String, usize)> {
        file.sections
            .iter()
            .filter(|s| s.name == section)
            .flat_map(|s| {
                s.entries
                    .iter()
                    .map(|e| (e.key.clone(), e.value.clone(), e.line))
            })
            .collect()
    }

    #[test]
    fn sections_keys_and_values() {
        let file =
            parse("[Unit]\nDescription = A thing \n\n[Service]\nExecStart=a.exe --x=1\n").unwrap();
        assert_eq!(file.sections.len(), 2);
        assert_eq!(
            entries(&file, "Unit"),
            [("Description".into(), "A thing".into(), 2)]
        );
        // Only the first '=' separates; the rest belongs to the value.
        assert_eq!(
            entries(&file, "Service"),
            [("ExecStart".into(), "a.exe --x=1".into(), 5)]
        );
    }

    #[test]
    fn comments_blank_lines_crlf_and_bom() {
        let file =
            parse("\u{feff}# comment\r\n; also\r\n\r\n[Unit]\r\n  # indented comment\r\nA=b\r\n")
                .unwrap();
        assert_eq!(entries(&file, "Unit"), [("A".into(), "b".into(), 6)]);
    }

    #[test]
    fn repeated_keys_keep_their_order() {
        let file = parse("[Service]\nEnvironment=A=1\nEnvironment=\nEnvironment=B=2\n").unwrap();
        let values: Vec<_> = entries(&file, "Service")
            .into_iter()
            .map(|(_, v, _)| v)
            .collect();
        assert_eq!(values, ["A=1", "", "B=2"]);
    }

    #[test]
    fn continuation_joins_with_a_space_and_skips_comments() {
        let text = "[Service]\nExecStart=\"C:\\Program Files\\x.exe\" \\\n  # not part of it\n  --flag \\\n  --other\nNext=1\n";
        let file = parse(text).unwrap();
        let got = entries(&file, "Service");
        assert_eq!(got[0].0, "ExecStart");
        assert_eq!(got[0].1, "\"C:\\Program Files\\x.exe\"  --flag  --other");
        assert_eq!(got[0].2, 2);
        assert_eq!(got[1], ("Next".into(), "1".into(), 6));
    }

    #[test]
    fn a_trailing_backslash_on_the_last_line_is_kept_as_a_value() {
        let file = parse("[Service]\nWorkingDirectory=C:\\Users\\").unwrap();
        assert_eq!(entries(&file, "Service")[0].1, "C:\\Users");
    }

    #[test]
    fn a_comment_does_not_continue() {
        let file = parse("[Unit]\n# ends in a backslash \\\nA=1\n").unwrap();
        assert_eq!(entries(&file, "Unit"), [("A".into(), "1".into(), 3)]);
    }

    #[test]
    fn errors_name_their_line() {
        assert_eq!(parse("A=1\n").unwrap_err().line, 1);
        assert_eq!(parse("[Unit]\n\njunk\n").unwrap_err().line, 3);
        assert_eq!(parse("[Unit\n").unwrap_err().line, 1);
        assert_eq!(parse("[]\n").unwrap_err().line, 1);
        assert_eq!(parse("[Unit]\n=value\n").unwrap_err().line, 2);
    }
}
