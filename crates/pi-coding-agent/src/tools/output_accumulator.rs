//! Bounded-memory accumulator for streaming tool output.
//!
//! Ground truth: pi-mono
//! `packages/coding-agent/src/core/tools/output-accumulator.ts`.
//!
//! Appends decode chunks with a streaming UTF-8 decoder, keeps only a decoded
//! tail for display snapshots, and spills to a temp file once the output grows
//! past the limits so the full output survives for the model to be pointed at.
//!
//! Porting note: pi's `TextDecoder` holds back an incomplete trailing UTF-8
//! sequence until the next chunk; `String::from_utf8_lossy` would emit a
//! replacement character instead, so multi-byte characters split across chunk
//! boundaries are decoded by hand in [`OutputAccumulator::decode_streaming`].

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationOptions, TruncationResult, format_size,
    truncate_tail,
};

/// pi's `OutputAccumulatorOptions` (`output-accumulator.ts:6-10`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutputAccumulatorOptions {
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
    pub temp_file_prefix: Option<String>,
}

impl OutputAccumulatorOptions {
    /// Options with only the temp-file prefix set.
    pub fn with_prefix(prefix: &str) -> OutputAccumulatorOptions {
        OutputAccumulatorOptions {
            temp_file_prefix: Some(prefix.to_string()),
            ..OutputAccumulatorOptions::default()
        }
    }
}

/// pi's `OutputSnapshot` (`output-accumulator.ts:12-16`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSnapshot {
    pub content: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<PathBuf>,
}

/// pi's `FullOutput` (`output-accumulator.ts:18-22`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullOutput {
    pub content: String,
    /// Whether `content` omits part of the output.
    pub truncated: bool,
}

/// pi's `OutputAccumulator` (`output-accumulator.ts:35-257`).
#[derive(Debug)]
pub struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    max_rolling_bytes: usize,
    temp_file_prefix: String,

    /// Bytes of an incomplete UTF-8 sequence carried to the next chunk.
    pending_utf8: Vec<u8>,
    raw_chunks: Vec<Vec<u8>>,
    tail_text: String,
    tail_bytes: usize,
    tail_starts_at_line_boundary: bool,
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    total_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    finished: bool,

    temp_file_path: Option<PathBuf>,
    temp_file: Option<File>,
}

impl OutputAccumulator {
    /// pi's constructor (`output-accumulator.ts:57-62`).
    pub fn new(options: OutputAccumulatorOptions) -> OutputAccumulator {
        let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
        let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        OutputAccumulator {
            max_rolling_bytes: std::cmp::max(max_bytes * 2, 1),
            max_lines,
            max_bytes,
            temp_file_prefix: options
                .temp_file_prefix
                .unwrap_or_else(|| "pi-output".to_string()),
            pending_utf8: Vec::new(),
            raw_chunks: Vec::new(),
            tail_text: String::new(),
            tail_bytes: 0,
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            total_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            finished: false,
            temp_file_path: None,
            temp_file: None,
        }
    }

    /// pi's `append` (`output-accumulator.ts:64-78`). Panics on a finished
    /// accumulator, which pi also refuses (`output-accumulator.ts:65-67`); the
    /// tools never do that.
    pub fn append(&mut self, data: &[u8]) {
        assert!(
            !self.finished,
            "Cannot append to a finished output accumulator"
        );
        self.total_raw_bytes += data.len();
        let text = self.decode_streaming(data);
        self.append_decoded_text(&text);

        if self.temp_file.is_some() || self.should_use_temp_file() {
            self.ensure_temp_file();
            if let Some(file) = self.temp_file.as_mut() {
                let _ = file.write_all(data);
            }
        } else if !data.is_empty() {
            self.raw_chunks.push(data.to_vec());
        }
    }

