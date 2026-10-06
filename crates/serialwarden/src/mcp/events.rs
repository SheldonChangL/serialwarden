//! Per-device out-of-band event watermarks.
//!
//! Every one of this bridge's five read tools must carry any out-of-band
//! events (disconnect, lease activity, config change) that happened on the
//! relevant device since the agent last looked — not just `tail`/
//! `read_since`, whose own daemon replies already carry an `events` array
//! for free (see `serialwardend::query`'s module docs: events are never
//! dropped by a filter, only ever range-bounded). `get_config`, `wait_for`,
//! and `list_devices` have no such field in their own daemon reply, so this
//! bridge fetches it separately via `Request::QueryEvents` — see
//! `tools.rs`'s `fetch_new_events`.
//!
//! [`EventWatermarks`] is what makes that fetch return only *new* events
//! rather than the device's entire history every time: one high-water mark
//! per device, advanced past the highest `seq` this bridge has already
//! handed back (from *any* tool, not just `QueryEvents` — `tail`/
//! `read_since`'s own embedded events advance it too), so the very next
//! read tool call after a disconnect is guaranteed to include that
//! disconnect event exactly once, per the "斷線發生時，下一次任何讀取工具
//! 的結果都含 disconnect 事件" acceptance criterion.
//!
//! # Where a watermark starts: the device's tip when the bridge first sees it
//!
//! Scoped to this bridge process's lifetime only (in-memory, not
//! persisted). A device has *no* watermark until this bridge first touches
//! it — in any tool, including appearing in a `list_devices` reply — and at
//! that moment it is initialized to the device's current stream tip
//! ([`EventWatermarks::init_at`], fed by `tools.rs`'s `ensure_watermark`).
//! Only events recorded after that point are ever delivered as "new".
//!
//! This used to start every device at seq 0, so a fresh bridge's first call
//! handed back the device's *entire* recorded event history. On a
//! long-lived install that history only grows: the field report this was
//! fixed against had a first `list_devices` return a 1.39 MB JSON-RPC line,
//! 563 KB of it 1,840 lease/disconnect/config-change events dating back
//! weeks, across 33 devices. None of it was "new to the agent" in any
//! useful sense — an agent that wants history asks for it explicitly
//! (`tail`, `read_since`, `serialwarden export`), bounded.
//!
//! A device that first appears *after* the bridge started (hot-plug, or
//! simply one the agent never touched before) also starts at its tip when
//! first seen. Its events between bridge start and first sight are not
//! replayed: until the agent has looked at a device there is no "since you
//! last looked" to be relative to, and a device's recorded history cannot be
//! split into "before/after this bridge started" without comparing wall
//! clocks across what may be two machines (the bridge can reach a remote
//! daemon through a forwarded socket). The device's own `connected` state
//! is in `list_devices`, and `tail` shows its recent window, events
//! included in the stream.
//!
//! # The size cap, and why the watermark advances past what it omits
//!
//! Even post-bridge events can arrive in a burst (a flapping lease, a
//! script hammering config changes). [`cap_events`] bounds what one tool
//! result carries to [`events_cap_bytes`] — the presentation layer's own
//! default `max_result_bytes` — keeping the *newest* events and reporting
//! the rest explicitly (`events_truncated`, `events_omitted`, and per
//! device a `first_seq`/`last_seq` range), never silently.
//!
//! The watermark still advances past the omitted events. Holding it back so
//! they come out on later calls instead would deliver oldest-first, so a
//! device producing events faster than one result can carry would starve
//! the newest — the disconnect an agent most needs to see would sit behind
//! an ever-growing backlog. And a single high-water mark cannot describe
//! "these newer ones were delivered, those older ones were not" anyway.
//! Nothing is lost by advancing: the omitted events stay in the daemon's
//! record stream, and the reported range's `first_seq` is a valid
//! `read_since` cursor (which never filters events out) that pages through
//! them in bounded chunks.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

