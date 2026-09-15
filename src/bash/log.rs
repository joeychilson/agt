//! A process's output as a terminal shows it, kept in the process's log file.
//!
//! Output passes through a terminal parser as it arrives. Escape sequences
//! are dropped, and carriage returns, backspaces and erasing a line change
//! the line being written as they would on screen, so a progress bar leaves
//! one line rather than hundreds. The file always holds the text so far, the
//! unfinished line included, which is rewritten in place as it changes, so
//! `cat`, `grep` and `tail -f` read the file as they would a terminal's
//! scrollback. Images a command shows are found in the raw output and written
//! as their headers, each on a line of its own.
//!
//! Offsets count bytes of text from the start of the process's output, so they
//! keep their meaning after the file restarts at its size limit.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::pty::{COLS, ROWS};
use crate::image::{Markers, Piece, Shown};
use crate::item::Content;

/// The file restarts once its text reaches this size; the model is told how
/// much earlier output was discarded.
const LOG_CAP: u64 = 64 * 1024 * 1024;
/// An unfinished line this long is ended, so a program that prints without
/// newlines cannot grow the line held in memory without bound.
const LINE_CAP: usize = 64 * 1024;
/// Text the live tail of a process reads from the end of its log.
const TAIL_BYTES: u64 = 2048;

pub(super) struct Log {
    file: File,
    /// Where the file starts in the output: text discarded by restarts.
    base: u64,
    parser: vte::Parser,
    text: Text,
    /// Where the unfinished line starts in the output.
    line_start: u64,
    /// The unfinished line as the file holds it.
    line: String,
    markers: Markers,
    /// Images a command showed that the model has not read, each at the offset
    /// of the text after its header.
    images: Vec<(u64, Shown)>,
    /// The emulated screen, which shows what a full-screen program draws. It
    /// is dropped once the process exits.
    screen: Option<vt100::Parser>,
    /// The start of the first line rewritten since output was last taken: text
    /// the model saw that has changed since, as when a progress line is
    /// overwritten.
    rewritten: u64,
    /// The first failure to write the file.
    error: Option<String>,
}

impl Log {
    pub(super) fn new(file: File) -> Self {
        Self {
            file,
            base: 0,
            parser: vte::Parser::new(),
            text: Text::default(),
            line_start: 0,
            line: String::new(),
            markers: Markers::default(),
            images: Vec::new(),
            screen: Some(vt100::Parser::new(ROWS, COLS, 0)),
            rewritten: u64::MAX,
            error: None,
        }
    }

    /// The offset where the text ends.
    pub(super) fn end(&self) -> u64 {
        self.line_start + self.line.len() as u64
    }

    /// Adds output as the terminal delivered it.
    pub(super) fn append(&mut self, bytes: &[u8]) {
        if let Some(screen) = &mut self.screen {
            screen.process(bytes);
        }
        for piece in self.markers.split(bytes) {
            match piece {
                Piece::Output(bytes) => self.parser.advance(&mut self.text, &bytes),
                Piece::Image(shown) => {
                    if !self.text.cells.is_empty() {
                        self.text.end_line();
                    }
                    self.text.ended.push_str(&shown.header);
                    self.text.ended.push('\n');
                    self.flush();
                    self.images.push((self.end(), shown));
                }
            }
        }
        self.flush();
    }

    /// Ends the output: the start of a marker it ended in was output after all.
    pub(super) fn finish(&mut self) {
        let partial = self.markers.finish();
        self.parser.advance(&mut self.text, &partial);
        self.flush();
    }

    /// Releases what only a running process needs.
    pub(super) fn exited(&mut self) {
        self.screen = None;
    }

    /// Whether there is output a reader at `cursor` has not seen.
    pub(super) fn unread(&self, cursor: u64) -> bool {
        self.end() > cursor || self.rewritten < cursor
    }

