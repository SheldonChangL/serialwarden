//! Decode health and baud suggestions, inferred from what the device has
//! already been recorded saying (issue #50).
//!
//! # Why this replaced "the next common rate"
//!
//! The first version of this check (T5.3, issue #20) measured how much of
//! the recent output failed to decode and, past a threshold, suggested the
//! first entry of a fixed list of rates that wasn't the current one. On a
//! Realtek RTL8735B that had dropped into its UART download mode at 115200,
//! that meant suggesting 74880 — an ESP8266 boot-ROM rate, unrelated to the
//! chip and in the wrong direction. Everything needed for a better answer
//! was already in the recording:
//!
//! 1. the device had printed clean text at 115200 minutes earlier, so 115200
//!    was not "the wrong setting" — the device changed modes;
//! 2. that text named the chip (`RTL8735B_VOE_1.7.1.0`);
//! 3. nothing at all decoded any more.
//!
//! [`infer`] uses that evidence, in this priority order:
//!
//! 1. **History** — which rates this device has produced readable text at
//!    *in the current connection*. If the current rate was readable within
//!    the last [`MODE_SWITCH_WINDOW_S`] and now nothing arriving is text,
//!    the device appears to have switched modes, and only faster rates are
//!    offered (a rate the device was readable at first, then a chip
//!    fingerprint's rate, then the next common rates up) — never that rate
//!    or one below it.
//! 2. **Fingerprint** — a chip/ROM string from [`FINGERPRINTS`] seen in this
//!    connection's text maps to the rate its platform documents, carried
//!    with its source URL so the GUI shows where the number comes from.
//! 3. **Direction** — nothing arriving now is text and there is no history
//!    or fingerprint: faster rates are tried first.
//! 4. **Common** — otherwise the next common rate, said to be exactly that.
//!
//! A rate already tried in this connection without producing a readable
//! line (garbage, binary or silence) is not offered again; the next
//! candidate is.
//!
//! # Where the evidence comes from
//!
//! [`EvidenceTracker`] is fed by the query layer's ingest loop
//! (`crate::query::DeviceQueryState::ingest`) record by record: raw `rx`
//! bytes, assembled lines, and `connect`/`config_change`/`config_reapplied`
//! events. It keeps per-rate segments since the last connect plus the newest
//! [`RECENT_RX_BYTES`] of raw bytes, so a `decode_health` query costs a few
//! KiB of work no matter how much history the device has, and never holds
//! the line store's lock.
//!
//! # What this does not do
//!
//! It never derives a rate from the garbled bytes themselves. The GUI's
//! "try" button tests a suggestion empirically instead (`webui/src/lib/baudTrial.ts`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

use crate::query::AssembledLine;

/// One known chip/ROM banner and the rate its platform documents for the
/// mode that banner implies. To add one: a literal `pattern` that appears
/// in that platform's own log text, the `baud`, a `source_url` that states
/// the number, and a `reason` saying exactly what the number is (and what
/// it is not). No entry goes in without a source anyone can check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    /// Case-sensitive substring searched for in recorded text lines.
    pub pattern: &'static str,
    /// Human-readable platform name, shown next to the suggestion.
    pub platform: &'static str,
    pub baud: u32,
    pub source_url: &'static str,
    /// Further pages the `reason` relies on, shown as extra links.
    pub also_see: &'static [&'static str],
    /// What `baud` is, as one or two sentences without a final period,
    /// shown verbatim in the GUI.
    pub reason: &'static str,
}

/// The fingerprint table. See [`Fingerprint`] for how to extend it.
pub const FINGERPRINTS: &[Fingerprint] = &[
    Fingerprint {
        pattern: "RTL8735B",
        platform: "Realtek RTL8735B (AmebaPro2)",
        baud: 1_500_000,
        source_url: "https://aiot.realmcu.com/en/latest/tools/image_tool/index.html",
        also_see: &[
            "https://ameba-doc-rtos-pro2-sdk.readthedocs-hosted.com/en/latest/application_note/04_IMAGE.html",
        ],
        reason: "1500000 is the default download rate of Realtek's Image Tool, not a property of \
                 the chip: the ROM itself announces 115200 in download mode, and the rate after \
                 that is whatever the host tool's -b option sets (the AmebaPro2 SDK's uartfwburn \
                 examples use 3000000)",
    },
    Fingerprint {
        pattern: "ets Jan",
        platform: "Espressif ESP8266 boot ROM",
        baud: 74_880,
        source_url: "https://docs.espressif.com/projects/esptool/en/latest/esp8266/advanced-topics/boot-mode-selection.html",
        also_see: &[],
        reason: "the ESP8266 boot ROM prints its boot log at 74880 baud",
    },
];

/// Below this many sampled (non-neutral) bytes no suggestion is made.
pub const MIN_SAMPLE_BYTES: usize = 32;

/// Fraction of non-text bytes at/above which a suggestion is made.
pub const UNDECODABLE_THRESHOLD: f64 = 0.2;

/// The bytes after the sample's last readable line count as "no text now"
/// only when they are at least [`MIN_SAMPLE_BYTES`] non-neutral bytes, at
/// least this fraction of them is non-text, and they are at least
/// [`NO_TEXT_NOW_MIN_SHARE`] of the sample — a short binary burst after a
/// screenful of text (a GPS's UBX frame between NMEA sentences) is not
/// "the device stopped sending text".
const NO_TEXT_NOW_RATIO: f64 = 0.5;
const NO_TEXT_NOW_MIN_SHARE: f64 = 0.25;

/// How many of the newest raw `rx` bytes are kept for the decode-health
/// sample. Raw bytes rather than assembled lines: a stream that stopped
/// being text often stops containing line breaks too, and binary-protocol
/// framing detection needs every byte.
pub const RECENT_RX_BYTES: usize = 8 * 1024;

/// "Readable at this rate before, nothing now" is a mode switch only if the
/// readable text is this recent (seconds, daemon monotonic clock). Older
/// than that, a reflash to another rate is as likely as a mode switch.
pub const MODE_SWITCH_WINDOW_S: f64 = 600.0;

/// A line must have at least this many characters to count as readable.
const READABLE_LINE_MIN_CHARS: usize = 8;

/// This many readable lines at a rate is what "this device produced
/// readable text at that rate" means.
const READABLE_LINES_FOR_HISTORY: usize = 5;

/// Per-rate segments kept since the last connect.
const MAX_SEGMENTS: usize = 64;

/// How many further candidates a suggestion lists after its own rate.
const MAX_ALTERNATIVES: usize = 3;

/// Standard rates, ascending — the "faster rates first" order.
const RATES_ASCENDING: &[u32] = &[
    9600, 19_200, 38_400, 57_600, 115_200, 230_400, 460_800, 921_600, 1_500_000, 3_000_000,
];

/// Last-resort order when there is no evidence at all. 74880 is
/// deliberately absent: it is one chip family's ROM rate and lives in
/// [`FINGERPRINTS`], where it is only suggested when that chip is seen.
const FALLBACK_ORDER: &[u32] = &[
    115_200, 9600, 57_600, 38_400, 19_200, 230_400, 460_800, 921_600,
];

