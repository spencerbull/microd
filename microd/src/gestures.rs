//! Device-agnostic gesture recognition over raw pad events.
//!
//! Consumes key press/release and joystick samples; emits higher-level
//! gestures. Pure state machine (time passed in) so it is unit-testable.
//!
//! Semantics:
//! - `tap`: press → release faster than the long-press threshold. For keys in
//!   the double-tap set, emission is deferred by the double window so a second
//!   tap can upgrade it to `double_tap`; other keys emit instantly on release.
//! - `long_press`: held past the threshold (fires at threshold, not release).
//! - `chord`: a key pressed while other keys are held. The held keys are
//!   reported as modifiers and their own tap/long_press is suppressed for
//!   that hold (a modifier used is not also a tap).
//! - `flick`: joystick deflection crossing 0.9 fires once with a 4-way
//!   direction; re-arms when deflection falls below 0.3.
//!
//! Encoder rotation (`step` actions) passes through untouched — steps are
//! already atomic; clients coalesce them as they see fit.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum Gesture {
    Tap { key: String },
    DoubleTap { key: String },
    LongPress { key: String },
    Chord { key: String, held: Vec<String> },
    Flick { direction: &'static str },
}

pub struct Config {
    pub long_press: Duration,
    pub double_window: Duration,
    /// Keys whose taps are deferred to allow double-tap detection.
    pub double_tap_keys: HashSet<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            long_press: Duration::from_millis(450),
            double_window: Duration::from_millis(250),
            double_tap_keys: HashSet::new(),
        }
    }
}

struct Held {
    pressed_at: Instant,
    /// Consumed as part of a chord or double-tap: emits nothing further and
    /// is no longer chord-eligible.
    consumed: bool,
    /// Long-press already emitted. Release emits nothing, but the key STAYS
    /// chord-eligible — a modifier held slowly (past the long threshold)
    /// must still form chords. Policy layers simply avoid binding long_press
    /// on keys they use as modifiers.
    long_fired: bool,
}

#[derive(Default)]
pub struct GestureEngine {
    pub config: Config,
    held: HashMap<String, Held>,
    /// Taps awaiting their double-tap window, keyed by key name.
    pending_tap: HashMap<String, Instant>,
    joy_armed: bool,
}

impl GestureEngine {
    pub fn new(config: Config) -> Self {
        GestureEngine { config, joy_armed: true, ..Default::default() }
    }

    pub fn key_press(&mut self, key: &str, now: Instant) -> Vec<Gesture> {
        let mut out = Vec::new();

        // Second tap within the window upgrades to double_tap.
        if let Some(first) = self.pending_tap.remove(key) {
            if now.duration_since(first) <= self.config.double_window {
                out.push(Gesture::DoubleTap { key: key.to_string() });
                self.held.insert(
                    key.to_string(),
                    Held { pressed_at: now, consumed: true, long_fired: false },
                );
                return out;
            }
            // Window expired but tick() hasn't run: emit the stale tap now.
            out.push(Gesture::Tap { key: key.to_string() });
        }

        // A press while other keys are held is a chord; every physically held
        // key acts as a modifier (already-consumed holds still qualify, so a
        // modifier kept down chords repeatedly).
        let mut held_names: Vec<String> = self.held.keys().cloned().collect();
        if !held_names.is_empty() {
            held_names.sort();
            for name in &held_names {
                if let Some(h) = self.held.get_mut(name) {
                    h.consumed = true;
                }
            }
            out.push(Gesture::Chord { key: key.to_string(), held: held_names });
            self.held.insert(
                key.to_string(),
                Held { pressed_at: now, consumed: true, long_fired: false },
            );
            return out;
        }

        self.held.insert(
            key.to_string(),
            Held { pressed_at: now, consumed: false, long_fired: false },
        );
        out
    }

    pub fn key_release(&mut self, key: &str, now: Instant) -> Vec<Gesture> {
        let Some(held) = self.held.remove(key) else {
            return Vec::new();
        };
        if held.consumed || held.long_fired {
            return Vec::new();
        }
        let elapsed = now.duration_since(held.pressed_at);
        if elapsed >= self.config.long_press {
            // Threshold passed but tick() didn't fire yet.
            return vec![Gesture::LongPress { key: key.to_string() }];
        }
        if self.config.double_tap_keys.contains(key) {
            self.pending_tap.insert(key.to_string(), now);
            return Vec::new();
        }
        vec![Gesture::Tap { key: key.to_string() }]
    }

