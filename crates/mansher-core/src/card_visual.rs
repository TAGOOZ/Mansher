//! Card motion state machine (upstream PeggedView / GrabArea / CaptureFlight).
//!
//! One structure replaces scattered press / copied checks at card
//! build time. Precedence: press dominates hover, arrival dominates rest,
//! gust only from rest while revealed. Copy nudge is
//! an orthogonal overlay (300ms) while the badge lives 1200ms in the app.

use std::collections::{HashMap, HashSet};

use super::Peg;

/// Milliseconds since an arbitrary epoch. Passed in so tests use a fake clock.
pub type Ms = u64;

const ARRIVAL_MS: Ms = 1100;
const GUST_MS: Ms = 900;
const GUST_MIN_MS: Ms = 7000;
const GUST_SPREAD_MS: Ms = 9000;
const NUDGE_MS: Ms = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Arriving,
    Rest,
    Gust,
    Hovered,
    Pressed,
}

#[derive(Debug, Clone)]
pub struct CardState {
    pub phase: Phase,
    pub since: Ms,
    pub tilt_deg: f64,
    pub gust_slot: u8,
    pub next_gust: Ms,
    pub copy_nudge_until: Option<Ms>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardFrame {
    pub id: u128,
    pub tilt_deg: f64,
    pub phase: Phase,
    pub gust_slot: u8,
    pub copy_nudge: bool,
    pub falling: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct UiHints {
    pub now: Ms,
    pub hovered: Option<u128>,
    pub pressed: Option<u128>,
    pub revealed: bool,
}

#[derive(Debug, Default)]
pub struct CardVisuals {
    states: HashMap<u128, CardState>,
}

fn fold(id: u128) -> u64 {
    (id ^ (id >> 64)) as u64
}

fn slot(id: u128) -> u8 {
    (fold(id) % 4) as u8
}

fn gust_delay(id: u128) -> Ms {
    GUST_MIN_MS + fold(id) % GUST_SPREAD_MS
}

impl CardVisuals {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn hung(&mut self, id: u128, tilt_deg: f64, now: Ms) {
        self.states.insert(
            id,
            CardState {
                phase: Phase::Arriving,
                since: now,
                tilt_deg,
                gust_slot: slot(id),
                next_gust: now.saturating_add(gust_delay(id)),
                copy_nudge_until: None,
            },
        );
    }

    pub fn copied(&mut self, id: u128, now: Ms) {
        let st = self.states.entry(id).or_insert_with(|| CardState {
            phase: Phase::Rest,
            since: now,
            tilt_deg: 0.0,
            gust_slot: slot(id),
            next_gust: now.saturating_add(gust_delay(id)),
            copy_nudge_until: None,
        });
        st.copy_nudge_until = Some(now.saturating_add(NUDGE_MS));
    }

    pub fn dropped(&mut self, id: u128, _now: Ms) {
        self.states.remove(&id);
    }

    pub fn frames(&mut self, pegs: &[Peg], hints: UiHints) -> Vec<CardFrame> {
        let live: HashSet<u128> = pegs.iter().map(|p| p.id.as_u128()).collect();
        self.states.retain(|id, _| live.contains(id));
        let mut out = Vec::with_capacity(pegs.len());
        for p in pegs {
            let id = p.id.as_u128();
            let st = self.states.entry(id).or_insert_with(|| CardState {
                phase: Phase::Rest,
                since: hints.now,
                tilt_deg: p.tilt,
                gust_slot: slot(id),
                next_gust: hints.now.saturating_add(gust_delay(id)),
                copy_nudge_until: None,
            });
            let nudge = st.copy_nudge_until.is_some_and(|u| hints.now < u);
            let phase = if hints.pressed == Some(id) {
                Phase::Pressed
            } else if hints.hovered == Some(id) {
                Phase::Hovered
            } else {
                match st.phase {
                    Phase::Arriving if hints.now.saturating_sub(st.since) >= ARRIVAL_MS => {
                        st.phase = Phase::Rest;
                        st.since = hints.now;
                        Phase::Rest
                    }
                    Phase::Arriving => Phase::Arriving,
                    Phase::Gust if hints.now.saturating_sub(st.since) >= GUST_MS => {
                        st.phase = Phase::Rest;
                        st.since = hints.now;
                        st.next_gust = hints.now.saturating_add(gust_delay(id));
                        Phase::Rest
                    }
                    Phase::Gust => Phase::Gust,
                    Phase::Rest if hints.revealed && hints.now >= st.next_gust => {
                        st.phase = Phase::Gust;
                        st.since = hints.now;
                        Phase::Gust
                    }
                    _ => Phase::Rest,
                }
            };
            out.push(CardFrame {
                id,
                tilt_deg: st.tilt_deg,
                phase,
                gust_slot: st.gust_slot,
                copy_nudge: nudge,
                falling: false,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn peg(id: u128, tilt: f64, falling: bool) -> Peg {
        Peg {
            id: uuid::Uuid::from_u128(id),
            path: PathBuf::from("/tmp/x.png"),
            added_at: chrono::Local::now(),
            tilt,
            falling,
        }
    }

    fn hints(now: Ms) -> UiHints {
        UiHints {
            now,
            hovered: None,
            pressed: None,
            revealed: true,
        }
    }

    #[test]
    fn hang_swing_settles_to_rest() {
        let mut v = CardVisuals::new();
        v.hung(1, 1.5, 1000);
        let f = v.frames(&[peg(1, -2.0, false)], hints(1000));
        assert_eq!(f[0].phase, Phase::Arriving, "fresh hang swings in");
        assert_eq!(
            f[0].tilt_deg, 1.5,
            "tilt frozen at hang, later peg tilt ignored"
        );
        let f = v.frames(&[peg(1, -2.0, false)], hints(1000 + 1099));
        assert_eq!(f[0].phase, Phase::Arriving, "settle wobble still playing");
        let f = v.frames(&[peg(1, -2.0, false)], hints(1000 + 1100));
        assert_eq!(f[0].phase, Phase::Rest, "arrival window ends by 1100ms");
    }

    #[test]
    fn gust_fires_only_while_revealed() {
        let mut v = CardVisuals::new();
        v.hung(1, 0.0, 0);
        let hidden = UiHints {
            revealed: false,
            ..hints(7001)
        };
        let f = v.frames(&[peg(1, 0.0, false)], hidden);
        assert_eq!(f[0].phase, Phase::Rest, "gust frozen while hidden");
        let f = v.frames(&[peg(1, 0.0, false)], hints(7001));
        assert_eq!(
            f[0].phase,
            Phase::Gust,
            "gust fires at now + 7000 + hash % 9000"
        );
        assert_eq!(f[0].gust_slot, 1, "gust slot is hash % 4");
        let f = v.frames(&[peg(1, 0.0, false)], hints(7001 + 899));
        assert_eq!(f[0].phase, Phase::Gust, "gust holds its 900ms window");
        let f = v.frames(&[peg(1, 0.0, false)], hints(7001 + 900));
        assert_eq!(f[0].phase, Phase::Rest, "gust releases after 900ms");
        let f = v.frames(&[peg(1, 0.0, false)], hints(7001 + 900 + 7000 + 1));
        assert_eq!(f[0].phase, Phase::Gust, "tick reschedules the next gust");
    }

    #[test]
    fn press_beats_hover() {
        let mut v = CardVisuals::new();
        v.hung(1, 0.0, 0);
        let f = v.frames(&[peg(1, 0.0, false)], hints(1200));
        assert_eq!(f[0].phase, Phase::Rest, "settled before interaction");
        let h = UiHints {
            hovered: Some(1),
            ..hints(1100)
        };
        let f = v.frames(&[peg(1, 0.0, false)], h);
        assert_eq!(f[0].phase, Phase::Hovered, "hover scales the card");
        let h = UiHints {
            hovered: Some(1),
            pressed: Some(1),
            ..hints(1100)
        };
        let f = v.frames(&[peg(1, 0.0, false)], h);
        assert_eq!(f[0].phase, Phase::Pressed, "press wins over hover");
    }

    #[test]
    fn dropped_removes_state() {
        let mut v = CardVisuals::new();
        v.hung(1, 0.0, 0);
        v.dropped(1, 2000);
        assert!(
            !v.states.contains_key(&1),
            "dropped id leaves no visual state"
        );
        let h = UiHints {
            hovered: Some(1),
            pressed: Some(1),
            ..hints(2100)
        };
        let f = v.frames(&[peg(1, 0.0, false)], h);
        assert_eq!(
            f[0].phase,
            Phase::Pressed,
            "no terminal state outranks press and hover"
        );
        assert!(!f[0].falling, "frames never emit falling");
    }

    #[test]
    fn copy_nudge_overlays_without_changing_phase() {
        let mut v = CardVisuals::new();
        v.hung(1, 0.0, 0);
        v.copied(1, 1000);
        let h = UiHints {
            hovered: Some(1),
            ..hints(1100)
        };
        let f = v.frames(&[peg(1, 0.0, false)], h);
        assert!(f[0].copy_nudge, "nudge shows inside its 450ms window");
        assert_eq!(
            f[0].phase,
            Phase::Hovered,
            "nudge coexists with the base phase"
        );
        let h = UiHints {
            hovered: Some(1),
            ..hints(1450)
        };
        let f = v.frames(&[peg(1, 0.0, false)], h);
        assert!(
            !f[0].copy_nudge,
            "nudge ends at 450ms while the badge outlives it"
        );
    }

    #[test]
    fn absent_ids_pruned_each_frames_call() {
        let mut v = CardVisuals::new();
        v.hung(1, 0.0, 0);
        v.hung(2, 0.0, 0);
        let f = v.frames(&[peg(1, 0.0, false)], hints(100));
        assert_eq!(f.len(), 1, "frames follow the peg row");
        assert_eq!(v.states.len(), 1, "absent ids pruned each frames call");
        v.hung(2, 0.0, 200);
        let f = v.frames(&[peg(1, 0.0, false), peg(2, 0.0, false)], hints(200));
        assert_eq!(f[1].phase, Phase::Arriving, "re-hung id replays arrival");
    }
}