    /// pi's `finish` (`output-accumulator.ts:80-89`).
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let text = self.decode_streaming(&[]);
        self.append_decoded_text(&text);
        if self.should_use_temp_file() {
            self.ensure_temp_file();
        }
    }

    /// pi's `snapshot` (`output-accumulator.ts:91-119`).
    ///
    /// `persist_if_truncated` is pi's `options.persistIfTruncated`.
    pub fn snapshot(&mut self, persist_if_truncated: bool) -> OutputSnapshot {
        let tail_truncation = truncate_tail(
            &self.get_snapshot_text(),
            TruncationOptions {
                max_lines: Some(self.max_lines),
                max_bytes: Some(self.max_bytes),
            },
        );
        let truncated =
            self.total_lines > self.max_lines || self.total_decoded_bytes > self.max_bytes;
        let truncated_by = if truncated {
            Some(tail_truncation.truncated_by.unwrap_or(
                if self.total_decoded_bytes > self.max_bytes {
                    crate::tools::truncate::TruncatedBy::Bytes
                } else {
                    crate::tools::truncate::TruncatedBy::Lines
                },
            ))
        } else {
            None
        };
        let truncation = TruncationResult {
            total_lines: self.total_lines,
            total_bytes: self.total_decoded_bytes,
            truncated,
            truncated_by,
            max_lines: self.max_lines,
            max_bytes: self.max_bytes,
            ..tail_truncation
        };

        if persist_if_truncated && truncation.truncated {
            self.ensure_temp_file();
        }

        OutputSnapshot {
            content: truncation.content.clone(),
            truncation,
            full_output_path: self.temp_file_path.clone(),
        }
    }

    /// pi's `closeTempFile` (`output-accumulator.ts:121-142`).
    pub fn close_temp_file(&mut self) {
        if let Some(file) = self.temp_file.take() {
            let _ = file.sync_all();
        }
    }

    /// pi's `readFullOutput` (`output-accumulator.ts:149-176`).
    ///
    /// Call after [`OutputAccumulator::finish`] and
    /// [`OutputAccumulator::close_temp_file`]. Output longer than `max_bytes`
    /// keeps its head and tail around an omission marker.
    pub fn read_full_output(&self, max_bytes: usize) -> std::io::Result<FullOutput> {
        let Some(path) = self.temp_file_path.as_ref() else {
            let mut raw = Vec::new();
            for chunk in &self.raw_chunks {
                raw.extend_from_slice(chunk);
            }
            return Ok(FullOutput {
                content: String::from_utf8_lossy(&raw).into_owned(),
                truncated: false,
            });
        };

        let mut file = File::open(path)?;
        let size = file.metadata()?.len() as usize;
        if size <= max_bytes {
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;
            return Ok(FullOutput {
                content: String::from_utf8_lossy(&buffer).into_owned(),
                truncated: false,
            });
        }

        let head_bytes = max_bytes / 2;
        let tail_bytes = max_bytes - head_bytes;
        let mut head = vec![0u8; head_bytes];
        let mut tail = vec![0u8; tail_bytes];
        file.seek(SeekFrom::Start(0))?;
        let _ = file.read(&mut head)?;
        file.seek(SeekFrom::Start((size - tail_bytes) as u64))?;
        let _ = file.read(&mut tail)?;

        // Cut at character boundaries: the head decode holds back an incomplete
        // trailing sequence, the tail skips leading continuation bytes
        // (`output-accumulator.ts:165-170`).
        let head_text = decode_prefix(&head);
        let mut tail_start = 0;
        while tail_start < tail.len() && tail[tail_start] & 0xc0 == 0x80 {
            tail_start += 1;
        }
        let tail_text = String::from_utf8_lossy(&tail[tail_start..]).into_owned();
        let omitted = size - head_bytes - tail_bytes;
        Ok(FullOutput {
            content: format!("{head_text}\n\n[... {omitted} bytes omitted ...]\n\n{tail_text}"),
            truncated: true,
        })
    }

    /// pi's `getLastLineBytes` (`output-accumulator.ts:178-180`).
    pub fn last_line_bytes(&self) -> usize {
        self.current_line_bytes
    }

    /// pi's `appendDecodedText` (`output-accumulator.ts:182-211`).
    fn append_decoded_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bytes = text.len();
        self.total_decoded_bytes += bytes;
        self.tail_text.push_str(text);
        self.tail_bytes += bytes;
        if self.tail_bytes > self.max_rolling_bytes * 2 {
            self.trim_tail();
        }

        let newlines = text.bytes().filter(|byte| *byte == b'\n').count();
        if newlines == 0 {
            self.current_line_bytes += bytes;
            self.has_open_line = true;
        } else {
            self.completed_lines += newlines;
            let tail = match text.rfind('\n') {
                Some(index) => &text[index + 1..],
                None => "",
            };
            self.current_line_bytes = tail.len();
            self.has_open_line = !tail.is_empty();
        }
        self.total_lines = self.completed_lines + usize::from(self.has_open_line);
    }

    /// pi's `trimTail` (`output-accumulator.ts:213-228`).
    fn trim_tail(&mut self) {
        let buffer = self.tail_text.as_bytes();
        if buffer.len() <= self.max_rolling_bytes {
            self.tail_bytes = buffer.len();
            return;
        }
        let mut start = buffer.len() - self.max_rolling_bytes;
        while start < buffer.len() && buffer[start] & 0xc0 == 0x80 {
            start += 1;
        }
        if start != 0 {
            self.tail_starts_at_line_boundary = buffer[start - 1] == b'\n';
        }
        self.tail_text = self.tail_text[start..].to_string();
        self.tail_bytes = self.tail_text.len();
    }

    /// pi's `getSnapshotText` (`output-accumulator.ts:230-237`).
    fn get_snapshot_text(&self) -> String {
        if self.tail_starts_at_line_boundary {
            return self.tail_text.clone();
        }
        match self.tail_text.find('\n') {
            Some(index) => self.tail_text[index + 1..].to_string(),
            None => self.tail_text.clone(),
        }
    }

    /// pi's `shouldUseTempFile` (`output-accumulator.ts:239-243`).
    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > self.max_bytes
            || self.total_decoded_bytes > self.max_bytes
            || self.total_lines > self.max_lines
    }

    /// pi's `ensureTempFile` (`output-accumulator.ts:245-256`).
    fn ensure_temp_file(&mut self) {
        if self.temp_file_path.is_some() {
            return;
        }
        let path = temp_file_path(&self.temp_file_prefix);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .or_else(|_| {
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)
            })
            .expect("temp output file can be created");
        for chunk in &self.raw_chunks {
            let _ = file.write_all(chunk);
        }
        self.raw_chunks = Vec::new();
        self.temp_file = Some(file);
        self.temp_file_path = Some(path);
    }

    /// `TextDecoder.decode(chunk, {stream: true})` plus the flush in `finish`:
    /// an incomplete trailing sequence is held back, invalid bytes become one
    /// replacement character per maximal invalid subsequence.
    fn decode_streaming(&mut self, data: &[u8]) -> String {
        let mut buffer = std::mem::take(&mut self.pending_utf8);
        buffer.extend_from_slice(data);
        match std::str::from_utf8(&buffer) {
            Ok(text) => {
                self.pending_utf8.clear();
                text.to_string()
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                let text = String::from_utf8_lossy(&buffer[..valid_up_to]).into_owned();
                let incomplete = &buffer[valid_up_to..];
                self.pending_utf8 = if error.error_len().is_none() {
                    incomplete.to_vec()
                } else {
                    // Genuinely invalid bytes: emit replacements and continue.
                    String::from_utf8_lossy(incomplete).into_owned();
                    Vec::new()
                };
                text
            }
        }
    }
}

