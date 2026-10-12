//! Portable Show/hide state machine — a 1:1 port of upstream
//! `AppDelegate` reveal logic (`tick()`, `reveal()`, `setRevealed()`),
//! decoupled from the *sensor* so every distro gets true hover legally.
//!
//! Upstream senses with a 30Hz global pointer poll, which Wayland forbids.
//! Here the sensor is pluggable; the state machine is identical:
//!
//! - [`Sensor::LayerStrip`] — 2–3px `layer-shell` exclusive strip at the top
//!   edge (Niri/Sway/Hyprland/KDE). Hover-enter fires inside our own surface:
//!   Wayland-legal, zero polling, true Tendedero feel.
//! - [`Sensor::X11Poll`] — X11 sessions only (`XQueryPointer` at 30Hz, exact
//!   macOS clone: menu-band dwell 250ms, menu-click suppression). No polling
//!   loop behind it yet (dead end noted by plan 010, untestable here).
//!
//! Timings mirror upstream: reveal dwell 250ms, retract 500ms away,
//! new-capture peek 2.5s, `pinned` (shortcut-opened) stays until visited.

/// Milliseconds since an arbitrary epoch. Passed in so tests use a fake clock.
pub type Ms = u64;

pub const REVEAL_DELAY_MS: Ms = 250;
pub const RETRACT_DELAY_MS: Ms = 500;
pub const PEEK_MS: Ms = 2500;

/// Which hover sensor feeds the machine on this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensor {
    LayerStrip,
    X11Poll,
    /// No hover sensor: tray + hotkey + peek only.
    None,
}

impl Sensor {
    /// True hover available, or degraded to click-to-reveal?
    pub fn has_hover(self) -> bool {
        !matches!(self, Sensor::None)
    }
}

/// What the UI should do after an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Noop,
    Reveal,
    Hide,
}

/// Events from sensors and the app. Names mirror `AppDelegate` members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Pointer entered the hot strip / menu-bar band.
    HotEnter(Ms),
    /// Pointer left the hot strip.
    HotLeave,
    /// Pointer inside the revealed line zone.
    InsideLine,
    /// Pointer outside the revealed line zone.
    OutsideLine(Ms),
    /// A new screenshot hung — auto-peek.
    Captured(Ms),
    /// Hotkey or tray toggle (stays open while empty: `keepOpen`).
    Toggle,
    /// Click on a top bar / menu — suppress hover until pointer leaves.
    TopBarClicked,
    /// Pointer left the top bar entirely — lift suppression.
    TopBarLeft,
    /// Fullscreen app now covers (`true`) or left (`false`) our monitor.
    Fullscreen(bool),
    /// Drag / long-press in progress — never retract while busy.
    Busy(bool),
}

/// Portable port of `AppDelegate` show/hide state.
#[derive(Debug)]
pub struct ShowHide {
    pub sensor: Sensor,
    pub wanted: bool,
    pub revealed: bool,
    pub pinned: bool,
    fullscreen_blocked: bool,
    busy: bool,
    suppressed: bool,
    /// Explicit toggle-hide lingers until the pointer leaves the hot strip:
    /// swallows the re-reveal that would otherwise fire while the hiding
    /// cursor still sits inside. Cleared by `HotLeave`, never by dwell.
    /// Separate from `suppressed` (menu-click suppression, lifted only by
    /// `TopBarLeft`) so strip-leave timing stays out of the X11 top-bar flow.
    hide_linger: bool,
    hot_since: Option<Ms>,
    away_since: Option<Ms>,
    peek_until: Ms,
}

impl ShowHide {
    pub fn new(sensor: Sensor) -> Self {
        Self {
            sensor,
            wanted: false,
            revealed: false,
            pinned: false,
            fullscreen_blocked: false,
            busy: false,
            suppressed: false,
            hide_linger: false,
            hot_since: None,
            away_since: None,
            peek_until: 0,
        }
    }

    /// Can the panel be ordered in at all? (`refresh()`: wanted && !blocked.)
    pub fn presentable(&self) -> bool {
        self.wanted && !self.fullscreen_blocked
    }