/// Reconstruct a [`serialwardend::query::OobRecord`] from one daemon wire
/// event object (`TASKS.md` T3.2, issue #13) — the mirror image of
/// `crate::mcp::line::assembled_line_from_wire`, needed for the same
/// reason: this bridge calls `serialwardend::presentation::present` directly
/// (reusing the daemon crate's own logic rather than reimplementing it),
/// which takes real `OobRecord`s, not wire JSON. Every field this produces
/// round-trips back through `serialwardend::presentation::event_to_json` to
/// the identical wire shape the daemon itself sent (same field names as
/// `serialwardend::protocol::session`'s private `oob_json`).
pub fn oob_from_wire(v: &Value) -> serialwardend::query::OobRecord {
    use serialwardend::query::OobRecord;
    use warden_proto::Kind;

    let kind = match v.get("kind").and_then(Value::as_str) {
        Some("rx") => Kind::Rx,
        Some("tx") => Kind::Tx,
        Some("gate") => Kind::Gate,
        _ => Kind::Event,
    };
    let name = v
        .get("event")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let mut extra = serde_json::Map::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if !matches!(k.as_str(), "seq" | "t_mono" | "t_wall" | "kind" | "event") {
                extra.insert(k.clone(), val.clone());
            }
        }
    }
    OobRecord {
        seq: v.get("seq").and_then(Value::as_u64).unwrap_or(0),
        t_mono: v.get("t_mono").and_then(Value::as_f64).unwrap_or(0.0),
        t_wall: v
            .get("t_wall")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        kind,
        name,
        extra,
    }
}

#[derive(Default)]
pub struct EventWatermarks {
    /// device id -> lowest event `seq` not yet delivered. A device absent
    /// from this map has not been seen by this bridge yet — see the module
    /// docs.
    next_seq: Mutex<HashMap<String, u64>>,
}

impl EventWatermarks {
    /// Whether `device` already has a watermark.
    pub fn is_tracked(&self, device: &str) -> bool {
        self.next_seq
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(device)
    }

    /// Start tracking `device` at `tip` (its stream tip, i.e. one past its
    /// newest record) — a no-op if it is already tracked, so a concurrent
    /// call that initialized it first and has since advanced it can never
    /// be rolled back by a second, staler initialization.
    pub fn init_at(&self, device: &str, tip: u64) {
        self.next_seq
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(device.to_string())
            .or_insert(tip);
    }

    /// The `since_seq` to pass to `Request::QueryEvents` for `device` right
    /// now — everything at or after this point is "new". Callers initialize
    /// the device first ([`Self::init_at`]); the 0 fallback only matters to
    /// this module's own unit tests.
    pub fn since_seq(&self, device: &str) -> u64 {
        *self
            .next_seq
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device)
            .unwrap_or(&0)
    }

    /// Advance `device`'s watermark past the highest `seq` among `events`
    /// (each expected to have a `seq` field, matching `oob_json`'s wire
    /// shape). A no-op if `events` is empty or none of them are newer than
    /// the current watermark — advancing can only ever move forward, never
    /// back, so calling this with a stale/overlapping batch is always safe.
    pub fn advance(&self, device: &str, events: &[Value]) {
        let Some(max_seq) = events
            .iter()
            .filter_map(|e| e.get("seq").and_then(Value::as_u64))
            .max()
        else {
            return;
        };
        let mut map = self.next_seq.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(device.to_string()).or_insert(0);
        if max_seq + 1 > *entry {
            *entry = max_seq + 1;
        }
    }

    /// Filter `events` (a full or over-wide batch — e.g. `tail`'s daemon
    /// reply, which always carries a device's *entire* out-of-band event
    /// history, not just what's new — see `query::DeviceQueryState::tail`'s
    /// docs) down to only the ones at/after `device`'s current watermark,
    /// then advance the watermark past whatever was returned.
    ///
    /// Read-filter-advance happens as one critical section under this
    /// struct's own lock (no `.await` anywhere in between — the caller
    /// already has `events` in hand), which is what makes "each event
    /// handed back to a tool call exactly once" hold even under two
    /// concurrent calls for the same device: unlike
    /// [`Self::since_seq`]/[`Self::advance`] called as two separate steps
    /// around an `.await` (see `tools.rs`'s `fetch_new_events`, which needs
    /// its own separate serialization for exactly this reason), there is no
    /// window here for another call to observe the same pre-advance
    /// watermark.
    pub fn take_new(&self, device: &str, events: &[Value]) -> Vec<Value> {
        let mut map = self.next_seq.lock().unwrap_or_else(|e| e.into_inner());
        let since = *map.get(device).unwrap_or(&0);
        let new_events: Vec<Value> = events
            .iter()
            .filter(|e| {
                e.get("seq")
                    .and_then(Value::as_u64)
                    .is_none_or(|seq| seq >= since)
            })
            .cloned()
            .collect();
        if let Some(max_seq) = new_events
            .iter()
            .filter_map(|e| e.get("seq").and_then(Value::as_u64))
            .max()
        {
            let entry = map.entry(device.to_string()).or_insert(0);
            if max_seq + 1 > *entry {
                *entry = max_seq + 1;
            }
        }
        new_events
    }
}

