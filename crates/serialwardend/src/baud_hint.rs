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
//! 3. nothing at all decoded any more, which is what a stream sent *faster*
//!    than it is read looks like: bits are lost during sampling, so no
//!    re-framing of the captured bytes recovers text.
//!
//! [`infer`] uses that evidence, in this priority order:
//!
//! 1. **History** — which rates this device has produced readable text at.
//!    If the current rate was readable before and now nothing is, the
//!    device appears to have switched modes, and the suggestion goes up
//!    (mode switches into a bootloader/download protocol almost always
//!    speed up), never back to that rate or below it.
//! 2. **Fingerprint** — a chip/ROM string from [`FINGERPRINTS`] seen in the
//!    recorded text maps to that platform's documented rate, carried with
//!    its source URL so the GUI can show where the number comes from.
//! 3. **Direction** — with no history or fingerprint and no text at all in
//!    the sample, the next common rate *above* the current one.
//! 4. **Common** — otherwise, the next common rate, said to be exactly
//!    that: a next thing to try, not a reading off these bytes.
//!
//! Every suggestion carries its [`Basis`] and a plain-language
//! `explanation`, so the GUI states what the guess rests on.
//!
//! # What this does not do
//!
//! It never derives a rate from the garbled bytes themselves. When the real
//! rate is *lower* than the read rate an offline re-framing could in
//! principle recover it, but when it is higher (the case above) the bits
//! are gone. The GUI's "try" button closes that gap empirically instead:
//! it applies the rate, re-measures, and reverts if nothing improved.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::query::{AssembledLine, OobRecord, RecentRxChunk};

/// One known chip/ROM banner and the rate its platform documents for the
/// mode that banner implies. To add one: a literal `pattern` that appears
/// in that platform's own log text, the `baud`, a `source_url` that states
/// the number, and a short `reason` saying what the number is. No entry
/// goes in without a source anyone can check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    /// Case-sensitive substring searched for in recorded text lines.
    pub pattern: &'static str,
    /// Human-readable platform name, shown next to the suggestion.
    pub platform: &'static str,
    pub baud: u32,
    pub source_url: &'static str,
    /// What `baud` is, in a sentence fragment ("… at 1500000 baud by
    /// default"), shown verbatim in the GUI.
    pub reason: &'static str,
}

/// The fingerprint table. See [`Fingerprint`] for how to extend it.
pub const FINGERPRINTS: &[Fingerprint] = &[
    Fingerprint {
        pattern: "RTL8735B",
        platform: "Realtek RTL8735B (AmebaPro2)",
        baud: 1_500_000,
        source_url: "https://aiot.realmcu.com/en/latest/tools/image_tool/index.html",
        reason: "Realtek's Image Tool downloads firmware at 1500000 baud by default",
    },
    Fingerprint {
        pattern: "ets Jan",
        platform: "Espressif ESP8266 boot ROM",
        baud: 74_880,
        source_url: "https://docs.espressif.com/projects/esptool/en/latest/esp8266/advanced-topics/boot-mode-selection.html",
        reason: "the ESP8266 boot ROM prints its boot log at 74880 baud",
    },
];

/// Below this many sampled bytes no suggestion is made — a handful of
/// bytes is too small for "most of this is undecodable" to mean anything.
pub const MIN_SAMPLE_BYTES: usize = 32;

/// Fraction of non-text bytes at/above which a suggestion is made. Any
/// real baud mismatch corrupts most bytes almost immediately, while a few
/// stray binary bytes in otherwise clean text stay well below this.
pub const UNDECODABLE_THRESHOLD: f64 = 0.2;

/// The bytes after the sample's last readable line count as "no text now"
/// when there are at least [`MIN_SAMPLE_BYTES`] of them and at least this
/// fraction is non-text. Higher than [`UNDECODABLE_THRESHOLD`] because it
/// claims more: not "some garbage", but "the device stopped sending text".
const NO_TEXT_NOW_RATIO: f64 = 0.5;

/// How far back (in raw line bytes, newest first) the history scan walks.
/// Bounded so a long-lived device's ever-growing in-memory history can't
/// make every `GET .../config` slower; a few MB covers many boots of a
/// typical debug console.
pub const HISTORY_MAX_BYTES: usize = 4 * 1024 * 1024;

