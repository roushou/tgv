//! Newline-delimited framing for a driven client's stdin and stdout. It knows
//! nothing about JSON: a frame is the bytes between two newlines.

/// The longest line the framer keeps. Longer lines are counted and dropped,
/// never buffered without bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineLimit(usize);

impl LineLimit {
    /// Room for an item that carries a large command output or diff.
    pub const DEFAULT: Self = Self(32 * 1024 * 1024);

    pub const fn bytes(bytes: usize) -> Self {
        Self(bytes)
    }
}

/// One frame read from a client's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Framed {
    /// A complete line, without its newline (or a trailing `\r`).
    Line(Vec<u8>),
    /// A line longer than the limit; its bytes were dropped.
    TooLong { bytes: usize },
}

/// Splits arbitrary byte fragments into lines. The frames it yields do not
/// depend on how the input was split.
#[derive(Debug)]
pub struct Lines {
    buffer: Vec<u8>,
    limit: LineLimit,
    state: Fill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    Keeping,
    /// The current line passed the limit; this many bytes were dropped.
    Dropping(usize),
}

impl Lines {
    pub fn new(limit: LineLimit) -> Self {
        Self {
            buffer: Vec::new(),
            limit,
            state: Fill::Keeping,
        }
    }

    /// The frames completed by `bytes`.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Framed> {
        let mut frames = Vec::new();
        for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
            let (content, ends) = match segment.split_last() {
                Some((b'\n', content)) => (content, true),
                Some(_) | None => (segment, false),
            };
            self.take(content);
            if ends {
                frames.push(self.end());
            }
        }
        frames
    }

    /// The unterminated last line, when the output ended without a newline.
    pub fn finish(mut self) -> Option<Framed> {
        match self.state {
            Fill::Keeping if self.buffer.is_empty() => None,
            Fill::Keeping | Fill::Dropping(_) => Some(self.end()),
        }
    }

    fn take(&mut self, content: &[u8]) {
        match self.state {
            Fill::Keeping if self.buffer.len() + content.len() <= self.limit.0 => {
                self.buffer.extend_from_slice(content);
            }
            Fill::Keeping => {
                self.state = Fill::Dropping(self.buffer.len() + content.len());
                self.buffer = Vec::new();
            }
            Fill::Dropping(dropped) => self.state = Fill::Dropping(dropped + content.len()),
        }
    }

    fn end(&mut self) -> Framed {
        let frame = match self.state {
            Fill::Keeping => {
                let mut line = std::mem::take(&mut self.buffer);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                Framed::Line(line)
            }
            Fill::Dropping(bytes) => Framed::TooLong { bytes },
        };
        self.state = Fill::Keeping;
        frame
    }
}

/// One line for a client's stdin, newline included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line(Vec<u8>);

impl Line {
    /// A JSON value as one line. JSON text never contains a raw newline.
    pub fn json(value: &serde_json::Value) -> Self {
        let mut bytes = value.to_string().into_bytes();
        bytes.push(b'\n');
        Self(bytes)
    }

    /// One line of text, which must hold no newline of its own.
    pub(crate) fn text(text: &str) -> Self {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\n');
        Self(bytes)
    }

    /// Text typed into a terminal, then Enter (a carriage return).
    pub(crate) fn keys(text: &str) -> Self {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\r');
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{Framed, LineLimit, Lines};
    use proptest::prelude::*;

    fn frame_all(limit: usize, parts: &[&[u8]]) -> Vec<Framed> {
        let mut lines = Lines::new(LineLimit::bytes(limit));
        let mut frames: Vec<Framed> = parts.iter().flat_map(|part| lines.push(part)).collect();
        frames.extend(lines.finish());
        frames
    }

    #[test]
    fn splits_lines_and_strips_carriage_returns() {
        assert_eq!(
            frame_all(64, &[b"{\"a\":1}\r\n{\"b\"", b":2}\n\npartial"]),
            vec![
                Framed::Line(b"{\"a\":1}".to_vec()),
                Framed::Line(b"{\"b\":2}".to_vec()),
                Framed::Line(Vec::new()),
                Framed::Line(b"partial".to_vec()),
            ]
        );
    }

    #[test]
    fn drops_overlong_lines_and_counts_them() {
        assert_eq!(
            frame_all(4, &[b"12", b"345", b"6\nok\n1234567"]),
            vec![
                Framed::TooLong { bytes: 6 },
                Framed::Line(b"ok".to_vec()),
                Framed::TooLong { bytes: 7 },
            ]
        );
    }

    proptest! {
        #[test]
        fn frames_do_not_depend_on_fragmentation(
            input in proptest::collection::vec(prop_oneof![Just(b'\n'), Just(b'\r'), any::<u8>()], 0..512),
            cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..16),
            limit in 1usize..64,
        ) {
            let whole = frame_all(limit, &[&input]);
            let mut positions: Vec<usize> = cuts.iter().map(|cut| cut.index(input.len() + 1)).collect();
            positions.sort_unstable();
            let mut parts = Vec::new();
            let mut start = 0;
            for position in positions {
                parts.push(&input[start..position]);
                start = position;
            }
            parts.push(&input[start..]);
            prop_assert_eq!(frame_all(limit, &parts), whole);
        }
    }
}