/// The hard cap, in serialized JSON bytes, on one tool result's `events`
/// array: the presentation layer's default
/// [`PresentationLimits::max_result_bytes`](serialwardend::presentation::PresentationLimits),
/// the same budget `tail`/`read_since` give a whole page by default.
pub fn events_cap_bytes() -> usize {
    serialwardend::presentation::PresentationLimits::default().max_result_bytes
}

/// Events one device had to leave out of a result because of the cap.
#[derive(Debug, Clone, PartialEq)]
pub struct OmittedRange {
    pub device: String,
    pub first_seq: u64,
    pub last_seq: u64,
    pub count: usize,
}

/// What [`cap_events`] kept, and what it had to leave out.
#[derive(Debug, Default, PartialEq)]
pub struct CappedEvents {
    /// Exactly the JSON values handed in, in their original order.
    pub kept: Vec<Value>,
    /// One entry per device that lost at least one event, in order of
    /// each device's first appearance in the input.
    pub omitted: Vec<OmittedRange>,
}

impl CappedEvents {
    pub fn omitted_count(&self) -> usize {
        self.omitted.iter().map(|r| r.count).sum()
    }

    /// Write `events` plus the explicit truncation marker into `result`:
    /// `events_truncated` always; `events_omitted` (a count) and
    /// `events_omitted_ranges` (`[{device, first_seq, last_seq, count}]`)
    /// only when something was left out. See the module docs.
    pub fn attach_to(self, result: &mut Value) {
        let truncated = !self.omitted.is_empty();
        let omitted_count = self.omitted_count();
        result["events"] = Value::Array(self.kept);
        result["events_truncated"] = Value::Bool(truncated);
        if truncated {
            result["events_omitted"] = serde_json::json!(omitted_count);
            result["events_omitted_ranges"] = Value::Array(
                self.omitted
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "device": r.device,
                            "first_seq": r.first_seq,
                            "last_seq": r.last_seq,
                            "count": r.count,
                        })
                    })
                    .collect(),
            );
        }
    }
}

