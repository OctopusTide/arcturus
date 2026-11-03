//! Reads a JSON-lines file from a byte offset, one line at a time, with a hard cap on how much
//! of one line is held in memory. A last line without its newline is still being written: it
//! is reported as unfinished and not consumed, so the next pass reads it again.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

/// Longest line kept in memory. Longer lines are skipped (and counted) without buffering them.
pub const MAX_LINE: usize = 64 << 20;

pub enum Line<'a> {
    /// A whole line without its `\n` (and `\r`). `start` is its byte offset.
    Complete { start: u64, bytes: &'a [u8] },
    /// Longer than the cap; skipped.
    TooLong,
    /// End of file in the middle of a line.
    Unfinished,
}

pub struct Lines<R> {
    r: R,
    offset: u64,
    buf: Vec<u8>,
    max: usize,
    done: bool,
}

impl Lines<BufReader<File>> {
    pub fn open(path: &Path, from: u64) -> io::Result<Self> {
        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(from))?;
        Ok(Lines::new(BufReader::with_capacity(256 << 10, f), from, MAX_LINE))
    }
}

impl<R: BufRead> Lines<R> {
    pub fn new(r: R, offset: u64, max: usize) -> Self {
        Lines { r, offset, buf: Vec::new(), max, done: false }
    }

    /// Byte offset just after the last complete line returned.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn next_line(&mut self) -> io::Result<Option<Line<'_>>> {
        if self.done {
            return Ok(None);
        }
        let start = self.offset;
        self.buf.clear();
        let mut len: u64 = 0;
        let mut too_long = false;
        loop {
            let chunk = self.r.fill_buf()?;
            if chunk.is_empty() {
                self.done = true;
                return Ok(if len == 0 { None } else { Some(Line::Unfinished) });
            }
            let (take, newline) = match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (chunk.len(), false),
            };
            let body = if newline { &chunk[..take - 1] } else { &chunk[..take] };
            if !too_long {
                if self.buf.len() + body.len() > self.max {
                    too_long = true;
                    self.buf = Vec::new();
                } else {
                    self.buf.extend_from_slice(body);
                }
            }
            len += take as u64;
            self.r.consume(take);
            if newline {
                break;
            }
        }
        self.offset = start + len;
        if too_long {
            return Ok(Some(Line::TooLong));
        }
        let mut bytes = self.buf.as_slice();
        if start == 0 {
            bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        }
        if let Some(b) = bytes.strip_suffix(b"\r") {
            bytes = b;
        }
        Ok(Some(Line::Complete { start, bytes }))
    }
}

/// The first complete line of a file (for headers such as Codex's `session_meta`).
pub fn first_line(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut lines = Lines::open(path, 0)?;
    Ok(match lines.next_line()? {
        Some(Line::Complete { bytes, .. }) => Some(bytes.to_vec()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(data: &[u8], from: u64, max: usize) -> (Vec<String>, u64) {
        let mut l = Lines::new(&data[from as usize..], from, max);
        let mut out = Vec::new();
        loop {
            let before = l.offset();
            let Some(line) = l.next_line().unwrap() else { break };
            out.push(match line {
                Line::Complete { start, bytes } => format!("{start}:{}", String::from_utf8_lossy(bytes)),
                Line::TooLong => format!("{before}:LONG"),
                Line::Unfinished => format!("{before}:UNFINISHED"),
            });
        }
        (out, l.offset())
    }

    #[test]
    fn lines_offsets_and_unfinished_tail() {
        let data = b"\xEF\xBB\xBFa\r\nbb\n\nccc";
        let (lines, off) = collect(data, 0, 100);
        assert_eq!(lines, ["0:a", "6:bb", "9:", "10:UNFINISHED"]);
        assert_eq!(off, 10, "the unfinished line is not consumed");
        let (lines, off) = collect(data, 6, 100);
        assert_eq!(lines, ["6:bb", "9:", "10:UNFINISHED"]);
        assert_eq!(off, 10);
    }

    #[test]
    fn overlong_lines_are_skipped_not_buffered() {
        let data = b"short\nthis-line-is-too-long\nok\n";
        let (lines, off) = collect(data, 0, 8);
        assert_eq!(lines, ["0:short", "6:LONG", "28:ok"]);
        assert_eq!(off, data.len() as u64);
    }
}