    /// Encoder step. Alone it emits nothing (raw steps already flow to
    /// clients); while keys are held it emits a chorded step and consumes the
    /// held keys' own tap, making hold-and-turn a first-class gesture.
    pub fn step(&mut self, key: &str, _now: Instant) -> Vec<Gesture> {
        let mut held_names: Vec<String> = self.held.keys().cloned().collect();
        if held_names.is_empty() {
            return Vec::new();
        }
        held_names.sort();
        for name in &held_names {
            if let Some(h) = self.held.get_mut(name) {
                h.consumed = true;
            }
        }
        vec![Gesture::Chord { key: key.to_string(), held: held_names }]
    }

    pub fn joystick(&mut self, angle: f64, deflection: f64) -> Vec<Gesture> {
        if deflection < 0.3 {
            self.joy_armed = true;
            return Vec::new();
        }
        if !self.joy_armed || deflection < 0.9 {
            return Vec::new();
        }
        self.joy_armed = false;
        // Angle in normalized turns: 0 = right, 0.25 = down, 0.5 = left, 0.75 = up.
        let direction = match angle {
            a if !(0.125..0.875).contains(&a) => "right",
            a if a < 0.375 => "down",
            a if a < 0.625 => "left",
            _ => "up",
        };
        vec![Gesture::Flick { direction }]
    }