/// Keep the newest suffix of `events` (ordered oldest to newest, each
/// paired with its device id) whose JSON array serialization fits in
/// `max_bytes`; report everything older as omitted, per device. A hard cap:
/// unlike the presentation layer's forward-progress rule, no event is kept
/// if it alone would exceed the budget, because the omitted range is always
/// reported and reachable — see the module docs.
pub fn cap_events(events: Vec<(String, Value)>, max_bytes: usize) -> CappedEvents {
    // "[" + "]", then each element plus a separating comma after the first.
    let mut used = 2usize;
    let mut keep_from = events.len();
    for (i, (_, event)) in events.iter().enumerate().rev() {
        let size = event.to_string().len() + usize::from(keep_from < events.len());
        if used + size > max_bytes {
            break;
        }
        used += size;
        keep_from = i;
    }

    let mut capped = CappedEvents::default();
    for (i, (device, event)) in events.into_iter().enumerate() {
        if i >= keep_from {
            capped.kept.push(event);
            continue;
        }
        let seq = event.get("seq").and_then(Value::as_u64).unwrap_or(0);
        match capped.omitted.iter_mut().find(|r| r.device == device) {
            Some(range) => {
                range.first_seq = range.first_seq.min(seq);
                range.last_seq = range.last_seq.max(seq);
                range.count += 1;
            }
            None => capped.omitted.push(OmittedRange {
                device,
                first_seq: seq,
                last_seq: seq,
                count: 1,
            }),
        }
    }
    capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn oob_from_wire_round_trips_an_event_record() {
        let wire = json!({
            "seq": 3, "t_mono": 1.5, "t_wall": "t3", "kind": "event",
            "event": "disconnect", "device_id": "usb-1",
        });
        let record = oob_from_wire(&wire);
        assert_eq!(record.seq, 3);
        assert_eq!(record.kind, warden_proto::Kind::Event);
        assert_eq!(record.name.as_deref(), Some("disconnect"));
        assert_eq!(
            record.extra.get("device_id").and_then(Value::as_str),
            Some("usb-1")
        );
        // Round-trips back to the identical wire shape.
        assert_eq!(serialwardend::presentation::event_to_json(&record), wire);
    }

    #[test]
    fn oob_from_wire_round_trips_a_gate_record() {
        let wire = json!({
            "seq": 9, "t_mono": 2.0, "t_wall": "t9", "kind": "gate",
            "action": "deny", "reason": "timeout_60s", "request_seq": 1,
        });
        let record = oob_from_wire(&wire);
        assert_eq!(record.kind, warden_proto::Kind::Gate);
        assert!(record.name.is_none());
        assert_eq!(
            record.extra.get("action").and_then(Value::as_str),
            Some("deny")
        );
    }

    #[test]
    fn untracked_device_falls_back_to_watermark_zero() {
        let w = EventWatermarks::default();
        assert!(!w.is_tracked("dev"));
        assert_eq!(w.since_seq("dev"), 0);
    }

    #[test]
    fn init_at_the_tip_hides_all_prior_history() {
        let w = EventWatermarks::default();
        w.init_at("dev", 1841);
        assert!(w.is_tracked("dev"));
        let history: Vec<Value> = (0..1841u64).map(|s| json!({"seq": s})).collect();
        assert!(w.take_new("dev", &history).is_empty());
        let mut grown = history.clone();
        grown.push(json!({"seq": 1841, "event": "disconnect"}));
        assert_eq!(
            w.take_new("dev", &grown),
            vec![json!({"seq": 1841, "event": "disconnect"})]
        );
    }

    #[test]
    fn init_at_never_overrides_an_existing_watermark() {
        let w = EventWatermarks::default();
        w.init_at("dev", 10);
        w.advance("dev", &[json!({"seq": 20})]);
        w.init_at("dev", 5);
        assert_eq!(w.since_seq("dev"), 21);
    }

    fn sized_event(seq: u64) -> Value {
        json!({"seq": seq, "kind": "event", "event": "lease_start", "command": "x".repeat(60)})
    }

    #[test]
    fn cap_events_keeps_everything_that_fits_and_marks_nothing() {
        let events: Vec<(String, Value)> = (0..3u64)
            .map(|s| ("dev".to_string(), sized_event(s)))
            .collect();
        let capped = cap_events(events.clone(), 8192);
        assert_eq!(capped.kept.len(), 3);
        assert!(capped.omitted.is_empty());

        let mut result = json!({});
        capped.attach_to(&mut result);
        assert_eq!(result["events_truncated"], false);
        assert!(result.get("events_omitted").is_none());
        assert!(result.get("events_omitted_ranges").is_none());
    }

    #[test]
    fn cap_events_keeps_the_newest_suffix_under_the_byte_cap() {
        let events: Vec<(String, Value)> = (0..1000u64)
            .map(|s| ("dev".to_string(), sized_event(s)))
            .collect();
        let capped = cap_events(events, 2048);
        assert!(Value::Array(capped.kept.clone()).to_string().len() <= 2048);
        assert_eq!(capped.kept.last().unwrap()["seq"], 999);
        let first_kept = capped.kept[0]["seq"].as_u64().unwrap();
        assert_eq!(
            capped.omitted,
            vec![OmittedRange {
                device: "dev".to_string(),
                first_seq: 0,
                last_seq: first_kept - 1,
                count: first_kept as usize,
            }]
        );
        assert_eq!(capped.omitted_count() + capped.kept.len(), 1000);

        let mut result = json!({});
        capped.attach_to(&mut result);
        assert_eq!(result["events_truncated"], true);
        assert_eq!(result["events_omitted"], first_kept);
        assert_eq!(result["events_omitted_ranges"][0]["first_seq"], 0);
    }

    #[test]
    fn cap_events_reports_omitted_ranges_per_device() {
        let mut events = Vec::new();
        for s in 0..50u64 {
            events.push(("dev-a".to_string(), sized_event(s)));
            events.push(("dev-b".to_string(), sized_event(100 + s)));
        }
        let capped = cap_events(events, 1024);
        assert_eq!(capped.omitted.len(), 2);
        assert_eq!(capped.omitted[0].device, "dev-a");
        assert_eq!(capped.omitted[0].first_seq, 0);
        assert_eq!(capped.omitted[1].device, "dev-b");
        assert_eq!(capped.omitted[1].first_seq, 100);
        assert_eq!(capped.omitted_count() + capped.kept.len(), 100);
    }

    #[test]
    fn cap_events_is_hard_even_for_a_single_oversized_event() {
        let big = json!({"seq": 7, "blob": "y".repeat(10_000)});
        let capped = cap_events(vec![("dev".to_string(), big)], 8192);
        assert!(capped.kept.is_empty());
        assert_eq!(capped.omitted[0].first_seq, 7);
        assert_eq!(capped.omitted[0].count, 1);
    }

    #[test]
    fn advance_moves_the_watermark_past_the_highest_seen_seq() {
        let w = EventWatermarks::default();
        w.advance(
            "dev",
            &[json!({"seq": 3}), json!({"seq": 7}), json!({"seq": 5})],
        );
        assert_eq!(w.since_seq("dev"), 8);
    }

    #[test]
    fn advance_never_moves_the_watermark_backward() {
        let w = EventWatermarks::default();
        w.advance("dev", &[json!({"seq": 10})]);
        assert_eq!(w.since_seq("dev"), 11);
        w.advance("dev", &[json!({"seq": 2})]);
        assert_eq!(
            w.since_seq("dev"),
            11,
            "an older/overlapping batch must not roll the watermark back"
        );
    }

    #[test]
    fn advance_with_no_events_is_a_no_op() {
        let w = EventWatermarks::default();
        w.advance("dev", &[]);
        assert_eq!(w.since_seq("dev"), 0);
    }

    #[test]
    fn watermarks_are_tracked_independently_per_device() {
        let w = EventWatermarks::default();
        w.advance("dev-a", &[json!({"seq": 100})]);
        assert_eq!(w.since_seq("dev-a"), 101);
        assert_eq!(w.since_seq("dev-b"), 0);
    }

    #[test]
    fn take_new_returns_the_full_batch_on_a_fresh_device_then_nothing_on_repeat() {
        let w = EventWatermarks::default();
        let full_history = vec![
            json!({"seq": 0, "event": "connect"}),
            json!({"seq": 3, "event": "disconnect"}),
        ];

        let first = w.take_new("dev", &full_history);
        assert_eq!(first, full_history);

        // The same (unbounded, always-full-history) batch handed to a
        // second call must not repeat anything already delivered -- this
        // is exactly what protects `tail` (whose daemon reply always
        // carries the device's entire event history, not just what's new)
        // from re-delivering the same disconnect on every subsequent call.
        let second = w.take_new("dev", &full_history);
        assert!(second.is_empty(), "repeat delivery: {second:?}");
    }

    #[test]
    fn take_new_returns_only_the_incremental_tail_of_a_growing_batch() {
        let w = EventWatermarks::default();
        let first_batch = vec![json!({"seq": 0}), json!({"seq": 1})];
        assert_eq!(w.take_new("dev", &first_batch), first_batch);

        let grown_batch = vec![json!({"seq": 0}), json!({"seq": 1}), json!({"seq": 2})];
        let incremental = w.take_new("dev", &grown_batch);
        assert_eq!(incremental, vec![json!({"seq": 2})]);
    }
}
