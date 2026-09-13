use crate::{EngineError, EngineResult};

/// Locates content within its original source file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceRange {
    File,
    Byte(ByteRange),
    Text(TextRange),
}

impl SourceRange {
    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        match self {
            Self::File => Ok(()),
            Self::Byte(range) => range.validate(),
            Self::Text(range) => range.validate(),
        }
    }

    /// Assumes both ranges refer to the same source file.
    pub(crate) fn contains(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File, _) => other.validate().is_ok(),
            (Self::Byte(outer), Self::Byte(inner)) => outer.contains(inner),
            (Self::Text(outer), Self::Text(inner)) => outer.contains(inner),
            _ => false,
        }
    }
}

/// Zero-based, half-open offsets in the original file bytes; empty spans are valid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ByteRange {
    pub start_offset: u64,
    pub end_offset: u64,
}

impl ByteRange {
    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if self.start_offset > self.end_offset {
            return Err(EngineError::invalid_argument(format!(
                "invalid byte range: raw file offsets {}..{} must be ordered",
                self.start_offset, self.end_offset
            )));
        }
        Ok(())
    }

    pub(crate) fn contains(&self, other: &Self) -> bool {
        self.validate().is_ok()
            && other.validate().is_ok()
            && self.start_offset <= other.start_offset
            && self.end_offset >= other.end_offset
    }
}

/// A half-open span in decoded UTF-8 text, with global and line-relative byte positions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextRange {
    start: TextPosition,
    end: TextPosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TextPosition {
    byte_offset: usize,
    line: usize,
    byte_column: usize,
}

impl TextRange {
    /// Requires all line starts, sorted, from the same decoded source.
    #[track_caller]
    pub(crate) fn from_offsets(
        source_text: &str,
        line_starts: &[usize],
        start_byte_offset: usize,
        end_byte_offset: usize,
    ) -> EngineResult<Self> {
        if source_text
            .get(start_byte_offset..end_byte_offset)
            .is_none()
        {
            return Err(invalid_text_offsets(
                source_text.len(),
                start_byte_offset,
                end_byte_offset,
            ));
        }
        let position = |byte_offset| {
            let line = line_starts.partition_point(|&start| start <= byte_offset);
            let line_start = *line_starts.get(line.checked_sub(1)?)?;
            if line_starts.first() != Some(&0)
                || (line_start > 0 && source_text.as_bytes().get(line_start - 1) != Some(&b'\n'))
            {
                return None;
            }
            Some(TextPosition {
                byte_offset,
                line,
                byte_column: byte_offset - line_start,
            })
        };
        let (Some(start), Some(end)) = (position(start_byte_offset), position(end_byte_offset))
        else {
            return Err(EngineError::invalid_argument(
                "cannot create text range: line starts must belong to the decoded source",
            ));
        };
        let range = Self { start, end };
        range.validate()?;
        Ok(range)
    }

    #[cfg(test)]
    pub(crate) fn from_text(
        source_text: &str,
        start_byte_offset: usize,
        end_byte_offset: usize,
    ) -> EngineResult<Self> {
        let line_starts = std::iter::once(0)
            .chain(
                source_text
                    .match_indices('\n')
                    .map(|(offset, _)| offset + 1),
            )
            .collect::<Vec<_>>();
        Self::from_offsets(
            source_text,
            &line_starts,
            start_byte_offset,
            end_byte_offset,
        )
    }

    /// Checks recorded coordinates without reading the source.
    #[track_caller]
    pub(crate) fn from_coordinates(
        start_byte_offset: usize,
        end_byte_offset: usize,
        start_line: usize,
        end_line: usize,
        start_byte_column: usize,
        end_byte_column: usize,
    ) -> EngineResult<Self> {
        let range = Self {
            start: TextPosition {
                byte_offset: start_byte_offset,
                line: start_line,
                byte_column: start_byte_column,
            },
            end: TextPosition {
                byte_offset: end_byte_offset,
                line: end_line,
                byte_column: end_byte_column,
            },
        };
        range.validate()?;
        Ok(range)
    }