    /// Takes the output after `cursor` with the images shown in it, keeping
    /// its first `head` and last `tail` bytes when it is longer, and moves the
    /// cursor to the end. A full-screen program's output is its screen.
    pub(super) fn take(&mut self, cursor: &mut u64, head: u64, tail: u64, path: &Path) -> Content {
        let end = self.end();
        let mut out = Content::default();
        if let Some(screen) = self.alternate_screen() {
            *cursor = end;
            self.rewritten = u64::MAX;
            out.push_str("[screen]\n");
            out.push_str(screen.trim_end());
            return out;
        }
        // Text seen before that has changed since is shown again.
        let from = (*cursor).min(self.rewritten);
        let start = from.max(self.base);
        if start > from {
            out.push_str(&format!("[{} of earlier output discarded]\n", bytes(start - from)));
        }
        *cursor = end;
        self.rewritten = u64::MAX;
        if end - start <= head + tail {
            self.put(&mut out, start, end, Cut::None);
        } else {
            // The lines the cuts fall in are left out.
            self.put(&mut out, start, start + head, Cut::After);
            out.push_str(&format!(
                "\n[… {} omitted; full output in {}]",
                bytes(end - start - head - tail),
                path.display()
            ));
            // An image shown in what is left out still reaches the model.
            for (_, shown) in self.shown(start + head, end - tail) {
                out.push_str(&format!("\n{}\n", shown.header));
                out.push_image(shown.reference.clone());
            }
            out.push_str("\n");
            self.put(&mut out, end - tail, end, Cut::Before);
        }
        out.trim_end();
        // Every image is shown at or before the end, so all of them are taken.
        self.images.clear();
        if let Some(error) = &self.error {
            out.push_str(&format!("\n[log file error: {error}]"));
        }
        out
    }

    /// The last `lines` non-empty lines of output, for showing it live.
    pub(super) fn tail(&self, lines: usize) -> Vec<String> {
        let text = self.alternate_screen().unwrap_or_else(|| {
            let end = self.end();
            self.read(end.saturating_sub(TAIL_BYTES).max(self.base), end)
        });
        let mut tail: Vec<String> = text
            .lines()
            .rev()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .take(lines)
            .map(str::to_owned)
            .collect();
        tail.reverse();
        tail
    }

    /// Writes the text from the unfinished line's start, as lines ended since
    /// the last flush and the unfinished line now, over what the file holds.
    fn flush(&mut self) {
        let mut text = std::mem::take(&mut self.text.ended);
        let ended = text.len();
        text.extend(&self.text.cells);
        if ended == 0 && text == self.line {
            return;
        }
        let mut same = self.line.bytes().zip(text.bytes()).take_while(|(a, b)| a == b).count();
        while !text.is_char_boundary(same) {
            same -= 1;
        }
        // A line that changed is read again whole, so it reads as the terminal
        // shows it.
        if same < self.line.len() {
            self.rewritten = self.rewritten.min(self.line_start);
        }
        let at = self.line_start - self.base + same as u64;
        let mut written = self.file.write_all_at(&text.as_bytes()[same..], at);
        if text.len() < self.line.len() {
            written = written.and_then(|()| self.file.set_len(at + (text.len() - same) as u64));
        }
        self.failed(written);
        self.line_start += ended as u64;
        self.line = text.split_off(ended);
        if self.line_start - self.base >= LOG_CAP {
            let restarted =
                self.file.set_len(0).and_then(|()| self.file.write_all_at(self.line.as_bytes(), 0));
            if restarted.is_ok() {
                self.base = self.line_start;
                let base = self.base;
                self.images.retain(|(offset, _)| *offset >= base);
            }
            self.failed(restarted);
        }
    }

    fn failed(&mut self, result: io::Result<()>) {
        if let Err(error) = result {
            self.error.get_or_insert_with(|| error.to_string());
        }
    }

    /// Adds the text from offset `from` to `to` to `out`, with the images shown
    /// in it after their headers. `cut` leaves out a line cut in half.
    fn put(&self, out: &mut Content, from: u64, to: u64, cut: Cut) {
        let mut at = from;
        let mut pieces: Vec<(String, Option<&Shown>)> = Vec::new();
        for (offset, shown) in self.shown(from, to) {
            pieces.push((self.read(at, offset), Some(shown)));
            at = offset;
        }
        pieces.push((self.read(at, to), None));
        let last = pieces.len() - 1;
        for (index, (text, shown)) in pieces.iter().enumerate() {
            let text = match cut {
                Cut::Before if index == 0 => text.find('\n').map_or("", |cut| &text[cut + 1..]),
                Cut::After if index == last => {
                    text.rfind('\n').map_or(text.as_str(), |cut| &text[..cut])
                }
                _ => text,
            };
            out.push_str(text);
            if let Some(shown) = shown {
                out.push_image(shown.reference.clone());
            }
        }
    }