/// What a suggestion rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    History,
    Fingerprint,
    Direction,
    Common,
}

/// The fingerprint that produced a [`Basis::Fingerprint`] suggestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FingerprintHit {
    pub pattern: &'static str,
    pub platform: &'static str,
    pub source_url: &'static str,
    pub also_see: &'static [&'static str],
    pub reason: &'static str,
}

/// A suggested rate and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BaudSuggestion {
    pub baud: u32,
    pub basis: Basis,
    /// The current rate produced readable text recently and nothing
    /// arriving now is text — "the device appears to have switched modes".
    pub mode_switch: bool,
    /// Rates this device has produced readable text at in this connection,
    /// ascending.
    pub readable_bauds: Vec<u32>,
    /// Rates tried in this connection without a readable line — never
    /// suggested again until the device reconnects.
    pub tried_bauds: Vec<u32>,
    /// The next candidates by the same rules, in order — what to try if
    /// `baud` doesn't work out.
    pub alternatives: Vec<u32>,
    pub fingerprint: Option<FingerprintHit>,
    /// One or two plain sentences stating the basis, for display.
    pub explanation: String,
}

/// `GET /api/devices/:id/config`'s `decode_health` field.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct DecodeHealth {
    /// Raw bytes sampled: the newest recorded `rx` bytes since the rate
    /// last changed (at most [`RECENT_RX_BYTES`]).
    pub checked_bytes: usize,
    /// Fraction of the sample that isn't text: invalid UTF-8 plus control
    /// characters, over the sample minus neutral bytes (NUL, BEL, BS, VT,
    /// FF, SO, SI, DEL — see [`is_neutral`]). `0.0` when nothing counted.
    pub undecodable_ratio: f64,
    /// Readable text lines in the sample (CR/LF-separated).
    pub text_lines: usize,
    /// `Some("slip")` when the sample is structured binary protocol traffic
    /// — the suggestion is then withheld.
    pub binary_protocol: Option<&'static str>,
    /// `t_wall` of the newest sampled chunk — lets the GUI tell a live
    /// garbled stream from a stale one even when no complete line arrived.
    pub newest_sample_t_wall: Option<String>,
    /// What the recorded events say about the port right now — lets a
    /// baud trial tell "the device went quiet" from "the device went away".
    pub port: PortState,
    /// How many `connect` events this connection-scoped evidence has seen;
    /// a change mid-trial means the device reconnected.
    pub connects: u64,
    /// `suggestion.baud`, kept as its own field for existing clients.
    pub suggested_baud: Option<u32>,
    pub suggestion: Option<BaudSuggestion>,
}

/// The port's state as the recorded events tell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PortState {
    /// No `connect`/`disconnect`/`lease_*` event seen yet.
    #[default]
    Unknown,
    Open,
    Disconnected,
    /// A `lease_start` without its `lease_end`: a tool (often a flasher) has
    /// the port.
    Leased,
}

/// One raw `rx` record's bytes, as kept by [`EvidenceTracker`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecentRxChunk {
    pub seq: u64,
    pub t_mono: f64,
    pub t_wall: String,
    pub bytes: Vec<u8>,
}

/// A stretch of the stream read at one rate.
#[derive(Debug, Clone, PartialEq)]
struct Segment {
    /// Seq of the event that started it; `None` for the implicit first
    /// segment, which covers everything from the start of the record.
    start_seq: Option<u64>,
    /// `None` until something names the rate (a connect before its
    /// `config_change`, or a connect whose apply failed).
    baud: Option<u32>,
    rx_bytes: usize,
    readable_lines: usize,
    last_readable_t_mono: Option<f64>,
}

impl Segment {
    fn new(start_seq: Option<u64>, baud: Option<u32>) -> Self {
        Self {
            start_seq,
            baud,
            rx_bytes: 0,
            readable_lines: 0,
            last_readable_t_mono: None,
        }
    }
}

/// Incrementally-built baud evidence for one device — see the module docs.
#[derive(Debug, Clone, Default)]
pub struct EvidenceTracker {
    /// Since the last connect, oldest first; at most [`MAX_SEGMENTS`].
    segments: VecDeque<Segment>,
    /// The newest fingerprint seen since the last connect.
    fingerprint: Option<&'static Fingerprint>,
    recent: VecDeque<RecentRxChunk>,
    /// Sum of `recent`'s byte lengths, kept as it changes.
    recent_bytes: usize,
    port: PortState,
    connects: u64,
}

/// A rate out of a `config_change` event's `old`/`new` value: the full
/// `PortConfig` object the daemon records, or a bare number.
fn baud_of(value: Option<&serde_json::Value>) -> Option<u32> {
    let v = value?;
    let n = v
        .as_u64()
        .or_else(|| v.get("baud").and_then(|b| b.as_u64()))?;
    u32::try_from(n).ok()
}

impl EvidenceTracker {
    fn current_mut(&mut self) -> &mut Segment {
        if self.segments.is_empty() {
            self.segments.push_back(Segment::new(None, None));
        }
        self.segments.back_mut().expect("just ensured non-empty")
    }

    fn reset(&mut self, seq: u64, baud: Option<u32>) {
        self.segments.clear();
        self.segments.push_back(Segment::new(Some(seq), baud));
        self.fingerprint = None;
    }

    fn switch_to(&mut self, seq: u64, baud: u32) {
        if self.current_mut().baud == Some(baud) {
            return;
        }
        self.segments.push_back(Segment::new(Some(seq), Some(baud)));
        while self.segments.len() > MAX_SEGMENTS {
            self.segments.pop_front();
        }
    }

    /// An `event` record. `connect` starts a fresh connection; the
    /// `config_change` every connect writes (`changed_by: "system:connect"`)
    /// does too, and names its rate. Any other `config_change` or
    /// `config_reapplied` moves to a new rate only if the port actually
    /// applied it (`applied` absent is treated as applied, for records
    /// written before that field existed): a change the port rejected left
    /// it reading at the old rate, so it is no evidence about the new one.
    pub fn on_event(
        &mut self,
        seq: u64,
        name: &str,
        extra: &serde_json::Map<String, serde_json::Value>,
    ) {
        let applied = extra
            .get("applied")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        match name {
            "connect" => {
                self.reset(seq, None);
                self.port = PortState::Open;
                self.connects += 1;
            }
            "disconnect" => self.port = PortState::Disconnected,
            "lease_start" => self.port = PortState::Leased,
            "lease_end" if self.port == PortState::Leased => self.port = PortState::Unknown,
            "config_change" => {
                let new = baud_of(extra.get("new"));
                if extra.get("changed_by").and_then(|v| v.as_str()) == Some("system:connect") {
                    self.reset(seq, new.filter(|_| applied));
                    return;
                }
                if !applied {
                    return;
                }
                let current = self.current_mut();
                if current.baud.is_none() {
                    current.baud = baud_of(extra.get("old"));
                }
                if let Some(new) = new {
                    self.switch_to(seq, new);
                }
            }
            "config_reapplied" if applied => {
                if let Some(baud) = baud_of(extra.get("config")) {
                    self.switch_to(seq, baud);
                }
            }
            _ => {}
        }
    }