    /// Fire time-based gestures: long-press thresholds and expired tap windows.
    pub fn tick(&mut self, now: Instant) -> Vec<Gesture> {
        let mut out = Vec::new();
        for (key, held) in self.held.iter_mut() {
            if !held.consumed
                && !held.long_fired
                && now.duration_since(held.pressed_at) >= self.config.long_press
            {
                held.long_fired = true;
                out.push(Gesture::LongPress { key: key.clone() });
            }
        }
        let window = self.config.double_window;
        let expired: Vec<String> = self
            .pending_tap
            .iter()
            .filter(|(_, t)| now.duration_since(**t) > window)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            self.pending_tap.remove(&key);
            out.push(Gesture::Tap { key });
        }
        out
    }

    /// The next instant at which tick() could emit something, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        let long = self
            .held
            .values()
            .filter(|h| !h.consumed && !h.long_fired)
            .map(|h| h.pressed_at + self.config.long_press);
        let taps = self
            .pending_tap
            .values()
            .map(|t| *t + self.config.double_window);
        long.chain(taps).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> GestureEngine {
        GestureEngine::new(Config::default())
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn quick_tap_emits_instantly() {
        let mut e = engine();
        let t0 = Instant::now();
        assert!(e.key_press("AG00", t0).is_empty());
        let out = e.key_release("AG00", t0 + ms(100));
        assert_eq!(out, vec![Gesture::Tap { key: "AG00".into() }]);
    }

    #[test]
    fn long_press_fires_on_tick_and_release_is_silent() {
        let mut e = engine();
        let t0 = Instant::now();
        e.key_press("AG00", t0);
        assert!(e.tick(t0 + ms(200)).is_empty());
        let out = e.tick(t0 + ms(500));
        assert_eq!(out, vec![Gesture::LongPress { key: "AG00".into() }]);
        assert!(e.key_release("AG00", t0 + ms(600)).is_empty());
    }

    #[test]
    fn long_press_fires_on_release_if_tick_missed_it() {
        let mut e = engine();
        let t0 = Instant::now();
        e.key_press("AG00", t0);
        let out = e.key_release("AG00", t0 + ms(500));
        assert_eq!(out, vec![Gesture::LongPress { key: "AG00".into() }]);
    }

    #[test]
    fn double_tap_on_registered_key() {
        let mut e = engine();
        e.config.double_tap_keys.insert("AG00".into());
        let t0 = Instant::now();
        e.key_press("AG00", t0);
        assert!(e.key_release("AG00", t0 + ms(80)).is_empty()); // deferred
        let out = e.key_press("AG00", t0 + ms(200));
        assert_eq!(out, vec![Gesture::DoubleTap { key: "AG00".into() }]);
        assert!(e.key_release("AG00", t0 + ms(260)).is_empty());
    }

    #[test]
    fn deferred_tap_flushes_after_window() {
        let mut e = engine();
        e.config.double_tap_keys.insert("AG00".into());
        let t0 = Instant::now();
        e.key_press("AG00", t0);
        e.key_release("AG00", t0 + ms(80));
        let out = e.tick(t0 + ms(400));
        assert_eq!(out, vec![Gesture::Tap { key: "AG00".into() }]);
    }

    #[test]
    fn chord_consumes_modifier_tap() {
        let mut e = engine();
        let t0 = Instant::now();
        e.key_press("ACT12", t0);
        let out = e.key_press("AG02", t0 + ms(100));
        assert_eq!(
            out,
            vec![Gesture::Chord { key: "AG02".into(), held: vec!["ACT12".into()] }]
        );
        // Neither release produces anything: both were consumed by the chord.
        assert!(e.key_release("AG02", t0 + ms(200)).is_empty());
        assert!(e.key_release("ACT12", t0 + ms(300)).is_empty());
    }

    #[test]
    fn modifier_held_past_long_threshold_does_not_long_press_after_chord() {
        let mut e = engine();
        let t0 = Instant::now();
        e.key_press("ACT12", t0);
        e.key_press("AG02", t0 + ms(100));
        assert!(e.tick(t0 + ms(1000)).is_empty());
    }

    #[test]
    fn slow_modifier_still_chords_after_long_press_fires() {
        let mut e = engine();
        let t0 = Instant::now();
        e.key_press("ACT12", t0);
        // Modifier held past the threshold: long_press fires...
        assert_eq!(
            e.tick(t0 + ms(500)),
            vec![Gesture::LongPress { key: "ACT12".into() }]
        );
        // ...but a second key press must still form the chord.
        let out = e.key_press("AG02", t0 + ms(900));
        assert_eq!(
            out,
            vec![Gesture::Chord { key: "AG02".into(), held: vec!["ACT12".into()] }]
        );
        assert!(e.key_release("AG02", t0 + ms(1000)).is_empty());
        assert!(e.key_release("ACT12", t0 + ms(1100)).is_empty());
    }

    #[test]
    fn held_modifier_chords_repeatedly() {
        let mut e = engine();
        let t0 = Instant::now();
        e.key_press("ACT12", t0);
        assert_eq!(
            e.key_press("AG02", t0 + ms(100)),
            vec![Gesture::Chord { key: "AG02".into(), held: vec!["ACT12".into()] }]
        );
        e.key_release("AG02", t0 + ms(200));
        // Modifier still down: a second key must chord again.
        assert_eq!(
            e.key_press("AG03", t0 + ms(300)),
            vec![Gesture::Chord { key: "AG03".into(), held: vec!["ACT12".into()] }]
        );
        e.key_release("AG03", t0 + ms(400));
        assert!(e.key_release("ACT12", t0 + ms(500)).is_empty());
    }

    #[test]
    fn step_alone_is_silent() {
        let mut e = engine();
        assert!(e.step("ENC_CW", Instant::now()).is_empty());
    }

    #[test]
    fn step_while_key_held_chords_and_consumes_tap() {
        let mut e = engine();
        let t0 = Instant::now();
        e.key_press("ACT12", t0);
        assert_eq!(
            e.step("ENC_CW", t0 + ms(100)),
            vec![Gesture::Chord { key: "ENC_CW".into(), held: vec!["ACT12".into()] }]
        );
        // Continued turning keeps chording.
        assert_eq!(
            e.step("ENC_CW", t0 + ms(150)),
            vec![Gesture::Chord { key: "ENC_CW".into(), held: vec!["ACT12".into()] }]
        );
        // The held key's own tap was consumed.
        assert!(e.key_release("ACT12", t0 + ms(300)).is_empty());
    }

    #[test]
    fn flick_fires_once_and_rearms() {
        let mut e = engine();
        assert!(e.joystick(0.75, 0.5).is_empty()); // armed but below fire threshold
        assert_eq!(e.joystick(0.75, 1.0), vec![Gesture::Flick { direction: "up" }]);
        assert!(e.joystick(0.75, 1.0).is_empty()); // not re-armed yet
        assert!(e.joystick(0.0, 0.1).is_empty()); // re-arm
        assert_eq!(e.joystick(0.5, 0.95), vec![Gesture::Flick { direction: "left" }]);
    }

    #[test]
    fn next_deadline_tracks_held_and_pending() {
        let mut e = engine();
        assert!(e.next_deadline().is_none());
        let t0 = Instant::now();
        e.key_press("AG00", t0);
        assert_eq!(e.next_deadline(), Some(t0 + e.config.long_press));
    }
}