    pub(crate) fn start_byte_offset(&self) -> usize {
        self.start.byte_offset
    }

    pub(crate) fn end_byte_offset(&self) -> usize {
        self.end.byte_offset
    }

    pub(crate) fn start_line(&self) -> usize {
        self.start.line
    }

    pub(crate) fn end_line(&self) -> usize {
        self.end.line
    }

    pub(crate) fn start_byte_column(&self) -> usize {
        self.start.byte_column
    }

    pub(crate) fn end_byte_column(&self) -> usize {
        self.end.byte_column
    }

    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        let valid_position = |position: TextPosition| {
            position.line > 0
                && position.byte_column <= position.byte_offset
                && if position.line == 1 {
                    position.byte_column == position.byte_offset
                } else {
                    position.byte_offset - position.byte_column >= position.line - 1
                }
        };
        let consistent = valid_position(self.start)
            && valid_position(self.end)
            && self.start.byte_offset <= self.end.byte_offset
            && self.start.line <= self.end.line
            && if self.start.line == self.end.line {
                self.start.byte_offset - self.start.byte_column
                    == self.end.byte_offset - self.end.byte_column
            } else {
                (self.end.byte_offset - self.end.byte_column)
                    .checked_sub(self.start.byte_offset)
                    .is_some_and(|distance| distance >= self.end.line - self.start.line)
            };
        if !consistent {
            return Err(EngineError::invalid_argument(format!(
                "invalid text range: {:?}..{:?}; UTF-8 byte offsets, one-based lines, and byte columns must be ordered and consistent",
                self.start, self.end
            )));
        }
        Ok(())
    }

    /// Reads an exact span, rejecting out-of-bounds offsets and split UTF-8 characters.
    #[track_caller]
    pub(crate) fn slice<'a>(&self, source_text: &'a str) -> EngineResult<&'a str> {
        source_text
            .get(self.start.byte_offset..self.end.byte_offset)
            .ok_or_else(|| {
                invalid_text_offsets(
                    source_text.len(),
                    self.start.byte_offset,
                    self.end.byte_offset,
                )
            })
    }

    pub(crate) fn contains(&self, other: &Self) -> bool {
        self.start.byte_offset <= other.start.byte_offset
            && self.end.byte_offset >= other.end.byte_offset
    }
}