    /// Raw `rx` bytes, as recorded.
    pub fn on_rx(&mut self, seq: u64, t_mono: f64, t_wall: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.current_mut().rx_bytes += bytes.len();
        let keep_from = bytes.len().saturating_sub(RECENT_RX_BYTES);
        self.recent.push_back(RecentRxChunk {
            seq,
            t_mono,
            t_wall: t_wall.to_string(),
            bytes: bytes[keep_from..].to_vec(),
        });
        self.recent_bytes += bytes.len() - keep_from;
        while let Some(front) = self.recent.front() {
            if self.recent_bytes - front.bytes.len() < RECENT_RX_BYTES {
                break;
            }
            self.recent_bytes -= front.bytes.len();
            self.recent.pop_front();
        }
    }

    /// An assembled line, in the order the query layer completes them.
    pub fn on_line(&mut self, line: &AssembledLine) {
        if is_readable_line(&line.raw) {
            let current = self.current_mut();
            current.readable_lines += 1;
            current.last_readable_t_mono = Some(line.t_mono);
        }
        // Lossy text: a banner often shares a line with garbage left in
        // the partial buffer by a mode switch, and an ASCII pattern
        // survives lossy decoding intact.
        if let Some(f) = FINGERPRINTS.iter().find(|f| line.text.contains(f.pattern)) {
            self.fingerprint = Some(f);
        }
    }

    /// The newest raw `rx` chunks held, oldest first.
    pub fn recent(&self) -> Vec<RecentRxChunk> {
        self.recent.iter().cloned().collect()
    }
}

/// Bytes that are neither evidence of text nor of garbage: terminal and
/// printer controls real consoles emit at the correct rate (a backspace
/// progress counter, NUL padding, shift-in/out). They are left out of both
/// sides of the undecodable ratio.
fn is_neutral(c: char) -> bool {
    matches!(
        c,
        '\0' | '\x07' | '\x08' | '\x0b' | '\x0c' | '\x0e' | '\x0f' | '\x7f'
    )
}

fn is_text_char(c: char) -> bool {
    !c.is_control() || matches!(c, '\t' | '\n' | '\r' | '\x1b')
}

/// `(non_text, neutral)` byte counts for `bytes`. Non-text: every byte of
/// an invalid UTF-8 sequence (a truncated sequence at the very end counts —
/// nothing more is coming in a point-in-time sample) plus control
/// characters other than tab, CR, LF, ESC and the [`is_neutral`] ones.
/// Control characters count because a mismatched rate produces plenty of
/// them and they are valid UTF-8.
pub fn classify_bytes(bytes: &[u8]) -> (usize, usize) {
    let mut non_text = 0usize;
    let mut neutral = 0usize;
    let mut rest = bytes;
    while !rest.is_empty() {
        let (valid, bad_len) = match std::str::from_utf8(rest) {
            Ok(s) => (s, 0),
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                let bad = e.error_len().unwrap_or(rest.len() - valid_up_to);
                // `valid_up_to` is a UTF-8 boundary by contract.
                (
                    std::str::from_utf8(&rest[..valid_up_to]).unwrap_or_default(),
                    bad,
                )
            }
        };
        for c in valid.chars() {
            if is_neutral(c) {
                neutral += 1;
            } else if !is_text_char(c) {
                non_text += c.len_utf8();
            }
        }
        non_text += bad_len;
        rest = &rest[valid.len() + bad_len..];
    }
    (non_text, neutral)
}

/// A run of at least [`MIN_SAMPLE_BYTES`] that is mostly neutral bytes
/// (at least half) with almost no text in it (under a quarter): what
/// reading far faster than the device sends looks like — each low bit
/// arrives as a break, so the stream is NULs. Neutral bytes only stay
/// neutral mixed into text (a progress counter, NUL padding after a line);
/// on their own they are the garbage.
fn neutral_garble(bytes: &[u8]) -> bool {
    let (non_text, neutral) = classify_bytes(bytes);
    let text = bytes.len() - non_text - neutral;
    bytes.len() >= MIN_SAMPLE_BYTES && neutral * 2 >= bytes.len() && text * 4 < bytes.len()
}

/// Whether NUL is most of `bytes` — the specific signature of reading
/// faster than the device sends.
fn mostly_nul(bytes: &[u8]) -> bool {
    bytes.iter().filter(|&&b| b == 0).count() * 2 >= bytes.len()
}

/// `non_text / (len - neutral)`, or `0.0` when nothing counts.
fn non_text_ratio(bytes: &[u8]) -> (f64, usize) {
    let (non_text, neutral) = classify_bytes(bytes);
    let counted = bytes.len() - neutral;
    let ratio = if counted == 0 {
        0.0
    } else {
        non_text as f64 / counted as f64
    };
    (ratio, counted)
}

/// Whether one line (terminator already stripped) is readable text: valid
/// UTF-8, at least [`READABLE_LINE_MIN_CHARS`] characters once neutral
/// controls are dropped and it is trimmed, and nothing else that isn't text.
pub fn is_readable_line(raw: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw) else {
        return false;
    };
    let kept: String = text.chars().filter(|&c| !is_neutral(c)).collect();
    let kept = kept.trim();
    kept.chars().count() >= READABLE_LINE_MIN_CHARS
        && kept
            .chars()
            .all(|c| is_text_char(c) && !matches!(c, '\n' | '\r'))
}

/// Whether `bytes` is SLIP-framed traffic (RFC 1055) — the structured
/// binary protocol that must not trigger a baud suggestion. All of:
///
/// - at least [`MIN_SAMPLE_BYTES`] bytes;
/// - at least 3 complete, non-empty frames between `0xC0` delimiters
///   (back-to-back `0xC0`s are empty frames and ignored, as SLIP allows);
/// - every `0xDB` (ESC) is followed by `0xDC` or `0xDD`;
/// - regular frame lengths: every complete frame is within a factor of two
///   of the median frame length;
/// - the partial frames before the first and after the last delimiter (the
///   sample window cuts frames) are no longer than the longest complete
///   frame, so the framing covers the whole sample.
///
/// A baud mismatch produces `0xC0` about once every 256 bytes with
/// geometrically distributed gaps and invalid escapes, which fails the
/// regularity and escape checks; a real SLIP link passes all of them.
pub fn looks_like_slip(bytes: &[u8]) -> bool {
    if bytes.len() < MIN_SAMPLE_BYTES {
        return false;
    }
    let delimiters: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter(|(_, &b)| b == 0xC0)
        .map(|(i, _)| i)
        .collect();
    if delimiters.len() < 2 {
        return false;
    }
    let frames: Vec<&[u8]> = delimiters
        .windows(2)
        .map(|w| &bytes[w[0] + 1..w[1]])
        .filter(|f| !f.is_empty())
        .collect();
    if frames.len() < 3 {
        return false;
    }
    let escapes_valid = |frame: &[u8]| {
        frame
            .iter()
            .enumerate()
            .all(|(i, &b)| b != 0xDB || matches!(frame.get(i + 1), Some(0xDC | 0xDD)))
    };
    let leading = &bytes[..delimiters[0]];
    let trailing = &bytes[delimiters[delimiters.len() - 1] + 1..];
    if !frames.iter().all(|f| escapes_valid(f)) {
        return false;
    }
    // A cut-off escape at the very end of the trailing partial frame is
    // fine; anywhere else it must be valid.
    let trailing_body = trailing.strip_suffix(&[0xDB]).unwrap_or(trailing);
    if !escapes_valid(leading) || !escapes_valid(trailing_body) {
        return false;
    }
    let mut lengths: Vec<usize> = frames.iter().map(|f| f.len()).collect();
    lengths.sort_unstable();
    let median = lengths[lengths.len() / 2];
    let max = lengths[lengths.len() - 1];
    lengths.iter().all(|&l| l * 2 >= median && l <= median * 2)
        && leading.len() <= max
        && trailing.len() <= max
}