/// A line must have at least this many characters to count as readable
/// text — short enough for real log lines, long enough that a few bytes of
/// garbage that happen to be printable don't qualify.
const READABLE_LINE_MIN_CHARS: usize = 8;

/// This many readable lines at a rate is what "this device produced
/// readable text at that rate" means.
const READABLE_LINES_FOR_HISTORY: usize = 5;

/// Standard rates, ascending — the "next rate up" list.
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
    pub reason: &'static str,
}

/// A suggested rate and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BaudSuggestion {
    pub baud: u32,
    pub basis: Basis,
    /// The current rate produced readable text earlier and the recent
    /// sample has none at all — "the device appears to have switched
    /// modes".
    pub mode_switch: bool,
    /// Rates this device has produced readable text at, within the
    /// scanned history, ascending.
    pub readable_bauds: Vec<u32>,
    pub fingerprint: Option<FingerprintHit>,
    /// One or two plain sentences stating the basis, for display.
    pub explanation: String,
}

/// `GET /api/devices/:id/config`'s `decode_health` field.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct DecodeHealth {
    /// Raw bytes sampled: the newest recorded `rx` bytes since the baud
    /// rate last changed (at most [`crate::query::RECENT_RX_BYTES`]).
    pub checked_bytes: usize,
    /// Fraction of `checked_bytes` that isn't text: bytes of invalid UTF-8
    /// plus control characters other than tab, CR, LF and ESC. `0.0` when
    /// nothing was sampled.
    pub undecodable_ratio: f64,
    /// Readable text lines in the sample (CR/LF-separated).
    pub text_lines: usize,
    /// `Some("slip")` when the sample is structured binary protocol traffic
    /// — the suggestion is then withheld, since undecodable bytes are what
    /// that protocol is supposed to look like.
    pub binary_protocol: Option<&'static str>,
    /// `t_wall` of the newest sampled chunk — lets the GUI tell a live
    /// garbled stream from a stale one even when no complete line arrived.
    pub newest_sample_t_wall: Option<String>,
    /// `suggestion.baud`, kept as its own field for existing clients.
    pub suggested_baud: Option<u32>,
    pub suggestion: Option<BaudSuggestion>,
}

/// Baud rate in effect over the recorded stream, rebuilt from
/// `config_change` events.
struct BaudTimeline {
    /// `(seq, old, new)` for every event that actually changed the rate.
    changes: Vec<(u64, Option<u32>, u32)>,
    current: u32,
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

impl BaudTimeline {
    fn from_events(events: &[OobRecord], current: u32) -> Self {
        let mut changes: Vec<(u64, Option<u32>, u32)> = events
            .iter()
            .filter(|e| e.name.as_deref() == Some("config_change"))
            .filter_map(|e| {
                let new = baud_of(e.extra.get("new"))?;
                let old = baud_of(e.extra.get("old"));
                (old != Some(new)).then_some((e.seq, old, new))
            })
            .collect();
        changes.sort_by_key(|c| c.0);
        Self { changes, current }
    }

    /// Index of the segment `seq` falls in: `0` before the first change,
    /// `i` after the `i`th.
    fn segment_of(&self, seq: u64) -> usize {
        self.changes.partition_point(|c| c.0 < seq)
    }

    fn baud_of_segment(&self, segment: usize) -> Option<u32> {
        if self.changes.is_empty() {
            return Some(self.current);
        }
        if segment == 0 {
            self.changes[0].1
        } else {
            Some(self.changes[segment - 1].2)
        }
    }

    fn last_change_seq(&self) -> Option<u64> {
        self.changes.last().map(|c| c.0)
    }
}

fn is_text_char(c: char) -> bool {
    !c.is_control() || matches!(c, '\t' | '\n' | '\r' | '\x1b')
}

/// Bytes of `bytes` that aren't text: every byte of an invalid UTF-8
/// sequence (a truncated sequence at the very end counts — nothing more is
/// coming in a point-in-time sample) plus every control character other
/// than tab, CR, LF and ESC (ANSI colour). Control characters count because
/// a mismatched baud produces plenty of them (`0x04`, `0x08`, …) and they
/// are valid UTF-8, so a UTF-8-only measure understated real garbage.
pub fn count_non_text_bytes(bytes: &[u8]) -> usize {
    let mut non_text = 0usize;
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
        non_text += valid
            .chars()
            .filter(|&c| !is_text_char(c))
            .map(char::len_utf8)
            .sum::<usize>();
        non_text += bad_len;
        rest = &rest[valid.len() + bad_len..];
    }
    non_text
}

