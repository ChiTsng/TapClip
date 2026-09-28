//! Shared-slot state machine. Decides what a phone tap does and how the PC
//! clipboard must react. Pure logic with an injected clock, no Win32.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// Largest text shared in either direction (UTF-8 bytes).
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// What the user copied on the PC, as classified by the clipboard layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalCopy {
    /// Plain text with newlines normalized to `\n`.
    Text(String),
    /// Marked by its source app as not to be recorded or synced (passwords etc.).
    Sensitive,
    /// No usable text: images, files, or text over the size limit.
    Other,
}

/// Direction requested by the phone. `Auto` unless the user answered an `Ask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Auto,
    Send,
    Receive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapOutcome {
    /// The phone text is now offered on the PC clipboard.
    Sent,
    /// The phone should copy this PC text.
    Received(String),
    /// Both sides have something new; the phone must pick a direction.
    Ask,
    /// Both sides already hold the same text.
    InSync,
    /// Nothing new on either side.
    Nothing,
}

/// What the Windows layer must do to the PC clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Offer this phone text on the PC clipboard and (re)start the expiry timer.
    Offer(String),
    /// A new local copy superseded the offer: stop the timer, drop the snapshot.
    Abandon,
    /// The offer was pasted: re-publish it as a normal clipboard item.
    Promote(String),
    /// The offer expired unused: put back what was there before.
    Restore,
}