/// Count readable CR/LF-separated pieces of `sample` and return the byte
/// offset just past the last one (`0` if there is none).
fn text_profile(sample: &[u8]) -> (usize, usize) {
    let mut count = 0usize;
    let mut last_end = 0usize;
    let mut start = 0usize;
    for i in 0..=sample.len() {
        if i == sample.len() || sample[i] == b'\n' || sample[i] == b'\r' {
            if is_readable_line(&sample[start..i]) {
                count += 1;
                last_end = i;
            }
            start = i + 1;
        }
    }
    (count, last_end)
}

/// What the segments since the last connect say, per rate.
struct History {
    readable_lines: BTreeMap<u32, usize>,
    last_readable_t_mono: BTreeMap<u32, f64>,
    /// Rate → index of its newest segment holding a readable line.
    last_readable_segment: BTreeMap<u32, usize>,
    tried: BTreeSet<u32>,
}

fn summarize(segments: &VecDeque<Segment>, current: u32) -> History {
    let mut history = History {
        readable_lines: BTreeMap::new(),
        last_readable_t_mono: BTreeMap::new(),
        last_readable_segment: BTreeMap::new(),
        tried: BTreeSet::new(),
    };
    let last = segments.len().saturating_sub(1);
    let mut newest_segment_of: BTreeMap<u32, usize> = BTreeMap::new();
    for (i, seg) in segments.iter().enumerate() {
        // The current segment's rate is `current` even when nothing named
        // it (it is what the port is configured to now).
        let Some(baud) = seg.baud.or((i == last).then_some(current)) else {
            continue;
        };
        newest_segment_of.insert(baud, i);
        if seg.readable_lines > 0 {
            *history.readable_lines.entry(baud).or_default() += seg.readable_lines;
            history.last_readable_segment.insert(baud, i);
            if let Some(t) = seg.last_readable_t_mono {
                let entry = history.last_readable_t_mono.entry(baud).or_insert(t);
                *entry = entry.max(t);
            }
        }
    }
    for (baud, i) in newest_segment_of {
        if baud != current && i != last && segments[i].readable_lines == 0 {
            history.tried.insert(baud);
        }
    }
    history
}

/// Decode health of the recent sample plus, if warranted, a suggested rate
/// and its basis — see the module docs for the rules. `configured` is the
/// rate the saved configuration names; the tracker's own idea of the rate
/// the port is actually reading at wins when it has one (a change the port
/// rejected leaves it on the old rate).
pub fn infer(configured: u32, evidence: &EvidenceTracker) -> DecodeHealth {
    let current_segment = evidence.segments.back();
    let current = current_segment.and_then(|s| s.baud).unwrap_or(configured);
    let since = current_segment.and_then(|s| s.start_seq);
    let chunks: Vec<&RecentRxChunk> = evidence
        .recent
        .iter()
        .filter(|c| since.is_none_or(|s| c.seq > s))
        .collect();
    let sample: Vec<u8> = chunks
        .iter()
        .flat_map(|c| c.bytes.iter().copied())
        .collect();
    let (text_lines, last_text_end) = text_profile(&sample);
    // With no readable line, a mostly-neutral sample is garbage, and every
    // non-text byte (neutral ones included) is reported as undecodable.
    let sample_neutral_garble = text_lines == 0 && neutral_garble(&sample);
    let (undecodable_ratio, counted) = if sample_neutral_garble {
        let (non_text, neutral) = classify_bytes(&sample);
        (
            (non_text + neutral) as f64 / sample.len() as f64,
            sample.len(),
        )
    } else {
        non_text_ratio(&sample)
    };
    // What arrived after the last readable line: "now".
    let trailing = &sample[last_text_end..];
    let (trailing_ratio, trailing_counted) = non_text_ratio(trailing);
    let trailing_garbled = (trailing_counted >= MIN_SAMPLE_BYTES
        && trailing_ratio >= NO_TEXT_NOW_RATIO)
        || neutral_garble(trailing);
    let no_text_now =
        trailing_garbled && trailing.len() as f64 >= sample.len() as f64 * NO_TEXT_NOW_MIN_SHARE;
    let too_fast = no_text_now && mostly_nul(trailing);
    let mut health = DecodeHealth {
        checked_bytes: sample.len(),
        undecodable_ratio,
        text_lines,
        newest_sample_t_wall: chunks.last().map(|c| c.t_wall.clone()),
        port: evidence.port,
        connects: evidence.connects,
        ..DecodeHealth::default()
    };
    let garbled = counted >= MIN_SAMPLE_BYTES && undecodable_ratio >= UNDECODABLE_THRESHOLD;
    if !garbled && !no_text_now {
        return health;
    }
    if looks_like_slip(&sample) || looks_like_slip(trailing) {
        health.binary_protocol = Some("slip");
        return health;
    }
    let history = summarize(&evidence.segments, current);
    let newest_t = chunks.last().map(|c| c.t_mono);
    let recently_readable_here = history
        .last_readable_t_mono
        .get(&current)
        .is_some_and(|&t| newest_t.is_none_or(|now| now - t <= MODE_SWITCH_WINDOW_S));
    // NULs mean reading faster than the device sends, which is the
    // opposite of a switch into a faster mode.
    let mode_switch = no_text_now
        && !too_fast
        && recently_readable_here
        && history.readable_lines.get(&current).copied().unwrap_or(0) >= READABLE_LINES_FOR_HISTORY;
    health.suggestion = suggest(
        current,
        no_text_now,
        too_fast,
        mode_switch,
        &history,
        evidence.fingerprint,
    );
    health.suggested_baud = health.suggestion.as_ref().map(|s| s.baud);
    health
}

struct Candidate {
    baud: u32,
    basis: Basis,
    fingerprint: Option<&'static Fingerprint>,
    explanation: String,
}

