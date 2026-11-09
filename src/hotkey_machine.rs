//! Deterministic port of HotkeyHook.Callback, independent of OS threads or APIs.
use crate::hotkey::{ESCAPE, Hotkey, SPACE, is_modifier, normalize};
use std::collections::BTreeSet;

/// A physical or injected key event from the platform hook.
#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
    pub key: u8,
    pub down: bool,
    pub injected: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HotkeyEvent {
    Start,
    Lock,
    Release,
    StopLocked,
    CancelSilent,
    Cancel,
}
/// Everything the hook must do in response to one key event.
#[derive(Default, Debug)]
pub struct Outcome {
    pub swallow: bool,
    pub event: Option<HotkeyEvent>,
    pub mask_start_menu: bool,
    pub capture: Option<Option<Hotkey>>,
}
#[derive(Default, Eq, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Holding,
    Locked,
}

/// Owns physical-key bookkeeping, dictation state and shortcut capture.
#[derive(Default)]
pub struct HotkeyMachine {
    hotkey: Hotkey,
    held: BTreeSet<u8>,
    pressed: BTreeSet<u8>,
    swallowed: BTreeSet<u8>,
    phase: Phase,
    combo_held: bool,
    swallow_space_up: bool,
    swallow_esc_up: bool,
    capture: Option<Vec<u8>>,
}
impl HotkeyMachine {
    pub fn new(hotkey: Hotkey) -> Self {
        Self {
            hotkey,
            ..Self::default()
        }
    }
    pub fn set_hotkey(&mut self, hotkey: Hotkey) {
        self.hotkey = hotkey;
    }
    pub fn force_idle(&mut self) {
        self.phase = Phase::Idle;
    }
    pub fn hotkey(&self) -> &Hotkey {
        &self.hotkey
    }
    /// Holding or hands-free, when Esc belongs to WispX.
    pub fn dictating(&self) -> bool {
        self.phase != Phase::Idle
    }
    /// Space would switch the held dictation to hands-free.
    pub fn lock_armed(&self) -> bool {
        self.combo_held && self.phase == Phase::Holding
    }
    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }
    pub fn cancel_capture(&mut self) {
        self.capture = None;
    }
    /// Returns a silent cancellation if capture interrupted a dictation.
    pub fn begin_capture(&mut self) -> Option<HotkeyEvent> {
        let cancelled = (self.phase != Phase::Idle).then_some(HotkeyEvent::CancelSilent);
        self.force_idle();
        self.combo_held = false;
        self.capture = Some(Vec::new());
        cancelled
    }
    pub fn process(&mut self, event: KeyEvent) -> Outcome {
        let mut out = Outcome::default();
        if event.injected {
            return out;
        }
        let key = normalize(event.key);
        let down = event.down;
        let modifier = is_modifier(key);
        let repeat = if modifier {
            if down {
                self.held.insert(key);
            } else {
                self.held.remove(&key);
            }
            false
        } else if down {
            !self.pressed.insert(key)
        } else {
            self.pressed.remove(&key);
            false
        };

        if let Some(largest) = &mut self.capture {
            if modifier && down {
                out.mask_start_menu = true;
                if self.held.len() > largest.len() {
                    *largest = self.held.iter().copied().collect();
                }
            }
            if down && !modifier || modifier && !down && self.held.is_empty() && !largest.is_empty()
            {
                let keys = if modifier {
                    largest.clone()
                } else {
                    self.held.iter().copied().chain([key]).collect()
                };
                self.capture = None;
                if !modifier {
                    self.swallowed.insert(key);
                }
                out.capture = Some(if key == ESCAPE {
                    None
                } else {
                    Some(
                        Hotkey::new(keys)
                            .expect("capture has modifiers and at most one nonzero main key"),
                    )
                });
            }
            out.swallow = true;
            return out;
        }
        let matches = self.hotkey.matches(&self.held);
        if self.swallowed.contains(&key) {
            if !down {
                self.swallowed.remove(&key);
                if self.combo_held && Some(key) == self.hotkey.main_key() {
                    self.combo_held = false;
                    if self.phase == Phase::Holding {
                        self.force_idle();
                        out.event = Some(HotkeyEvent::Release);
                    }
                }
            }
            out.swallow = true;
            return out;
        }
        if modifier {
            if self.hotkey.main_key().is_none() && down && matches && !self.combo_held {
                self.combo_held = true;
                out.mask_start_menu = self.hotkey.masks_menu();
                if self.phase == Phase::Locked {
                    self.force_idle();
                    out.event = Some(HotkeyEvent::StopLocked);
                } else if self.phase == Phase::Idle && !self.hotkey.has_extra_modifiers(&self.held)
                {
                    self.phase = Phase::Holding;
                    out.event = Some(HotkeyEvent::Start);
                }
            } else if !down && self.combo_held && !matches {
                self.combo_held = false;
                if self.phase == Phase::Holding {
                    self.force_idle();
                    out.event = Some(HotkeyEvent::Release);
                }
            }
        } else if down
            && !repeat
            && Some(key) == self.hotkey.main_key()
            && matches
            && !self.hotkey.has_extra_modifiers(&self.held)
        {
            self.combo_held = true;
            self.swallowed.insert(key);
            out.swallow = true;
            out.mask_start_menu = self.hotkey.masks_menu();
            if self.phase == Phase::Locked {
                self.force_idle();
                out.event = Some(HotkeyEvent::StopLocked);
            } else if self.phase == Phase::Idle {
                self.phase = Phase::Holding;
                out.event = Some(HotkeyEvent::Start);
            }
        } else if key == ESCAPE {
            if down && self.phase != Phase::Idle {
                self.force_idle();
                self.swallow_esc_up = true;
                out.swallow = true;
                out.event = Some(HotkeyEvent::Cancel);
            } else if !down && self.swallow_esc_up {
                self.swallow_esc_up = false;
                out.swallow = true;
            }
        } else if key == SPACE
            && if down {
                self.combo_held && self.phase == Phase::Holding
            } else {
                self.swallow_space_up
            }
        {
            if down {
                self.phase = Phase::Locked;
                self.swallow_space_up = true;
                out.event = Some(HotkeyEvent::Lock);
            } else {
                self.swallow_space_up = false;
            }
            out.swallow = true;
        } else if down && self.combo_held && self.phase == Phase::Holding {
            self.force_idle();
            out.event = Some(HotkeyEvent::CancelSilent);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: injected events mutate state; generic VKs miss; repeat
    // starts twice; swallowed main keys escape before physical release;
    // extra modifiers start recording; Space locks outside a held combo;
    // Esc leaks its key-up; unrelated keys fail to cancel silently; locked
    // release stops early; menu masking is omitted; capture loses its largest
    // modifier set, leaks keys, or returns a shortcut for Escape.
    use super::*;
    use crate::hotkey::*;

    fn down(machine: &mut HotkeyMachine, key: u8) -> Outcome {
        machine.process(KeyEvent {
            key,
            down: true,
            injected: false,
        })
    }
    fn up(machine: &mut HotkeyMachine, key: u8) -> Outcome {
        machine.process(KeyEvent {
            key,
            down: false,
            injected: false,
        })
    }

    #[test]
    fn hold_release_normalization_and_injection() {
        let mut m = HotkeyMachine::default();
        m.process(KeyEvent {
            key: LCTRL,
            down: true,
            injected: true,
        });
        assert_eq!(down(&mut m, LWIN).event, None);
        let start = down(&mut m, 0x11);
        assert_eq!(start.event, Some(HotkeyEvent::Start));
        assert!(start.mask_start_menu);
        assert_eq!(down(&mut m, LCTRL).event, None);
        assert_eq!(up(&mut m, LCTRL).event, Some(HotkeyEvent::Release));
    }

    #[test]
    fn locked_flow_and_space_ownership() {
        let mut m = HotkeyMachine::default();
        assert!(!down(&mut m, SPACE).swallow);
        up(&mut m, SPACE);
        down(&mut m, LCTRL);
        down(&mut m, LWIN);
        assert_eq!(down(&mut m, SPACE).event, Some(HotkeyEvent::Lock));
        assert!(up(&mut m, SPACE).swallow);
        assert_eq!(up(&mut m, LWIN).event, None);
        assert_eq!(down(&mut m, LWIN).event, Some(HotkeyEvent::StopLocked));
    }

    #[test]
    fn ownership_queries_follow_the_phase() {
        let mut m = HotkeyMachine::default();
        assert!(!m.dictating() && !m.lock_armed());
        down(&mut m, LCTRL);
        down(&mut m, LWIN);
        assert!(m.dictating() && m.lock_armed());
        down(&mut m, SPACE);
        assert!(m.dictating() && !m.lock_armed());
        down(&mut m, ESCAPE);
        assert!(!m.dictating());
        m.begin_capture();
        assert!(m.capturing());
        m.cancel_capture();
        assert!(!m.capturing());
    }

    #[test]
    fn cancellation_and_main_key_stays_owned() {
        let mut m = HotkeyMachine::new("Ctrl+D".parse().expect("chord"));
        down(&mut m, LCTRL);
        assert!(down(&mut m, b'D').swallow);
        assert_eq!(down(&mut m, b'A').event, Some(HotkeyEvent::CancelSilent));
        up(&mut m, LCTRL);
        assert!(down(&mut m, b'D').swallow);
        assert!(up(&mut m, b'D').swallow);
        assert!(!down(&mut m, b'D').swallow);
        up(&mut m, b'D');
        down(&mut m, LCTRL);
        down(&mut m, b'D');
        assert_eq!(down(&mut m, ESCAPE).event, Some(HotkeyEvent::Cancel));
        assert!(up(&mut m, ESCAPE).swallow);
        assert!(up(&mut m, b'D').swallow);
    }

    #[test]
    fn extra_modifiers_and_sided_shortcuts() {
        let mut m = HotkeyMachine::default();
        down(&mut m, LSHIFT);
        down(&mut m, LCTRL);
        assert_eq!(down(&mut m, LWIN).event, None);
        let mut m = HotkeyMachine::new("RightAlt".parse().expect("right alt"));
        assert_eq!(down(&mut m, LALT).event, None);
        up(&mut m, LALT);
        assert_eq!(down(&mut m, RALT).event, Some(HotkeyEvent::Start));
        assert_eq!(up(&mut m, RALT).event, Some(HotkeyEvent::Release));
    }

    #[test]
    fn capture_largest_chord_main_key_and_escape() {
        let mut m = HotkeyMachine::default();
        m.begin_capture();
        assert!(down(&mut m, RCTRL).mask_start_menu);
        down(&mut m, RSHIFT);
        up(&mut m, RSHIFT);
        let out = up(&mut m, RCTRL);
        assert!(out.swallow);
        assert_eq!(
            out.capture,
            Some(Some("Ctrl+Shift".parse().expect("chord")))
        );
        m.begin_capture();
        down(&mut m, RCTRL);
        assert_eq!(
            down(&mut m, b'D').capture,
            Some(Some("Ctrl+D".parse().expect("chord")))
        );
        assert!(up(&mut m, b'D').swallow);
        up(&mut m, RCTRL);
        m.begin_capture();
        assert_eq!(down(&mut m, ESCAPE).capture, Some(None));
        assert!(up(&mut m, ESCAPE).swallow);
    }
}