    /// Drag / long-press in progress — the UI polls this each tick.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn handle(&mut self, ev: Event) -> Command {
        match ev {
            Event::HotEnter(now) => {
                if self.suppressed || self.hide_linger || self.revealed || !self.presentable() {
                    return Command::Noop;
                }
                if self.hot_since.is_none() {
                    self.hot_since = Some(now);
                }
                if now.saturating_sub(self.hot_since.unwrap_or(now)) >= REVEAL_DELAY_MS {
                    self.hot_since = None;
                    return self.set_revealed(true);
                }
                Command::Noop
            }
            Event::HotLeave => {
                self.hot_since = None;
                self.hide_linger = false;
                Command::Noop
            }
            Event::InsideLine => {
                self.away_since = None;
                if self.pinned {
                    self.pinned = false; // visited: unpin, like upstream
                }
                Command::Noop
            }
            Event::OutsideLine(now) => {
                if !self.revealed || self.pinned || self.busy || now < self.peek_until {
                    return Command::Noop;
                }
                if self.away_since.is_none() {
                    self.away_since = Some(now);
                }
                if now.saturating_sub(self.away_since.unwrap_or(now)) >= RETRACT_DELAY_MS {
                    self.away_since = None;
                    return self.set_revealed(false);
                }
                Command::Noop
            }
            Event::Captured(now) => {
                self.wanted = true;
                if self.presentable() {
                    self.peek_until = now.saturating_add(PEEK_MS);
                    self.away_since = None;
                    return self.set_revealed(true);
                }
                Command::Noop
            }
            Event::Toggle => {
                if self.revealed {
                    let cmd = self.set_revealed(false);
                    // Hide sticks while the hiding cursor lingers; `wanted`
                    // stays true so the next leave-and-reenter reveals again.
                    self.hide_linger = true;
                    cmd
                } else {
                    self.wanted = true;
                    self.pinned = true;
                    if self.presentable() {
                        return self.set_revealed(true);
                    }
                    Command::Noop
                }
            }
            Event::TopBarClicked => {
                self.suppressed = true;
                self.hot_since = None;
                if self.revealed {
                    self.pinned = false;
                    return self.set_revealed(false);
                }
                Command::Noop
            }
            Event::TopBarLeft => {
                self.suppressed = false;
                Command::Noop
            }
            Event::Fullscreen(blocked) => {
                // Dead end noted by plan 010: no producer emits Fullscreen
                // yet; plan 023 owns finding one.
                self.fullscreen_blocked = blocked;
                if blocked && self.revealed {
                    return self.set_revealed(false);
                }
                Command::Noop
            }
            Event::Busy(b) => {
                self.busy = b;
                if b {
                    self.away_since = None;
                }
                Command::Noop
            }
        }
    }

    fn set_revealed(&mut self, on: bool) -> Command {
        if on == self.revealed {
            return Command::Noop;
        }
        self.revealed = on;
        if on {
            self.away_since = None;
            Command::Reveal
        } else {
            self.pinned = false;
            self.peek_until = 0;
            Command::Hide
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_reveals_after_dwell() {
        let mut sh = ShowHide::new(Sensor::LayerStrip);
        sh.wanted = true;
        assert_eq!(sh.handle(Event::HotEnter(1000)), Command::Noop);
        assert_eq!(
            sh.handle(Event::HotEnter(1000 + REVEAL_DELAY_MS)),
            Command::Reveal
        );
        assert!(sh.revealed);
    }

    #[test]
    fn suppressed_top_bar_click_blocks_reveal_until_leave() {
        let mut sh = ShowHide::new(Sensor::X11Poll);
        sh.wanted = true;
        sh.handle(Event::TopBarClicked);
        assert_eq!(sh.handle(Event::HotEnter(0)), Command::Noop);
        assert_eq!(
            sh.handle(Event::HotEnter(REVEAL_DELAY_MS + 1)),
            Command::Noop
        );
        sh.handle(Event::TopBarLeft);
        assert_eq!(sh.handle(Event::HotEnter(5000)), Command::Noop);
        assert_eq!(
            sh.handle(Event::HotEnter(5000 + REVEAL_DELAY_MS)),
            Command::Reveal
        );
    }

    #[test]
    fn retracts_after_away_delay_unless_pinned_busy_or_peeking() {
        let mut sh = ShowHide::new(Sensor::LayerStrip);
        sh.wanted = true;
        sh.handle(Event::Captured(0)); // peek until 2500
        assert!(sh.revealed);
        assert_eq!(sh.handle(Event::OutsideLine(1000)), Command::Noop); // peeking
        assert_eq!(sh.handle(Event::OutsideLine(3000)), Command::Noop); // away starts
        assert_eq!(
            sh.handle(Event::OutsideLine(3000 + RETRACT_DELAY_MS)),
            Command::Hide
        );
    }

    #[test]
    fn pinned_toggle_stays_until_visited() {
        let mut sh = ShowHide::new(Sensor::None);
        assert_eq!(sh.handle(Event::Toggle), Command::Reveal);
        assert!(sh.pinned);
        assert_eq!(sh.handle(Event::OutsideLine(9000)), Command::Noop); // pinned: stays
        sh.handle(Event::InsideLine); // cursor visits → unpin
        assert!(!sh.pinned);
        assert_eq!(sh.handle(Event::OutsideLine(9500)), Command::Noop);
        assert_eq!(
            sh.handle(Event::OutsideLine(9500 + RETRACT_DELAY_MS)),
            Command::Hide
        );
    }

    #[test]
    fn fullscreen_hides_and_blocks() {
        let mut sh = ShowHide::new(Sensor::LayerStrip);
        sh.wanted = true;
        sh.handle(Event::Captured(0));
        assert!(sh.revealed);
        assert_eq!(sh.handle(Event::Fullscreen(true)), Command::Hide);
        assert!(!sh.presentable());
    }

    #[test]
    fn toggle_hide_lingers_until_leave() {
        let mut sh = ShowHide::new(Sensor::None);
        assert_eq!(sh.handle(Event::Toggle), Command::Reveal);
        assert_eq!(sh.handle(Event::Toggle), Command::Hide);
        // Lingering cursor must not re-reveal: hide sticks.
        assert_eq!(sh.handle(Event::HotEnter(1000)), Command::Noop);
        assert_eq!(
            sh.handle(Event::HotEnter(1000 + REVEAL_DELAY_MS)),
            Command::Noop
        );
        assert!(!sh.revealed);
    }

    #[test]
    fn toggle_hide_resurrects_after_leave() {
        let mut sh = ShowHide::new(Sensor::None);
        assert_eq!(sh.handle(Event::Toggle), Command::Reveal);
        assert_eq!(sh.handle(Event::Toggle), Command::Hide);
        assert!(!sh.revealed);
        sh.handle(Event::HotLeave);
        assert_eq!(sh.handle(Event::HotEnter(1000)), Command::Noop);
        assert_eq!(
            sh.handle(Event::HotEnter(1000 + REVEAL_DELAY_MS)),
            Command::Reveal
        );
    }

    #[test]
    fn toggle_show_hide_show_keeps_pin_flow() {
        let mut sh = ShowHide::new(Sensor::None);
        assert_eq!(sh.handle(Event::Toggle), Command::Reveal);
        assert!(sh.pinned);
        assert_eq!(sh.handle(Event::Toggle), Command::Hide);
        assert!(!sh.pinned);
        assert!(sh.wanted);
        assert_eq!(sh.handle(Event::Toggle), Command::Reveal);
        assert!(sh.pinned);
        assert!(sh.wanted);
    }

    #[test]
    fn topbar_suppression_survives_strip_leave() {
        let mut sh = ShowHide::new(Sensor::X11Poll);
        sh.wanted = true;
        sh.handle(Event::TopBarClicked);
        sh.handle(Event::HotLeave);
        assert_eq!(sh.handle(Event::HotEnter(0)), Command::Noop);
        assert_eq!(
            sh.handle(Event::HotEnter(REVEAL_DELAY_MS + 1)),
            Command::Noop
        );
        sh.handle(Event::TopBarLeft);
        assert_eq!(sh.handle(Event::HotEnter(5000)), Command::Noop);
        assert_eq!(
            sh.handle(Event::HotEnter(5000 + REVEAL_DELAY_MS)),
            Command::Reveal
        );
    }
}