#[track_caller]
fn invalid_text_offsets(source_len: usize, start: usize, end: usize) -> EngineError {
    EngineError::invalid_argument(format!(
        "cannot read text range {start}..{end}: offsets must be ordered, within the decoded source ({source_len} bytes), and on UTF-8 character boundaries"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(source: &str, start: usize, end: usize) -> SourceRange {
        SourceRange::Text(TextRange::from_text(source, start, end).expect("valid text range"))
    }

    fn bytes(start_offset: u64, end_offset: u64) -> SourceRange {
        SourceRange::Byte(ByteRange {
            start_offset,
            end_offset,
        })
    }

    #[test]
    fn text_spans_read_decoded_sources_and_reject_invalid_boundaries() {
        let source_text = "A中😀\r\nB";
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend(source_text.encode_utf16().flat_map(u16::to_le_bytes));
        let mut utf16_be = vec![0xfe, 0xff];
        utf16_be.extend(source_text.encode_utf16().flat_map(u16::to_be_bytes));

        for bytes in [source_text.as_bytes().to_vec(), utf16, utf16_be] {
            let decoded = crate::utils::decode_text(&bytes, true).expect("valid source encoding");
            assert_eq!(decoded, source_text);
            for (start, end, expected) in [(1, 8, "中😀"), (8, 11, "\r\nB"), (11, 11, "")] {
                let range = TextRange::from_text(&decoded, start, end).expect("valid range");
                assert_eq!(range.slice(&decoded).expect("valid source span"), expected);
            }
            for (start, end) in [(2, 8), (1, 6), (8, 1), (0, 12), (usize::MAX, usize::MAX)] {
                assert_eq!(
                    TextRange::from_text(&decoded, start, end)
                        .expect_err("invalid source span")
                        .code(),
                    EngineError::INVALID_ARGUMENT,
                );
            }
            let range = TextRange::from_text(&decoded, 1, 8).expect("valid range");
            assert!(range.slice("short").is_err());
        }
    }

    #[test]
    fn ranges_validate_coordinate_boundaries() {
        for (range, valid) in [
            (SourceRange::File, true),
            (text("abc", 0, 3), true),
            (text("", 0, 0), true),
            (bytes(0, 8), true),
            (bytes(0, 0), true),
            (bytes(u64::MAX, u64::MAX), true),
            (bytes(1, 0), false),
        ] {
            assert_eq!(range.validate().is_ok(), valid, "{range:?}");
            assert_eq!(range.contains(&range), valid, "{range:?}");
            assert_eq!(SourceRange::File.contains(&range), valid, "{range:?}");
        }
        for (start, end, first_line, last_line, first_column, last_column) in [
            (0, 8, 0, 2, 0, 1),
            (0, 8, 3, 2, 0, 1),
            (9, 8, 1, 1, 9, 8),
            (0, 8, 1, 1, 0, 7),
            (2, 8, 1, 2, 1, 0),
            (2, 8, 2, 2, 3, 9),
            (2, 8, 2, 3, 0, 7),
            (0, 0, 1, 2, 0, 0),
            (0, 1, 2, 2, 0, 1),
        ] {
            assert!(
                TextRange::from_coordinates(
                    start,
                    end,
                    first_line,
                    last_line,
                    first_column,
                    last_column,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn containment_checks_both_boundaries() {
        let source = "ab\ncd\nefghi";
        let outer_text = text(source, 0, 10);
        for (outer, inner, contained) in [
            (outer_text, text(source, 3, 6), true),
            (outer_text, text(source, 10, 10), true),
            (outer_text, text(source, 0, 11), false),
            (text(source, 3, 6), text(source, 2, 5), false),
            (bytes(2, 10), bytes(3, 8), true),
            (bytes(2, 10), bytes(1, 8), false),
            (bytes(0, 10), bytes(10, 10), true),
            (bytes(0, 10), bytes(10, 11), false),
            (bytes(0, 10), bytes(8, 2), false),
            (bytes(8, 2), bytes(3, 5), false),
        ] {
            assert_eq!(
                outer.contains(&inner),
                contained,
                "{outer:?} contains {inner:?}"
            );
        }
    }

    #[test]
    fn containment_requires_matching_coordinate_spaces() {
        let ranges = [text("0123456789", 0, 10), bytes(0, 10)];
        for (outer_index, outer) in ranges.iter().enumerate() {
            for (inner_index, inner) in ranges.iter().enumerate() {
                assert_eq!(outer.contains(inner), outer_index == inner_index);
            }
            assert!(!outer.contains(&SourceRange::File));
        }
    }

    #[test]
    fn text_positions_share_an_exclusive_endpoint() {
        let source = "A中😀\r\nB\n";
        let line_starts = [0, 10, 12];
        for (start, end, first_line, last_line, first_column, last_column) in [
            (0, 0, 1, 1, 0, 0),
            (1, 8, 1, 1, 1, 8),
            (1, 10, 1, 2, 1, 0),
            (10, 11, 2, 2, 0, 1),
            (0, 12, 1, 3, 0, 0),
            (12, 12, 3, 3, 0, 0),
        ] {
            let range = TextRange::from_offsets(source, &line_starts, start, end)
                .expect("valid indexed coordinates");
            assert_eq!(
                (
                    range.start_line(),
                    range.end_line(),
                    range.start_byte_column(),
                    range.end_byte_column()
                ),
                (first_line, last_line, first_column, last_column),
            );
            assert_eq!(
                (range.start_byte_offset(), range.end_byte_offset()),
                (start, end)
            );
            assert_eq!(
                range.slice(source).expect("valid source span"),
                &source[start..end]
            );
        }
        assert!(TextRange::from_offsets(source, &[], 0, 1).is_err());
        assert!(TextRange::from_offsets(source, &[1], 1, 8).is_err());
        assert!(TextRange::from_offsets(source, &[0, 2], 10, 11).is_err());
    }
}