/// Whether one line (terminator already stripped) is readable text: valid
/// UTF-8, at least [`READABLE_LINE_MIN_CHARS`] characters once trimmed,
/// and no control characters other than tab and ESC.
pub fn is_readable_line(raw: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw) else {
        return false;
    };
    let text = text.trim();
    text.chars().count() >= READABLE_LINE_MIN_CHARS
        && text
            .chars()
            .all(|c| is_text_char(c) && !matches!(c, '\n' | '\r'))
}

/// Whether `bytes` is SLIP-framed traffic (RFC 1055) — the structured
/// binary protocol that must not trigger a baud suggestion. All of:
///
/// - at least 3 complete, non-empty frames between `0xC0` delimiters
///   (back-to-back `0xC0`s are empty frames and ignored, as SLIP allows);
/// - every `0xDB` (ESC) inside a frame is followed by `0xDC` or `0xDD` —
///   any other escape is invalid SLIP;
/// - regular frame lengths: every complete frame is within a factor of two
///   of the median frame length;
/// - the partial frames before the first and after the last delimiter (the
///   sample window cuts frames) are no longer than the longest complete
///   frame, so the framing covers the whole sample.
///
/// A baud mismatch produces `0xC0` about once every 256 bytes with
/// geometrically distributed gaps and invalid escapes, which fails the
/// regularity and escape checks; a real SLIP link passes all four.
pub fn looks_like_slip(bytes: &[u8]) -> bool {
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

/// Per-rate readability over the scanned history.
#[derive(Default)]
struct History {
    /// Readable lines per rate.
    readable_lines: BTreeMap<u32, usize>,
    /// Rate → seq of its newest readable line.
    last_readable_seq: BTreeMap<u32, u64>,
    /// Rates whose most recent segment had enough line bytes, was mostly
    /// non-text and held no readable line — tried, didn't decode.
    known_bad: BTreeSet<u32>,
    fingerprint: Option<&'static Fingerprint>,
}

#[derive(Default)]
struct SegmentStats {
    bytes: usize,
    non_text: usize,
    readable: usize,
}

fn scan_history(lines: &[AssembledLine], timeline: &BaudTimeline, current: u32) -> History {
    let mut history = History::default();
    let mut segments: BTreeMap<usize, SegmentStats> = BTreeMap::new();
    let mut scanned = 0usize;
    for line in lines.iter().rev() {
        if scanned >= HISTORY_MAX_BYTES {
            break;
        }
        scanned += line.raw.len();
        let segment = timeline.segment_of(line.seq);
        let readable = is_readable_line(&line.raw);
        let stats = segments.entry(segment).or_default();
        stats.bytes += line.raw.len();
        stats.non_text += count_non_text_bytes(&line.raw);
        if readable {
            stats.readable += 1;
        }
        // Matched against the lossy text, not only fully valid lines: a
        // banner often lands in the same line as garbage left in the
        // partial buffer by a mode switch, and an ASCII pattern survives
        // lossy decoding intact (random bytes forming it are negligible).
        if history.fingerprint.is_none() {
            history.fingerprint = FINGERPRINTS.iter().find(|f| line.text.contains(f.pattern));
        }
        let Some(baud) = timeline.baud_of_segment(segment) else {
            continue;
        };
        if readable {
            *history.readable_lines.entry(baud).or_default() += 1;
            history.last_readable_seq.entry(baud).or_insert(line.seq);
        }
    }
    // Newest segment per rate decides whether that rate is known bad.
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    for (segment, stats) in segments.iter().rev() {
        let Some(baud) = timeline.baud_of_segment(*segment) else {
            continue;
        };
        if !seen.insert(baud) || baud == current {
            continue;
        }
        let ratio = stats.non_text as f64 / stats.bytes.max(1) as f64;
        if stats.bytes >= MIN_SAMPLE_BYTES && ratio >= UNDECODABLE_THRESHOLD && stats.readable == 0
        {
            history.known_bad.insert(baud);
        }
    }
    history
}

/// Decode health of the recent sample plus, if warranted, a suggested rate
/// and its basis — see the module docs for the rules.
///
/// - `current`: the port's configured rate now.
/// - `recent`: the newest raw `rx` chunks
///   ([`crate::query::DeviceQueryState::recent_rx`]); only those recorded
///   after the last rate change are sampled.
/// - `lines`: assembled history, oldest first (scanned newest first, up to
///   [`HISTORY_MAX_BYTES`]).
/// - `events`: the device's `config_change` events (other events are
///   ignored), used to attribute each line to the rate it was read at.
pub fn infer(
    current: u32,
    recent: &[RecentRxChunk],
    lines: &[AssembledLine],
    events: &[OobRecord],
) -> DecodeHealth {
    let timeline = BaudTimeline::from_events(events, current);
    let since = timeline.last_change_seq();
    let chunks: Vec<&RecentRxChunk> = recent
        .iter()
        .filter(|c| since.is_none_or(|s| c.seq > s))
        .collect();
    let sample: Vec<u8> = chunks
        .iter()
        .flat_map(|c| c.bytes.iter().copied())
        .collect();
    let checked_bytes = sample.len();
    let undecodable_ratio = if checked_bytes == 0 {
        0.0
    } else {
        count_non_text_bytes(&sample) as f64 / checked_bytes as f64
    };
    let (text_lines, last_text_end) = text_profile(&sample);
    // What arrived after the last readable line: "now". A device that
    // printed its boot log and then switched into a binary protocol has
    // both in the sample; only this tail says what it is doing *now*.
    let trailing = &sample[last_text_end..];
    let no_text_now = trailing.len() >= MIN_SAMPLE_BYTES
        && count_non_text_bytes(trailing) as f64 / trailing.len() as f64 >= NO_TEXT_NOW_RATIO;
    let mut health = DecodeHealth {
        checked_bytes,
        undecodable_ratio,
        text_lines,
        newest_sample_t_wall: chunks.last().map(|c| c.t_wall.clone()),
        ..DecodeHealth::default()
    };
    let garbled = checked_bytes >= MIN_SAMPLE_BYTES && undecodable_ratio >= UNDECODABLE_THRESHOLD;
    if !garbled && !no_text_now {
        return health;
    }
    if looks_like_slip(&sample) || looks_like_slip(trailing) {
        health.binary_protocol = Some("slip");
        return health;
    }
    let history = scan_history(lines, &timeline, current);
    health.suggestion = suggest(current, no_text_now, &history);
    health.suggested_baud = health.suggestion.as_ref().map(|s| s.baud);
    health
}

fn next_rate_up(current: u32, excluded: impl Fn(u32) -> bool) -> Option<u32> {
    RATES_ASCENDING
        .iter()
        .copied()
        .find(|&b| b > current && !excluded(b))
}

fn suggest(current: u32, no_text_now: bool, history: &History) -> Option<BaudSuggestion> {
    let excluded = |b: u32| b == current || history.known_bad.contains(&b);
    let readable_bauds: Vec<u32> = history
        .readable_lines
        .iter()
        .filter(|(_, &n)| n >= READABLE_LINES_FOR_HISTORY)
        .map(|(&b, _)| b)
        .collect();
    // Readable lines at `current` in the history can only predate the
    // trailing non-text run, so "readable here before, nothing now" is a
    // mode switch rather than a wrong setting.
    let mode_switch = no_text_now
        && history.readable_lines.get(&current).copied().unwrap_or(0) >= READABLE_LINES_FOR_HISTORY;
    let fingerprint = history.fingerprint.filter(|f| !excluded(f.baud));
    let hit = |f: &Fingerprint| FingerprintHit {
        pattern: f.pattern,
        platform: f.platform,
        source_url: f.source_url,
        reason: f.reason,
    };
    let make =
        |baud: u32, basis: Basis, fp: Option<&Fingerprint>, explanation: String| BaudSuggestion {
            baud,
            basis,
            mode_switch,
            readable_bauds: readable_bauds.clone(),
            fingerprint: fp.map(hit),
            explanation,
        };
    let fingerprint_sentence = |f: &Fingerprint| {
        format!(
            "The log earlier printed \"{}\" ({}); {}.",
            f.pattern, f.platform, f.reason
        )
    };

    if mode_switch {
        let switched = format!(
            "It printed readable text at {current} earlier and none now, so the device appears \
             to have switched modes."
        );
        // A mode switch (into a bootloader or download protocol) almost
        // always speeds up, and a rate faster than the read rate is
        // exactly what decodes to nothing — so only rates above `current`.
        if let Some(f) = fingerprint.filter(|f| f.baud > current) {
            return Some(make(
                f.baud,
                Basis::Fingerprint,
                Some(f),
                format!("{switched} {}", fingerprint_sentence(f)),
            ));
        }
        if let Some(&b) = readable_bauds
            .iter()
            .find(|&&b| b > current && !excluded(b))
        {
            return Some(make(
                b,
                Basis::History,
                None,
                format!("{switched} It also printed readable text at {b} before."),
            ));
        }
        let b = next_rate_up(current, excluded)?;
        return Some(make(
            b,
            Basis::History,
            None,
            format!(
                "{switched} Such switches usually go faster, so {b} is the next common rate up — \
                 a direction, not a reading off these bytes."
            ),
        ));
    }

    if let Some(b) = readable_bauds
        .iter()
        .copied()
        .filter(|&b| !excluded(b))
        .max_by_key(|b| history.last_readable_seq.get(b).copied().unwrap_or(0))
    {
        return Some(make(
            b,
            Basis::History,
            None,
            format!("This device printed readable text at {b} earlier in this log."),
        ));
    }
    if let Some(f) = fingerprint {
        return Some(make(
            f.baud,
            Basis::Fingerprint,
            Some(f),
            fingerprint_sentence(f),
        ));
    }
    if no_text_now {
        if let Some(b) = next_rate_up(current, excluded) {
            return Some(make(
                b,
                Basis::Direction,
                None,
                format!(
                    "Nothing arriving now is text. When the real rate is higher than the one being read, \
                     bits are lost and nothing decodes, so faster rates come first: {b} is the \
                     next common rate up — a direction, not a reading off these bytes."
                ),
            ));
        }
    }
    let b = FALLBACK_ORDER.iter().copied().find(|&b| !excluded(b))?;
    Some(make(
        b,
        Basis::Common,
        None,
        format!("{b} is just the next common rate to try, not a reading off these bytes."),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use warden_proto::Kind;

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

    fn line(seq: u64, raw: &[u8]) -> AssembledLine {
        AssembledLine {
            raw: raw.to_vec(),
            text: String::from_utf8_lossy(raw).into_owned(),
            seq,
            t_mono: 0.0,
            t_wall: "2026-10-06T00:00:00Z".into(),
            capped: false,
        }
    }

    fn chunk(seq: u64, bytes: &[u8]) -> RecentRxChunk {
        RecentRxChunk {
            seq,
            t_wall: format!("2026-10-06T00:00:{:02}Z", seq % 60),
            bytes: bytes.to_vec(),
        }
    }

    fn baud_change(seq: u64, old: u32, new: u32) -> OobRecord {
        let mut extra = serde_json::Map::new();
        extra.insert("old".into(), json!({ "baud": old, "data_bits": "eight" }));
        extra.insert("new".into(), json!({ "baud": new, "data_bits": "eight" }));
        extra.insert("changed_by".into(), "gui".into());
        OobRecord {
            seq,
            t_mono: 0.0,
            t_wall: String::new(),
            kind: Kind::Event,
            name: Some("config_change".into()),
            extra,
        }
    }

    /// `n` readable boot-log lines without a chip name, from `seq` on.
    fn plain_text_lines(seq: u64, n: usize) -> Vec<AssembledLine> {
        (0..n)
            .map(|i| {
                line(
                    seq + i as u64,
                    format!("[app] heartbeat tick {i}").as_bytes(),
                )
            })
            .collect()
    }

    fn garbled_chunks(seq: u64, n: usize) -> Vec<RecentRxChunk> {
        (0..n)
            .map(|i| chunk(seq + i as u64, ISSUE_SAMPLE))
            .collect()
    }

    #[test]
    fn non_text_counts_invalid_utf8_and_controls_but_not_ansi_or_whitespace() {
        assert_eq!(
            count_non_text_bytes(b"hello\tworld\r\n\x1b[31mred\x1b[0m"),
            0
        );
        assert_eq!(count_non_text_bytes(&[0x80]), 1);
        assert_eq!(count_non_text_bytes(&[b'a', 0x08, b'b', 0x04]), 2);
        // A truncated multi-byte sequence at the very end is undecodable.
        assert_eq!(count_non_text_bytes(&[b'a', 0xe6, 0x97]), 2);
        // Valid non-ASCII text is text.
        assert_eq!(count_non_text_bytes("溫度 25°C".as_bytes()), 0);
    }

    #[test]
    fn the_issue_sample_is_overwhelmingly_non_text_and_not_slip() {
        let ratio = count_non_text_bytes(ISSUE_SAMPLE) as f64 / ISSUE_SAMPLE.len() as f64;
        assert!(ratio > 0.8, "ratio {ratio}");
        assert!(!looks_like_slip(ISSUE_SAMPLE));
        assert!(!looks_like_slip(&ISSUE_SAMPLE.repeat(40)));
    }

    #[test]
    fn clean_text_gets_no_suggestion() {
        let recent = [chunk(
            1,
            b"boot ok\r\nsensor ready, 25.3C\r\n".repeat(4).as_slice(),
        )];
        let health = infer(115_200, &recent, &[], &[]);
        assert_eq!(health.undecodable_ratio, 0.0);
        assert!(health.text_lines >= 4);
        assert_eq!(health.suggestion, None);
        assert_eq!(health.suggested_baud, None);
    }

    #[test]
    fn a_sample_below_the_minimum_size_gets_no_suggestion() {
        let recent = [chunk(1, &[0x80; MIN_SAMPLE_BYTES - 1])];
        let health = infer(115_200, &recent, &[], &[]);
        assert_eq!(health.undecodable_ratio, 1.0);
        assert_eq!(health.suggestion, None);
    }

    #[test]
    fn nothing_sampled_reports_zero_and_no_suggestion() {
        let health = infer(115_200, &[], &[], &[]);
        assert_eq!(health.checked_bytes, 0);
        assert_eq!(health.undecodable_ratio, 0.0);
        assert_eq!(health.newest_sample_t_wall, None);
        assert_eq!(health.suggestion, None);
    }

    /// Acceptance criterion 1: readable at 115200 earlier, now all binary —
    /// the suggestion goes up, never to 115200 or below, and says why.
    #[test]
    fn history_readable_at_the_current_rate_then_all_binary_suggests_upward() {
        let lines = plain_text_lines(1, 20);
        let recent = garbled_chunks(100, 10);
        let health = infer(115_200, &recent, &lines, &[]);
        let s = health.suggestion.expect("suggestion");
        assert_eq!(s.basis, Basis::History);
        assert!(s.mode_switch);
        assert!(s.baud > 115_200, "suggested {}", s.baud);
        assert_eq!(s.baud, 230_400);
        assert_eq!(s.readable_bauds, vec![115_200]);
        assert!(
            s.explanation.contains("switched modes"),
            "{}",
            s.explanation
        );
        assert_eq!(health.suggested_baud, Some(s.baud));
        assert_eq!(health.text_lines, 0);
    }

    #[test]
    fn a_mode_switch_prefers_a_faster_rate_the_device_was_readable_at() {
        // Readable at 921600 first, then moved to 115200 and readable
        // there, now garbled at 115200.
        let mut lines = plain_text_lines(1, 10);
        lines.extend(plain_text_lines(20, 10));
        let events = [
            baud_change(0, 115_200, 921_600),
            baud_change(15, 921_600, 115_200),
        ];
        let recent = garbled_chunks(100, 10);
        let s = infer(115_200, &recent, &lines, &events)
            .suggestion
            .expect("suggestion");
        assert_eq!(s.basis, Basis::History);
        assert!(s.mode_switch);
        assert_eq!(s.baud, 921_600);
        assert_eq!(s.readable_bauds, vec![115_200, 921_600]);
    }

    /// Acceptance criterion 2, with the issue's own data: the RTL8735B
    /// banner was recorded, then the download-mode bytes — 1500000, with the
    /// source carried through.
    #[test]
    fn the_rtl8735b_fingerprint_suggests_1500000_with_its_source() {
        let mut lines: Vec<AssembledLine> = ISSUE_TEXT
            .iter()
            .enumerate()
            .map(|(i, t)| line(i as u64 + 1, t.as_bytes()))
            .collect();
        lines.extend(plain_text_lines(10, 10));
        let recent = garbled_chunks(100, 10);
        let health = infer(115_200, &recent, &lines, &[]);
        let s = health.suggestion.expect("suggestion");
        assert_eq!(s.baud, 1_500_000);
        assert_eq!(s.basis, Basis::Fingerprint);
        assert!(s.mode_switch);
        let fp = s.fingerprint.expect("fingerprint carried through");
        assert_eq!(fp.pattern, "RTL8735B");
        assert_eq!(
            fp.source_url,
            "https://aiot.realmcu.com/en/latest/tools/image_tool/index.html"
        );
        assert!(s.explanation.contains("RTL8735B"), "{}", s.explanation);
        assert!(
            s.explanation.contains("switched modes"),
            "{}",
            s.explanation
        );
    }

    #[test]
    fn a_fingerprint_without_mode_switch_history_still_suggests_its_rate() {
        // One banner line is not enough history to call a mode switch.
        let lines = [line(1, ISSUE_TEXT[0].as_bytes())];
        let s = infer(115_200, &garbled_chunks(100, 3), &lines, &[])
            .suggestion
            .expect("suggestion");
        assert_eq!(s.basis, Basis::Fingerprint);
        assert!(!s.mode_switch);
        assert_eq!(s.baud, 1_500_000);
    }

    #[test]
    fn a_fingerprint_is_found_in_a_line_that_also_holds_garbage() {
        let mut raw = ISSUE_SAMPLE.to_vec();
        raw.extend(ISSUE_TEXT[0].as_bytes());
        let s = infer(9600, &garbled_chunks(100, 3), &[line(1, &raw)], &[])
            .suggestion
            .expect("suggestion");
        assert_eq!(s.basis, Basis::Fingerprint);
        assert_eq!(s.baud, 1_500_000);
    }

    #[test]
    fn a_mode_switch_never_follows_a_fingerprint_downward() {
        // ESP8266's 74880 is below 115200: with mode-switch evidence the
        // suggestion must still go up.
        let mut lines = vec![line(1, b"ets Jan  8 2013,rst cause:2, boot mode:(3,6)")];
        lines.extend(plain_text_lines(2, 10));
        let s = infer(115_200, &garbled_chunks(100, 3), &lines, &[])
            .suggestion
            .expect("suggestion");
        assert!(s.baud > 115_200, "suggested {}", s.baud);
        assert_eq!(s.basis, Basis::History);
    }

    /// Acceptance criterion 3: all binary, zero text lines, no history at
    /// all — a suggestion still appears, and it goes up.
    #[test]
    fn all_binary_with_no_history_suggests_the_next_rate_up() {
        let health = infer(115_200, &[chunk(1, ISSUE_SAMPLE)], &[], &[]);
        assert_eq!(health.text_lines, 0);
        let s = health.suggestion.expect("suggestion");
        assert_eq!(s.basis, Basis::Direction);
        assert_eq!(s.baud, 230_400);
        assert!(!s.mode_switch);
    }

    #[test]
    fn mixed_garble_with_some_text_falls_back_to_the_common_list() {
        // Garbage interleaved with text that keeps arriving: not "no text
        // now", no history, no fingerprint.
        let mut bytes = b"[boot] starting services\r\n".to_vec();
        bytes.extend(ISSUE_SAMPLE.repeat(3));
        bytes.extend(b"\r\n[boot] services up and running\r\n");
        let health = infer(9600, &[chunk(1, &bytes)], &[], &[]);
        let s = health.suggestion.expect("suggestion");
        assert_eq!(s.basis, Basis::Common);
        assert_eq!(s.baud, 115_200);
        assert!(s.explanation.contains("not a reading off these bytes"));
    }

    #[test]
    fn only_bytes_since_the_last_rate_change_are_sampled() {
        // Garbage at 115200, then a switch to 1500000 with clean text: the
        // garbage belongs to the old rate and must not count.
        let mut recent = garbled_chunks(1, 5);
        let clean = b"nor download success, rebooting now\r\n";
        recent.push(chunk(20, clean));
        let events = [baud_change(10, 115_200, 1_500_000)];
        let health = infer(1_500_000, &recent, &[], &events);
        assert_eq!(health.undecodable_ratio, 0.0);
        assert_eq!(health.checked_bytes, clean.len());
        assert_eq!(health.suggestion, None);
    }

    #[test]
    fn a_rate_already_tried_and_garbled_is_not_suggested_again() {
        // Readable at 115200; tried 230400 (garbage, no readable lines);
        // back at 115200 and still garbled → skip 230400.
        let mut lines = plain_text_lines(1, 10);
        lines.extend((0..5).map(|i| line(30 + i, &ISSUE_SAMPLE.repeat(2))));
        let events = [
            baud_change(20, 115_200, 230_400),
            baud_change(40, 230_400, 115_200),
        ];
        let s = infer(115_200, &garbled_chunks(100, 5), &lines, &events)
            .suggestion
            .expect("suggestion");
        assert_eq!(s.baud, 460_800);
    }

    #[test]
    fn readable_history_at_another_rate_is_suggested_back() {
        // Readable at 9600, then someone switched to 115200: garbage.
        let lines = plain_text_lines(1, 10);
        let events = [baud_change(50, 9600, 115_200)];
        let mut bytes = b"x [ok]\r\n".to_vec();
        bytes.extend(ISSUE_SAMPLE.repeat(3));
        let s = infer(115_200, &[chunk(60, &bytes)], &lines, &events)
            .suggestion
            .expect("suggestion");
        assert_eq!(s.basis, Basis::History);
        assert_eq!(s.baud, 9600);
        assert!(!s.mode_switch);
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

    /// Acceptance criterion 4: SLIP traffic with regular frame lengths is
    /// overwhelmingly non-text, and still gets no suggestion.
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
        let health = infer(115_200, &[chunk(1, &stream)], &[], &[]);
        assert!(health.undecodable_ratio >= UNDECODABLE_THRESHOLD);
        assert_eq!(health.binary_protocol, Some("slip"));
        assert_eq!(health.suggestion, None);
        assert_eq!(health.suggested_baud, None);
    }

    #[test]
    fn slip_detection_rejects_irregular_frames_and_bad_escapes() {
        // Irregular lengths: 4, 60, 9 bytes.
        let mut irregular = Vec::new();
        for len in [4usize, 60, 9, 30] {
            irregular.extend(slip_frame(&vec![0x90; len]));
        }
        assert!(!looks_like_slip(&irregular));
        // Regular lengths but an invalid escape (0xDB 0x41).
        let mut bad_escape = Vec::new();
        for _ in 0..5 {
            bad_escape.extend([0xC0, 0x90, 0xDB, 0x41, 0x91, 0x92, 0xC0]);
        }
        assert!(!looks_like_slip(&bad_escape));
        // Too few frames.
        assert!(!looks_like_slip(&slip_frame(&[0x90; 10])));
        // Long unframed garbage after the last delimiter.
        let mut unframed = Vec::new();
        for _ in 0..4 {
            unframed.extend(slip_frame(&[0x90; 10]));
        }
        unframed.extend(ISSUE_SAMPLE.repeat(4));
        assert!(!looks_like_slip(&unframed));
    }

    #[test]
    fn every_fingerprint_entry_is_complete() {
        for f in FINGERPRINTS {
            assert!(!f.pattern.is_empty());
            assert!(f.baud > 0);
            assert!(f.source_url.starts_with("https://"), "{}", f.pattern);
            assert!(!f.reason.is_empty());
            assert!(!f.platform.is_empty());
        }
    }

    #[test]
    fn config_change_events_with_bare_numeric_values_are_understood() {
        let mut extra = serde_json::Map::new();
        extra.insert("field".into(), "baud".into());
        extra.insert("old".into(), 9600.into());
        extra.insert("new".into(), 115_200.into());
        let ev = OobRecord {
            seq: 5,
            t_mono: 0.0,
            t_wall: String::new(),
            kind: Kind::Event,
            name: Some("config_change".into()),
            extra,
        };
        let timeline = BaudTimeline::from_events(&[ev], 115_200);
        assert_eq!(timeline.last_change_seq(), Some(5));
        assert_eq!(timeline.baud_of_segment(timeline.segment_of(1)), Some(9600));
        assert_eq!(
            timeline.baud_of_segment(timeline.segment_of(6)),
            Some(115_200)
        );
    }
}