    /// The images shown after offset `from`, up to and including `to`.
    fn shown(&self, from: u64, to: u64) -> impl Iterator<Item = (u64, &Shown)> {
        self.images
            .iter()
            .filter(move |(offset, _)| from < *offset && *offset <= to)
            .map(|(offset, shown)| (*offset, shown))
    }

    /// The text from offset `from` to `to`, which callers keep within the file.
    fn read(&self, from: u64, to: u64) -> String {
        let mut buf = vec![0; usize::try_from(to.saturating_sub(from)).unwrap_or(0)];
        let mut filled = 0;
        while filled < buf.len() {
            match self.file.read_at(&mut buf[filled..], from - self.base + filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        buf.truncate(filled);
        // Reads start and end at character boundaries, apart from a cut
        // through a line that is left out.
        String::from_utf8(buf)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
    }

    /// The contents of a full-screen program's screen, while one is showing.
    fn alternate_screen(&self) -> Option<String> {
        self.screen
            .as_ref()
            .filter(|parser| parser.screen().alternate_screen())
            .map(|parser| parser.screen().contents())
    }
}

/// Which end of a span of text falls inside a line, which is left out.
#[derive(Clone, Copy)]
enum Cut {
    None,
    /// The span starts inside a line.
    Before,
    /// The span ends inside a line.
    After,
}

/// The text a terminal shows, as output renders it.
#[derive(Default)]
struct Text {
    /// Lines ended since the text was last flushed, each with its newline.
    ended: String,
    /// The unfinished line.
    cells: Vec<char>,
    /// The column output goes to.
    col: usize,
}

impl Text {
    fn end_line(&mut self) {
        self.ended.extend(self.cells.drain(..));
        self.ended.push('\n');
        self.col = 0;
    }
}

impl vte::Perform for Text {
    fn print(&mut self, c: char) {
        match self.cells.get_mut(self.col) {
            Some(cell) => *cell = c,
            None => self.cells.push(c),
        }
        self.col += 1;
        if self.cells.len() >= LINE_CAP {
            self.end_line();
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.end_line(),
            b'\r' => self.col = 0,
            b'\t' => self.print('\t'),
            0x08 => self.col = self.col.saturating_sub(1),
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore || !intermediates.is_empty() || action != 'K' {
            return;
        }
        // Progress displays erase the line before drawing shorter text.
        match params.iter().next().and_then(|param| param.first()).copied().unwrap_or(0) {
            0 => self.cells.truncate(self.col),
            1 => {
                let end = (self.col + 1).min(self.cells.len());
                self.cells[..end].fill(' ');
            }
            2 => self.cells.fill(' '),
            _ => {}
        }
    }
}

/// A byte count in a few characters: `512 B`, `2.0 KB`, `3.1 MB`.
fn bytes(count: u64) -> String {
    match count {
        0..1024 => format!("{count} B"),
        1024..1_048_576 => format!("{:.1} KB", count as f64 / 1024.0),
        _ => format!("{:.1} MB", count as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn log() -> (Log, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        let handle = file.reopen().expect("reopen");
        (Log::new(handle), file)
    }

    /// Everything the log file holds.
    fn contents(file: &tempfile::NamedTempFile) -> String {
        fs::read_to_string(file.path()).expect("log")
    }

    /// The output a reader at `cursor` takes, as text.
    fn take(log: &mut Log, cursor: &mut u64) -> String {
        log.take(cursor, 4096, 8192, Path::new("/s/procs/1.log")).text().to_owned()
    }

    #[test]
    fn the_file_holds_what_a_terminal_shows() {
        let (mut log, file) = log();
        let raw: &[u8] =
            b"\x1b[1;32mok\x1b[0m\r\nstep 1/3\rstep 3/3\r\n\tindent\x08X\n\xe2\x9c\x93 done";
        // Delivered a byte at a time, as a slow terminal would.
        for byte in raw {
            log.append(std::slice::from_ref(byte));
        }
        assert_eq!(
            contents(&file),
            "ok\nstep 3/3\n\tindenX\n✓ done",
            "the unfinished line is there too"
        );
        log.append(b"\r\x1b[Kfinished\n");
        assert_eq!(
            contents(&file),
            "ok\nstep 3/3\n\tindenX\nfinished\n",
            "a shorter line leaves nothing behind"
        );
        let (mut other, file) = self::log();
        other.append(b"abcdef\rxy\x1b[Kz");
        assert_eq!(contents(&file), "xyz");
    }

    #[test]
    fn text_seen_before_it_changed_is_read_again() {
        let (mut log, _file) = log();
        let mut cursor = 0;
        log.append(b"name? ");
        assert_eq!(take(&mut log, &mut cursor), "name?");
        assert!(!log.unread(cursor));
        log.append(b"ada\r\nhi ada\r\n");
        assert_eq!(
            take(&mut log, &mut cursor),
            "ada\nhi ada",
            "appended text is read from where it left off"
        );
        log.append(b"downloading 10%");
        assert_eq!(take(&mut log, &mut cursor), "downloading 10%");
        log.append(b"\rdownloading 90%");
        assert!(log.unread(cursor), "the line the reader saw changed");
        assert_eq!(take(&mut log, &mut cursor), "downloading 90%");
    }

    #[test]
    fn images_are_written_as_their_headers_on_lines_of_their_own() {
        let (mut log, file) = log();
        let shown = Shown {
            reference: "images/ab-4x2.png".into(),
            header: "[image /w/a.png · 4x2]".into(),
        };
        let marker = shown.marker();
        let (first, second) = marker.split_at(9);
        log.append(b"before");
        log.append(first);
        log.append(second);
        log.append(b"after\r\n");
        assert_eq!(contents(&file), "before\n[image /w/a.png · 4x2]\nafter\n");
        let mut cursor = 0;
        let output = log.take(&mut cursor, 4096, 8192, Path::new("/s/procs/1.log"));
        let parts = output.into_parts();
        assert_eq!(parts[0]["text"], "before\n[image /w/a.png · 4x2]\n");
        assert_eq!(parts[1]["image_url"], "images/ab-4x2.png");
        assert_eq!(parts[2]["text"], "after");
    }

    #[test]
    fn long_output_keeps_its_ends_and_the_images_in_between() {
        let (mut log, _file) = log();
        let shown = Shown {
            reference: "images/ab-4x2.png".into(),
            header: "[image /w/a.png · 4x2]".into(),
        };
        let lines: String = (1..=3000).map(|n| format!("{n}\n")).collect();
        log.append(lines.as_bytes());
        log.append(&shown.marker());
        let more: String = (3001..=6000).map(|n| format!("{n}\n")).collect();
        log.append(more.as_bytes());
        let mut cursor = 0;
        let output = log.take(&mut cursor, 4096, 8192, Path::new("/s/procs/1.log"));
        let text = output.text().to_owned();
        assert!(text.starts_with("1\n2\n"), "{text}");
        assert!(text.ends_with("\n6000"), "{text}");
        let omitted =
            text.find("omitted; full output in /s/procs/1.log]").expect("output is left out");
        let header = text.find("]\n[image /w/a.png · 4x2]\n").expect("the header stays");
        assert!(omitted < header, "{text}");
        assert_eq!(crate::image::references(&output.into_output()).count(), 1);
        assert!(text.len() < 4096 + 8192 + 200);
    }

    #[test]
    fn a_full_log_restarts_and_says_what_was_discarded() {
        let (mut log, file) = log();
        let chunk = format!("{}\n", "x".repeat(1023));
        let count = LOG_CAP / 1024;
        for _ in 0..count {
            log.append(chunk.as_bytes());
        }
        log.append(b"kept\n");
        assert_eq!(contents(&file), "kept\n");
        let mut cursor = 0;
        let text = take(&mut log, &mut cursor);
        assert!(text.starts_with("[64.0 MB of earlier output discarded]\n"), "{}", &text[..80]);
        assert!(text.ends_with("kept"), "the tail is read from the restarted file");
    }

    #[test]
    fn lines_without_end_are_bounded() {
        let (mut log, file) = log();
        log.append(&[b'y'; LINE_CAP + 10]);
        let text = contents(&file);
        assert_eq!(text.lines().next().map(str::len), Some(LINE_CAP));
        assert_eq!(log.tail(1), ["y".repeat(10)]);
    }

    #[test]
    fn sizes_format_compactly() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2.0 KB");
        assert_eq!(bytes(3 * 1_048_576), "3.0 MB");
    }
}