fn suggest(
    current: u32,
    no_text_now: bool,
    too_fast: bool,
    mode_switch: bool,
    history: &History,
    fingerprint: Option<&'static Fingerprint>,
) -> Option<BaudSuggestion> {
    let excluded = |b: u32| b == current || history.tried.contains(&b);
    let readable_bauds: Vec<u32> = history
        .readable_lines
        .iter()
        .filter(|(_, &n)| n >= READABLE_LINES_FOR_HISTORY)
        .map(|(&b, _)| b)
        .collect();
    let fingerprint_sentence = |f: &Fingerprint| {
        format!(
            "This connection's log printed \"{}\" ({}): {}.",
            f.pattern, f.platform, f.reason
        )
    };
    let mut candidates: Vec<Candidate> = Vec::new();

    if mode_switch {
        let switched = format!(
            "It printed readable text at {current} recently and nothing arriving now is text, so \
             the device appears to have switched modes."
        );
        // Faster rates only: a mode switch into a bootloader or download
        // protocol almost always speeds up. Current evidence (a faster
        // rate this device was readable at) ranks above a table default.
        for &b in readable_bauds.iter().filter(|&&b| b > current) {
            candidates.push(Candidate {
                baud: b,
                basis: Basis::History,
                fingerprint: None,
                explanation: format!(
                    "{switched} It printed readable text at {b} earlier in this connection."
                ),
            });
        }
        if let Some(f) = fingerprint.filter(|f| f.baud > current) {
            candidates.push(Candidate {
                baud: f.baud,
                basis: Basis::Fingerprint,
                fingerprint: Some(f),
                explanation: format!("{switched} {}", fingerprint_sentence(f)),
            });
        }
        for &b in RATES_ASCENDING.iter().filter(|&&b| b > current) {
            candidates.push(Candidate {
                baud: b,
                basis: Basis::History,
                fingerprint: None,
                explanation: format!(
                    "{switched} Faster rates are tried first in that case, so {b} is next — an \
                     order to try in, not a reading off these bytes."
                ),
            });
        }
    } else {
        let mut others: Vec<u32> = readable_bauds
            .iter()
            .copied()
            .filter(|&b| b != current)
            .collect();
        others.sort_by_key(|b| {
            std::cmp::Reverse(history.last_readable_segment.get(b).copied().unwrap_or(0))
        });
        for b in others {
            candidates.push(Candidate {
                baud: b,
                basis: Basis::History,
                fingerprint: None,
                explanation: format!(
                    "This device printed readable text at {b} earlier in this connection."
                ),
            });
        }
        if let Some(f) = fingerprint {
            candidates.push(Candidate {
                baud: f.baud,
                basis: Basis::Fingerprint,
                fingerprint: Some(f),
                explanation: fingerprint_sentence(f),
            });
        }
        if too_fast {
            for &b in RATES_ASCENDING.iter().rev().filter(|&&b| b < current) {
                candidates.push(Candidate {
                    baud: b,
                    basis: Basis::Direction,
                    fingerprint: None,
                    explanation: format!(
                        "What arrives now is mostly NUL bytes, which is how a stream read far                          faster than it is sent looks. Slower rates are tried first, so {b} is                          next — an order to try in, not a reading off these bytes."
                    ),
                });
            }
        } else if no_text_now {
            for &b in RATES_ASCENDING.iter().filter(|&&b| b > current) {
                candidates.push(Candidate {
                    baud: b,
                    basis: Basis::Direction,
                    fingerprint: None,
                    explanation: format!(
                        "Nothing arriving now is text. Faster rates are tried first, so {b} is \
                         next — an order to try in, not a reading off these bytes."
                    ),
                });
            }
        }
        for &b in FALLBACK_ORDER {
            candidates.push(Candidate {
                baud: b,
                basis: Basis::Common,
                fingerprint: None,
                explanation: format!(
                    "{b} is just the next common rate to try, not a reading off these bytes."
                ),
            });
        }
    }

    let mut seen = BTreeSet::new();
    let mut ordered = candidates
        .into_iter()
        .filter(|c| !excluded(c.baud) && seen.insert(c.baud));
    let first = ordered.next()?;
    let alternatives: Vec<u32> = ordered.take(MAX_ALTERNATIVES).map(|c| c.baud).collect();
    let tried_bauds: Vec<u32> = history.tried.iter().copied().collect();
    let mut explanation = first.explanation;
    if !tried_bauds.is_empty() {
        let list: Vec<String> = tried_bauds.iter().map(u32::to_string).collect();
        explanation.push_str(&format!(
            " Already tried in this connection without readable text: {}.",
            list.join(", ")
        ));
    }
    Some(BaudSuggestion {
        baud: first.baud,
        basis: first.basis,
        mode_switch,
        readable_bauds,
        tried_bauds,
        alternatives,
        fingerprint: first.fingerprint.map(|f| FingerprintHit {
            pattern: f.pattern,
            platform: f.platform,
            source_url: f.source_url,
            also_see: f.also_see,
            reason: f.reason,
        }),
        explanation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map, Value};

    /// The issue's reference capture: an RTL8735B in UART download mode,
    /// read at 115200.
    const ISSUE_SAMPLE: &[u8] = &[
        0xf7, 0x08, 0x32, 0x08, 0xc8, 0x86, 0x84, 0x08, 0x04, 0x85, 0xe6, 0xc4, 0x08, //
        0x08, 0x88, 0x8f, 0x08, 0x3e, 0x06, 0x81, 0xe6, 0xc4, 0x08, //
        0x08, 0x88, 0x8f, 0x08, 0x3f, 0x06, 0x87, 0xe6, 0xf4, 0x08,
    ];

    /// The same device's normal-mode output at 115200, from the issue.
    const ISSUE_TEXT: &[&str] = &[
        "voe   :RTL8735B_VOE_1.7.1.0",
        "Set H264 default HIGH profile",
        "[video_pre_init_procedure] START",
    ];

    /// Feeds an [`EvidenceTracker`] the way the query layer does, with a
    /// controllable clock.
    struct Sim {
        tracker: EvidenceTracker,
        seq: u64,
        t: f64,
    }

    fn extra(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    impl Sim {
        fn new() -> Self {
            Self {
                tracker: EvidenceTracker::default(),
                seq: 0,
                t: 1000.0,
            }
        }
        fn next(&mut self) -> u64 {
            self.seq += 1;
            self.t += 0.01;
            self.seq
        }
        fn wait(&mut self, secs: f64) {
            self.t += secs;
        }
        /// What every real connect writes: `connect`, then the
        /// `system:connect` `config_change` naming the opened rate.
        fn connect(&mut self, baud: u32) {
            let s = self.next();
            self.tracker.on_event(s, "connect", &Map::new());
            let s = self.next();
            self.tracker.on_event(
                s,
                "config_change",
                &extra(&[
                    ("old", Value::Null),
                    ("new", json!({ "baud": baud })),
                    ("changed_by", "system:connect".into()),
                    ("applied", true.into()),
                ]),
            );
        }
        fn change(&mut self, old: u32, new: u32, applied: bool) {
            let s = self.next();
            self.tracker.on_event(
                s,
                "config_change",
                &extra(&[
                    ("old", json!({ "baud": old })),
                    ("new", json!({ "baud": new })),
                    ("changed_by", "gui".into()),
                    ("applied", applied.into()),
                ]),
            );
        }
        fn line(&mut self, raw: &[u8]) {
            let s = self.next();
            let mut bytes = raw.to_vec();
            bytes.extend(b"\r\n");
            self.tracker
                .on_rx(s, self.t, "2026-10-06T00:00:00Z", &bytes);
            self.tracker.on_line(&AssembledLine {
                raw: raw.to_vec(),
                text: String::from_utf8_lossy(raw).into_owned(),
                seq: s,
                t_mono: self.t,
                t_wall: "2026-10-06T00:00:00Z".into(),
                capped: false,
            });
        }
        fn text(&mut self, s: &str) {
            self.line(s.as_bytes());
        }
        fn texts(&mut self, n: usize) {
            for i in 0..n {
                self.text(&format!("[app] heartbeat tick {i}"));
            }
        }
        /// Raw bytes that complete no line.
        fn bytes(&mut self, b: &[u8]) {
            let s = self.next();
            self.tracker.on_rx(s, self.t, "2026-10-06T00:00:00Z", b);
        }
        fn garbage(&mut self, n: usize) {
            for _ in 0..n {
                self.bytes(ISSUE_SAMPLE);
            }
        }
        fn health(&self, configured: u32) -> DecodeHealth {
            infer(configured, &self.tracker)
        }
        fn suggestion(&self, configured: u32) -> BaudSuggestion {
            self.health(configured).suggestion.expect("suggestion")
        }
    }

    #[test]
    fn classify_counts_garbage_and_controls_but_not_text_or_neutral_bytes() {
        assert_eq!(
            classify_bytes(b"hello\tworld\r\n\x1b[31mred\x1b[0m"),
            (0, 0)
        );
        assert_eq!(classify_bytes(&[0x80]), (1, 0));
        assert_eq!(classify_bytes(&[b'a', 0x04, b'b', 0x06]), (2, 0));
        assert_eq!(classify_bytes(b" 42%\x08\x08\x08\x08\0\x0e\x0f"), (0, 7));
        // A truncated multi-byte sequence at the very end is undecodable.
        assert_eq!(classify_bytes(&[b'a', 0xe6, 0x97]), (2, 0));
        assert_eq!(classify_bytes("溫度 25°C".as_bytes()), (0, 0));
    }

    #[test]
    fn the_issue_sample_is_overwhelmingly_non_text_and_not_slip() {
        let (ratio, _) = non_text_ratio(ISSUE_SAMPLE);
        assert!(ratio > 0.6, "ratio {ratio}");
        assert!(!looks_like_slip(ISSUE_SAMPLE));
        assert!(!looks_like_slip(&ISSUE_SAMPLE.repeat(40)));
    }

    #[test]
    fn clean_text_gets_no_suggestion() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.texts(10);
        let health = sim.health(115_200);
        assert_eq!(health.undecodable_ratio, 0.0);
        assert_eq!(health.text_lines, 10);
        assert_eq!(health.suggestion, None);
    }

    #[test]
    fn a_sample_below_the_minimum_size_gets_no_suggestion() {
        let mut sim = Sim::new();
        sim.bytes(&[0x80; MIN_SAMPLE_BYTES - 1]);
        let health = sim.health(115_200);
        assert_eq!(health.undecodable_ratio, 1.0);
        assert_eq!(health.suggestion, None);
    }

    #[test]
    fn nothing_sampled_reports_zero_and_no_suggestion() {
        let health = Sim::new().health(115_200);
        assert_eq!(health.checked_bytes, 0);
        assert_eq!(health.undecodable_ratio, 0.0);
        assert_eq!(health.newest_sample_t_wall, None);
        assert_eq!(health.suggestion, None);
    }

    /// Readable at 115200 a moment ago, now all binary: the suggestion goes
    /// up, never to 115200 or below, and says why.
    #[test]
    fn readable_at_the_current_rate_then_all_binary_suggests_upward() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.texts(20);
        sim.garbage(10);
        let s = sim.suggestion(115_200);
        assert_eq!(s.basis, Basis::History);
        assert!(s.mode_switch);
        assert_eq!(s.baud, 230_400);
        assert_eq!(s.alternatives, vec![460_800, 921_600, 1_500_000]);
        assert_eq!(s.readable_bauds, vec![115_200]);
        assert!(
            s.explanation.contains("switched modes"),
            "{}",
            s.explanation
        );
        assert!(
            s.explanation.contains("order to try in"),
            "{}",
            s.explanation
        );
    }

    #[test]
    fn a_faster_rate_this_device_was_readable_at_outranks_a_fingerprint() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.text(ISSUE_TEXT[0]);
        sim.change(115_200, 921_600, true);
        sim.texts(10);
        sim.change(921_600, 115_200, true);
        sim.texts(10);
        sim.garbage(10);
        let s = sim.suggestion(115_200);
        assert_eq!(s.basis, Basis::History);
        assert!(s.mode_switch);
        assert_eq!(s.baud, 921_600);
        assert_eq!(s.alternatives[0], 1_500_000);
    }

    /// The issue's own data: the RTL8735B banner, then the download-mode
    /// bytes — 1500000 with its source, and wording that says what the
    /// number is and isn't.
    #[test]
    fn the_rtl8735b_fingerprint_suggests_1500000_with_its_sources() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        for t in ISSUE_TEXT {
            sim.text(t);
        }
        sim.texts(10);
        sim.garbage(10);
        let s = sim.suggestion(115_200);
        assert_eq!(s.baud, 1_500_000);
        assert_eq!(s.basis, Basis::Fingerprint);
        assert!(s.mode_switch);
        let fp = s.fingerprint.expect("fingerprint carried through");
        assert_eq!(fp.pattern, "RTL8735B");
        assert_eq!(
            fp.source_url,
            "https://aiot.realmcu.com/en/latest/tools/image_tool/index.html"
        );
        assert_eq!(fp.also_see.len(), 1);
        for needle in [
            "Image Tool",
            "-b",
            "3000000",
            "announces 115200",
            "switched modes",
        ] {
            assert!(
                s.explanation.contains(needle),
                "{needle}: {}",
                s.explanation
            );
        }
    }

    #[test]
    fn a_fingerprint_is_found_in_a_line_that_also_holds_garbage() {
        let mut sim = Sim::new();
        sim.connect(9600);
        let mut raw = ISSUE_SAMPLE.to_vec();
        raw.extend(ISSUE_TEXT[0].as_bytes());
        sim.line(&raw);
        sim.garbage(3);
        let s = sim.suggestion(9600);
        assert_eq!(s.basis, Basis::Fingerprint);
        assert!(!s.mode_switch);
        assert_eq!(s.baud, 1_500_000);
    }

    #[test]
    fn a_mode_switch_never_follows_a_fingerprint_downward() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.text("ets Jan  8 2013,rst cause:2, boot mode:(3,6)");
        sim.texts(10);
        sim.garbage(3);
        let s = sim.suggestion(115_200);
        assert!(s.baud > 115_200, "suggested {}", s.baud);
        assert_eq!(s.basis, Basis::History);
    }

    /// All binary, zero text lines, no history at all — a suggestion still
    /// appears, and faster rates come first.
    #[test]
    fn all_binary_with_no_history_suggests_the_next_rate_up() {
        let mut sim = Sim::new();
        sim.garbage(2);
        let health = sim.health(115_200);
        assert_eq!(health.text_lines, 0);
        let s = health.suggestion.expect("suggestion");
        assert_eq!(s.basis, Basis::Direction);
        assert_eq!(s.baud, 230_400);
        assert!(!s.mode_switch);
    }

    #[test]
    fn garble_with_text_still_arriving_falls_back_to_the_common_list() {
        let mut sim = Sim::new();
        sim.connect(9600);
        sim.text("[boot] starting services");
        sim.garbage(3);
        sim.bytes(b"\r\n");
        sim.text("[boot] services up and running");
        let s = sim.suggestion(9600);
        assert_eq!(s.basis, Basis::Common);
        assert_eq!(s.baud, 115_200);
        assert!(s.explanation.contains("not a reading off these bytes"));
    }

    #[test]
    fn only_bytes_since_the_last_rate_change_are_sampled() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.garbage(5);
        sim.change(115_200, 1_500_000, true);
        sim.text("nor download success, rebooting now");
        let health = sim.health(1_500_000);
        assert_eq!(health.undecodable_ratio, 0.0);
        assert_eq!(
            health.checked_bytes,
            "nor download success, rebooting now\r\n".len()
        );
        assert_eq!(health.suggestion, None);
    }

    /// Review item 2: readable text from an earlier connection (another
    /// board on the same adapter) is not evidence about this one.
    #[test]
    fn history_from_an_earlier_connection_is_not_a_mode_switch() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        for t in ISSUE_TEXT {
            sim.text(t);
        }
        sim.texts(20);
        sim.connect(115_200);
        sim.garbage(10);
        let s = sim.suggestion(115_200);
        assert!(!s.mode_switch);
        assert_eq!(s.basis, Basis::Direction);
        assert!(s.fingerprint.is_none());
        assert!(s.readable_bauds.is_empty());
    }

    #[test]
    fn readable_text_older_than_the_window_is_not_a_mode_switch() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.texts(20);
        sim.wait(MODE_SWITCH_WINDOW_S + 60.0);
        sim.garbage(10);
        let s = sim.suggestion(115_200);
        assert!(!s.mode_switch);
        assert_eq!(s.basis, Basis::Direction);
    }

    /// Review item 3: a rate tried with binary output and no line break
    /// (so no assembled line at all) is not suggested again; the next one is.
    #[test]
    fn a_rate_tried_with_only_binary_is_not_suggested_again() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.texts(10);
        sim.garbage(5);
        sim.change(115_200, 230_400, true);
        sim.garbage(20);
        sim.change(230_400, 115_200, true);
        sim.garbage(5);
        let s = sim.suggestion(115_200);
        assert_eq!(s.baud, 460_800);
        assert_eq!(s.tried_bauds, vec![230_400]);
        assert!(s.explanation.contains("Already tried"), "{}", s.explanation);
    }

    #[test]
    fn a_rate_tried_in_silence_is_not_suggested_again_either() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        for t in ISSUE_TEXT {
            sim.text(t);
        }
        sim.texts(10);
        sim.garbage(5);
        sim.change(115_200, 1_500_000, true);
        sim.change(1_500_000, 115_200, true);
        sim.garbage(5);
        let s = sim.suggestion(115_200);
        assert_eq!(s.tried_bauds, vec![1_500_000]);
        assert_ne!(s.baud, 1_500_000);
        assert_eq!(s.baud, 230_400);
    }

    /// Review item 6: a change the port rejected left it reading at the old
    /// rate — no new segment, nothing counted as tried, sampling continues.
    #[test]
    fn a_change_the_port_rejected_is_not_a_tried_rate() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.texts(10);
        sim.garbage(5);
        sim.change(115_200, 1_500_000, false);
        sim.garbage(5);
        // The saved configuration says 1500000; the port still reads 115200.
        let health = sim.health(1_500_000);
        // Sampling did not restart at the rejected change: all ten bursts count.
        assert!(
            health.checked_bytes >= 10 * ISSUE_SAMPLE.len(),
            "{health:?}"
        );
        let s = health.suggestion.expect("suggestion");
        assert!(s.tried_bauds.is_empty());
        assert!(s.mode_switch);
        assert_eq!(s.baud, 230_400);
    }

    #[test]
    fn readable_history_at_another_rate_is_suggested_back() {
        let mut sim = Sim::new();
        sim.connect(9600);
        sim.texts(10);
        sim.change(9600, 115_200, true);
        sim.garbage(3);
        let s = sim.suggestion(115_200);
        assert_eq!(s.basis, Basis::History);
        assert_eq!(s.baud, 9600);
        assert!(!s.mode_switch);
    }

    /// Review item 7: what real consoles send at the correct rate.
    #[test]
    fn a_backspace_progress_counter_is_not_garbage() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.text("Downloading firmware image");
        for pct in 0..60 {
            sim.bytes(format!("{pct:3}%\x08\x08\x08\x08").as_bytes());
        }
        let health = sim.health(115_200);
        assert_eq!(health.undecodable_ratio, 0.0, "{health:?}");
        assert_eq!(health.suggestion, None);

        // Even when the window holds nothing but the counter (no line):
        // half its bytes are text, so it isn't mistaken for a NUL stream.
        let mut sim = Sim::new();
        sim.connect(115_200);
        for pct in 0..60 {
            sim.bytes(format!("{pct:3}%\x08\x08\x08\x08").as_bytes());
        }
        assert_eq!(sim.health(115_200).suggestion, None);
    }

    /// Re-review item A: reading at 10x+ the device's rate yields NULs.
    #[test]
    fn an_all_nul_stream_warns_and_suggests_slower_rates() {
        let mut sim = Sim::new();
        sim.connect(1_500_000);
        sim.bytes(&[0u8; 200]);
        let health = sim.health(1_500_000);
        assert_eq!(health.undecodable_ratio, 1.0, "{health:?}");
        let s = health.suggestion.expect("suggestion");
        assert_eq!(s.basis, Basis::Direction);
        assert_eq!(s.baud, 921_600);
        assert!(!s.mode_switch);
        assert!(s.explanation.contains("NUL"), "{}", s.explanation);
    }

    #[test]
    fn a_mostly_nul_stream_with_a_few_stray_bytes_warns_too() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        let mut bytes = vec![0u8; 150];
        for i in (0..150).step_by(10) {
            bytes[i] = if i % 20 == 0 { 0x80 } else { b'x' };
        }
        sim.bytes(&bytes);
        let s = sim.suggestion(115_200);
        assert!(s.baud < 115_200, "suggested {}", s.baud);
    }

    /// The scenario that found it: an unconfirmed trial left the port at
    /// 1500000, the board reset to its 115200 console, and the log filled
    /// with NULs.
    #[test]
    fn nuls_after_a_fast_trial_suggest_the_rate_the_console_was_readable_at() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        for t in ISSUE_TEXT {
            sim.text(t);
        }
        sim.texts(10);
        sim.garbage(5);
        sim.change(115_200, 1_500_000, true);
        sim.garbage(5);
        sim.bytes(&[0u8; 400]);
        let s = sim.suggestion(1_500_000);
        assert_eq!(s.baud, 115_200);
        assert_eq!(s.basis, Basis::History);
    }

    #[test]
    fn nul_padding_after_clean_text_is_not_a_mode_switch() {
        let mut sim = Sim::new();
        sim.connect(115_200);
        sim.texts(20);
        sim.bytes(&[0u8; 40]);
        assert_eq!(sim.health(115_200).suggestion, None);
    }

    #[test]
    fn a_binary_frame_between_text_sentences_is_not_a_mode_switch() {
        // A GPS receiver interleaving NMEA text with a UBX binary message.
        let mut sim = Sim::new();
        sim.connect(9600);
        for i in 0..60 {
            sim.text(&format!(
                "$GPGGA,0921{i:02}.00,2503.71,N,12129.07,E,1,08,0.9,45.0,M,15.2,M,,*4{}",
                i % 10
            ));
        }
        let mut ubx = vec![0xB5, 0x62, 0x01, 0x07, 0x5C, 0x00];
        ubx.extend((0..40u8).map(|i| 0x80 | i.wrapping_mul(37)));
        sim.bytes(&ubx);
        let health = sim.health(9600);
        assert!(
            health.undecodable_ratio < UNDECODABLE_THRESHOLD,
            "{health:?}"
        );
        assert_eq!(health.suggestion, None);
    }

    /// SLIP frame: END, payload with every 0xC0/0xDB byte escaped, END.
    fn slip_frame(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0xC0];
        for &b in payload {
            match b {
                0xC0 => out.extend([0xDB, 0xDC]),
                0xDB => out.extend([0xDB, 0xDD]),
                b => out.push(b),
            }
        }
        out.push(0xC0);
        out
    }

    /// SLIP traffic with regular frame lengths is overwhelmingly non-text,
    /// and still gets no suggestion.
    #[test]
    fn slip_framed_binary_traffic_gets_no_suggestion() {
        let mut stream = vec![0x01, 0x8f, 0x92]; // tail of a frame cut by the window
        for i in 0..20u8 {
            let len = 18 + (i % 5) as usize; // 18..=22 bytes, regular
            let payload: Vec<u8> = (0..len)
                .map(|j| {
                    0x80u8
                        .wrapping_add(i.wrapping_mul(7))
                        .wrapping_add((j as u8).wrapping_mul(13))
                })
                .chain([0xC0, 0xDB]) // force escapes into every frame
                .collect();
            stream.extend(slip_frame(&payload));
        }
        stream.extend([0xC0, 0x90, 0x81]); // start of a frame cut by the window
        assert!(looks_like_slip(&stream));
        let mut sim = Sim::new();
        sim.bytes(&stream);
        let health = sim.health(115_200);
        assert!(health.undecodable_ratio >= UNDECODABLE_THRESHOLD);
        assert_eq!(health.binary_protocol, Some("slip"));
        assert_eq!(health.suggestion, None);
        assert_eq!(health.suggested_baud, None);
    }

    #[test]
    fn slip_detection_rejects_irregular_frames_bad_escapes_and_tiny_samples() {
        let mut irregular = Vec::new();
        for len in [4usize, 60, 9, 30] {
            irregular.extend(slip_frame(&vec![0x90; len]));
        }
        assert!(!looks_like_slip(&irregular));
        let mut bad_escape = Vec::new();
        for _ in 0..5 {
            bad_escape.extend([0xC0, 0x90, 0xDB, 0x41, 0x91, 0x92, 0xC0]);
        }
        assert!(!looks_like_slip(&bad_escape));
        assert!(!looks_like_slip(&slip_frame(&[0x90; 10])));
        let mut unframed = Vec::new();
        for _ in 0..4 {
            unframed.extend(slip_frame(&[0x90; 10]));
        }
        unframed.extend(ISSUE_SAMPLE.repeat(4));
        assert!(!looks_like_slip(&unframed));
        // Three one-byte frames are framing-shaped but far too little to
        // call a protocol.
        assert!(!looks_like_slip(&[
            0xC0, 0x90, 0xC0, 0x91, 0xC0, 0x92, 0xC0
        ]));
    }

    #[test]
    fn every_fingerprint_entry_is_complete() {
        for f in FINGERPRINTS {
            assert!(!f.pattern.is_empty());
            assert!(f.baud > 0);
            assert!(f.source_url.starts_with("https://"), "{}", f.pattern);
            assert!(f.also_see.iter().all(|u| u.starts_with("https://")));
            assert!(!f.reason.is_empty() && !f.reason.ends_with('.'));
            assert!(!f.platform.is_empty());
        }
    }

    #[test]
    fn port_state_follows_connect_disconnect_and_lease_events() {
        let mut sim = Sim::new();
        assert_eq!(sim.health(9600).port, PortState::Unknown);
        sim.connect(9600);
        let h = sim.health(9600);
        assert_eq!((h.port, h.connects), (PortState::Open, 1));
        sim.tracker.on_event(90, "lease_start", &Map::new());
        assert_eq!(sim.health(9600).port, PortState::Leased);
        sim.tracker.on_event(91, "lease_end", &Map::new());
        sim.tracker.on_event(92, "disconnect", &Map::new());
        assert_eq!(sim.health(9600).port, PortState::Disconnected);
        sim.connect(9600);
        let h = sim.health(9600);
        assert_eq!((h.port, h.connects), (PortState::Open, 2));
    }

    #[test]
    fn the_recent_window_stays_bounded_with_a_running_total() {
        let mut tracker = EvidenceTracker::default();
        for i in 0..100 {
            tracker.on_rx(i, 0.0, "", &[0x41; 1000]);
        }
        let total: usize = tracker.recent.iter().map(|c| c.bytes.len()).sum();
        assert_eq!(total, tracker.recent_bytes);
        assert!(
            (RECENT_RX_BYTES..RECENT_RX_BYTES + 1000).contains(&total),
            "{total}"
        );
    }

    #[test]
    fn config_change_events_with_bare_numeric_values_are_understood() {
        let mut tracker = EvidenceTracker::default();
        tracker.on_event(
            5,
            "config_change",
            &extra(&[
                ("field", "baud".into()),
                ("old", 9600.into()),
                ("new", 115_200.into()),
            ]),
        );
        assert_eq!(tracker.segments.len(), 2);
        assert_eq!(tracker.segments[0].baud, Some(9600));
        assert_eq!(tracker.segments[1].baud, Some(115_200));
        assert_eq!(tracker.segments[1].start_seq, Some(5));
    }
}