#[derive(Debug)]
enum Slot {
    Empty,
    /// The latest copy made on the PC.
    Pc {
        text: String,
        at: Instant,
        delivered: bool,
    },
    /// Phone text currently offered on the PC clipboard.
    Offer {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phone {
    Empty,
    /// Holds what it last received from the PC.
    FromPc,
    /// Holds what it already sent once.
    AlreadySent,
    New,
}

pub struct Engine {
    ttl: Duration,
    slot: Slot,
    /// Digest of the text the phone last received from the PC.
    from_pc: Option<u64>,
    /// Digest of the text the phone last sent to the PC.
    to_pc: Option<u64>,
}

impl Engine {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, slot: Slot::Empty, from_pc: None, to_pc: None }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn is_offering(&self) -> bool {
        matches!(self.slot, Slot::Offer { .. })
    }

    /// The user copied something on the PC.
    pub fn on_local_copy(&mut self, copy: LocalCopy, now: Instant) -> Effect {
        let effect = if self.is_offering() { Effect::Abandon } else { Effect::None };
        self.slot = match copy {
            LocalCopy::Text(text) if !text.is_empty() => Slot::Pc { text, at: now, delivered: false },
            _ => Slot::Empty,
        };
        effect
    }

    /// The phone tapped the tag, carrying its current clipboard text.
    pub fn on_tap(&mut self, phone: Option<String>, choice: Choice, now: Instant) -> (TapOutcome, Effect) {
        let phone = phone.map(|t| normalize_newlines(&t)).filter(|t| !t.is_empty());

        if let Some(text) = &phone
            && self.pc_text() == Some(text.as_str())
        {
            if let Slot::Pc { delivered, .. } = &mut self.slot {
                *delivered = true;
                self.from_pc = Some(digest(text));
            }
            return (TapOutcome::InSync, Effect::None);
        }

        let state = self.classify(phone.as_deref());
        let pc_fresh = self.pc_fresh(now);
        match (choice, phone) {
            (Choice::Send, Some(text)) => self.send(text),
            (Choice::Send, None) => (TapOutcome::Nothing, Effect::None),
            (Choice::Receive, _) => self.receive(),
            (Choice::Auto, phone) => match (state, pc_fresh) {
                (Phone::New, true) => (TapOutcome::Ask, Effect::None),
                (Phone::New | Phone::AlreadySent, false) => self.send(phone.expect("classified as present")),
                (_, true) => self.receive(),
                (Phone::Empty | Phone::FromPc, false) => (TapOutcome::Nothing, Effect::None),
            },
        }
    }

    /// Another app pasted the offered text.
    pub fn on_offer_used(&mut self, now: Instant) -> Effect {
        match std::mem::replace(&mut self.slot, Slot::Empty) {
            Slot::Offer { text } => {
                self.slot = Slot::Pc { text: text.clone(), at: now, delivered: true };
                Effect::Promote(text)
            }
            other => {
                self.slot = other;
                Effect::None
            }
        }
    }

    /// The offer's time ran out without a paste.
    pub fn on_offer_expired(&mut self) -> Effect {
        if self.is_offering() {
            self.slot = Slot::Empty;
            Effect::Restore
        } else {
            Effect::None
        }
    }

    fn classify(&self, phone: Option<&str>) -> Phone {
        let Some(text) = phone else { return Phone::Empty };
        let hash = Some(digest(text));
        if hash == self.from_pc {
            Phone::FromPc
        } else if hash == self.to_pc {
            Phone::AlreadySent
        } else {
            Phone::New
        }
    }

    fn pc_text(&self) -> Option<&str> {
        match &self.slot {
            Slot::Pc { text, .. } | Slot::Offer { text } => Some(text),
            Slot::Empty => None,
        }
    }

    fn pc_fresh(&self, now: Instant) -> bool {
        match &self.slot {
            Slot::Pc { text, at, delivered: false } => {
                now.saturating_duration_since(*at) <= self.ttl && Some(digest(text)) != self.from_pc
            }
            _ => false,
        }
    }

    fn send(&mut self, text: String) -> (TapOutcome, Effect) {
        self.to_pc = Some(digest(&text));
        self.slot = Slot::Offer { text: text.clone() };
        (TapOutcome::Sent, Effect::Offer(text))
    }

    fn receive(&mut self) -> (TapOutcome, Effect) {
        match &mut self.slot {
            Slot::Pc { text, delivered, .. } => {
                *delivered = true;
                self.from_pc = Some(digest(text));
                (TapOutcome::Received(text.clone()), Effect::None)
            }
            _ => (TapOutcome::Nothing, Effect::None),
        }
    }
}

/// Canonical in-memory form: `\n` line endings, as on iOS.
pub fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn digest(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(120);

    fn setup() -> (Engine, Instant) {
        (Engine::new(TTL), Instant::now())
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn text(s: &str) -> LocalCopy {
        LocalCopy::Text(s.to_owned())
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_owned())
    }

    #[test]
    fn fresh_pc_copy_is_received_once() {
        let (mut e, t0) = setup();
        e.on_local_copy(text("from pc"), t0);
        let (out, eff) = e.on_tap(None, Choice::Auto, t0 + secs(5));
        assert_eq!(out, TapOutcome::Received("from pc".into()));
        assert_eq!(eff, Effect::None);
        // The phone now holds it: tapping again moves nothing.
        let (out, _) = e.on_tap(some("from pc"), Choice::Auto, t0 + secs(6));
        assert_eq!(out, TapOutcome::InSync);
    }

    #[test]
    fn expired_pc_copy_is_not_offered() {
        let (mut e, t0) = setup();
        e.on_local_copy(text("old"), t0);
        let (out, _) = e.on_tap(None, Choice::Auto, t0 + TTL + secs(1));
        assert_eq!(out, TapOutcome::Nothing);
    }

    #[test]
    fn new_phone_text_is_sent_when_pc_has_nothing_fresh() {
        let (mut e, t0) = setup();
        let (out, eff) = e.on_tap(some("from phone"), Choice::Auto, t0);
        assert_eq!(out, TapOutcome::Sent);
        assert_eq!(eff, Effect::Offer("from phone".into()));
        assert!(e.is_offering());
    }

    #[test]
    fn both_new_asks_then_honours_the_choice() {
        let (mut e, t0) = setup();
        e.on_local_copy(text("pc"), t0);
        let (out, _) = e.on_tap(some("phone"), Choice::Auto, t0 + secs(1));
        assert_eq!(out, TapOutcome::Ask);

        let mut send = Engine::new(TTL);
        send.on_local_copy(text("pc"), t0);
        let (out, eff) = send.on_tap(some("phone"), Choice::Send, t0 + secs(2));
        assert_eq!((out, eff), (TapOutcome::Sent, Effect::Offer("phone".into())));

        let (out, _) = e.on_tap(some("phone"), Choice::Receive, t0 + secs(2));
        assert_eq!(out, TapOutcome::Received("pc".into()));
    }

    #[test]
    fn explicit_receive_still_works_just_after_expiry() {
        // The menu may sit open across the TTL boundary.
        let (mut e, t0) = setup();
        e.on_local_copy(text("pc"), t0);
        let (out, _) = e.on_tap(some("phone"), Choice::Receive, t0 + TTL + secs(3));
        assert_eq!(out, TapOutcome::Received("pc".into()));
    }

    #[test]
    fn text_received_from_pc_is_not_new_on_the_phone() {
        let (mut e, t0) = setup();
        e.on_local_copy(text("x"), t0);
        e.on_tap(None, Choice::Auto, t0 + secs(1));
        e.on_local_copy(text("y"), t0 + secs(10));
        let (out, _) = e.on_tap(some("x"), Choice::Auto, t0 + secs(11));
        assert_eq!(out, TapOutcome::Received("y".into()));
    }

    #[test]
    fn expired_offer_restores_and_a_later_tap_resends() {
        let (mut e, t0) = setup();
        e.on_tap(some("p"), Choice::Auto, t0);
        assert_eq!(e.on_offer_expired(), Effect::Restore);
        assert!(!e.is_offering());
        let (out, eff) = e.on_tap(some("p"), Choice::Auto, t0 + TTL + secs(30));
        assert_eq!((out, eff), (TapOutcome::Sent, Effect::Offer("p".into())));
    }

    #[test]
    fn newer_pc_copy_beats_an_already_sent_phone_text() {
        let (mut e, t0) = setup();
        e.on_tap(some("p"), Choice::Auto, t0);
        assert_eq!(e.on_local_copy(text("q"), t0 + secs(5)), Effect::Abandon);
        let (out, _) = e.on_tap(some("p"), Choice::Auto, t0 + secs(6));
        assert_eq!(out, TapOutcome::Received("q".into()));
    }

    #[test]
    fn same_text_on_both_sides_is_in_sync() {
        let (mut e, t0) = setup();
        e.on_local_copy(text("same"), t0);
        let (out, eff) = e.on_tap(some("same"), Choice::Auto, t0 + secs(1));
        assert_eq!((out, eff), (TapOutcome::InSync, Effect::None));
        // It counts as delivered: nothing fresh remains.
        let (out, _) = e.on_tap(None, Choice::Auto, t0 + secs(2));
        assert_eq!(out, TapOutcome::Nothing);
    }

    #[test]
    fn pasted_offer_is_promoted_and_then_in_sync() {
        let (mut e, t0) = setup();
        e.on_tap(some("p"), Choice::Auto, t0);
        assert_eq!(e.on_offer_used(t0 + secs(3)), Effect::Promote("p".into()));
        assert_eq!(e.on_offer_expired(), Effect::None);
        let (out, _) = e.on_tap(some("p"), Choice::Auto, t0 + secs(4));
        assert_eq!(out, TapOutcome::InSync);
        // A promoted phone text is not handed back as a fresh PC copy.
        let (out, _) = e.on_tap(None, Choice::Auto, t0 + secs(5));
        assert_eq!(out, TapOutcome::Nothing);
    }

    #[test]
    fn sensitive_or_non_text_copy_clears_the_slot() {
        let (mut e, t0) = setup();
        e.on_local_copy(text("x"), t0);
        e.on_local_copy(LocalCopy::Sensitive, t0 + secs(1));
        assert_eq!(e.on_tap(None, Choice::Auto, t0 + secs(2)).0, TapOutcome::Nothing);

        e.on_local_copy(text("y"), t0 + secs(3));
        e.on_local_copy(LocalCopy::Other, t0 + secs(4));
        assert_eq!(e.on_tap(None, Choice::Auto, t0 + secs(5)).0, TapOutcome::Nothing);
    }

    #[test]
    fn local_copy_during_offer_abandons_it() {
        let (mut e, t0) = setup();
        e.on_tap(some("p"), Choice::Auto, t0);
        assert_eq!(e.on_local_copy(LocalCopy::Other, t0 + secs(1)), Effect::Abandon);
        assert!(!e.is_offering());
        assert_eq!(e.on_offer_expired(), Effect::None);
    }

    #[test]
    fn line_endings_do_not_matter() {
        let (mut e, t0) = setup();
        e.on_local_copy(text("a\nb"), t0);
        let (out, _) = e.on_tap(some("a\r\nb"), Choice::Auto, t0 + secs(1));
        assert_eq!(out, TapOutcome::InSync);
    }

    #[test]
    fn empty_phone_text_counts_as_nothing() {
        let (mut e, t0) = setup();
        assert_eq!(e.on_tap(some(""), Choice::Auto, t0).0, TapOutcome::Nothing);
        assert_eq!(e.on_tap(some(""), Choice::Send, t0).0, TapOutcome::Nothing);
    }

    #[test]
    fn resending_while_offering_keeps_a_single_offer() {
        let (mut e, t0) = setup();
        e.on_tap(some("p1"), Choice::Auto, t0);
        let (out, eff) = e.on_tap(some("p2"), Choice::Auto, t0 + secs(1));
        assert_eq!((out, eff), (TapOutcome::Sent, Effect::Offer("p2".into())));
        assert_eq!(e.on_offer_used(t0 + secs(2)), Effect::Promote("p2".into()));
    }
}