/// pi's `defaultTempFilePath` (`utils/output-files.ts`): the same
/// `<tmpdir>/<prefix>-<random>.log` shape.
fn temp_file_path(prefix: &str) -> PathBuf {
    let id = random_hex(8);
    std::env::temp_dir().join(format!("{prefix}-{id}.log"))
}

/// 8 random bytes as hex, in the role of pi's `randomBytes(8)`. A pid+clock
/// mix keeps this deterministic-free without pulling in a randomness crate.
fn random_hex(bytes: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64)
        ^ (std::process::id() as u64) << 32;
    let mut hex = String::with_capacity(bytes * 2);
    for _ in 0..bytes {
        // xorshift64*
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let _ = write!(hex, "{:02x}", (state & 0xff) as u8);
    }
    hex
}

/// Decode a prefix, dropping an incomplete trailing sequence (pi's
/// `new TextDecoder().decode(head, { stream: true })`).
fn decode_prefix(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(error) if error.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The bash footer pi appends when output was truncated
/// (`bash.ts:366-384` `formatOutput`). `last_line_bytes` is
/// [`OutputAccumulator::last_line_bytes`] at snapshot time; the path renders
/// as `undefined` when the snapshot has none, which pi's template literal does.
pub fn truncation_footer(snapshot: &OutputSnapshot, last_line_bytes: usize) -> String {
    let truncation = &snapshot.truncation;
    if !truncation.truncated {
        return String::new();
    }
    let path = snapshot.full_output_path.as_ref().map_or_else(
        || "undefined".to_string(),
        |path| path.display().to_string(),
    );
    let start_line = truncation.total_lines - truncation.output_lines + 1;
    let end_line = truncation.total_lines;
    if truncation.last_line_partial {
        format!(
            "\n\n[Showing last {} of line {} (line is {}). Full output: {}]",
            format_size(truncation.output_bytes),
            end_line,
            format_size(last_line_bytes),
            path
        )
    } else if truncation.truncated_by == Some(crate::tools::truncate::TruncatedBy::Lines) {
        format!(
            "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {path}]",
            truncation.total_lines
        )
    } else {
        format!(
            "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {path}]",
            truncation.total_lines,
            format_size(DEFAULT_MAX_BYTES)
        )
    }
}
