//! The keys on Windows, with no Windows in it so they can be tested anywhere. This is
//! the logic of the original app's keyboard hook (`windows/src/Input/HotkeyHook.cs`):
//!
//! ```text
//! hold Ctrl+Win                  -> record; let go to transcribe
//! press Space while holding them -> keep recording after letting go
//! Ctrl+Win while hands-free      -> finish
//! Esc while recording            -> throw it away, and Esc reaches nothing else
//! any other key while holding    -> it was a Windows shortcut, so get out of the way
//! ```

pub const VK_SHIFT: u32 = 0x10;
pub const VK_CONTROL: u32 = 0x11;
pub const VK_MENU: u32 = 0x12;
pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_SPACE: u32 = 0x20;
pub const VK_LWIN: u32 = 0x5B;
pub const VK_RWIN: u32 = 0x5C;
pub const VK_LSHIFT: u32 = 0xA0;
pub const VK_RSHIFT: u32 = 0xA1;
pub const VK_LCONTROL: u32 = 0xA2;
pub const VK_RCONTROL: u32 = 0xA3;
pub const VK_LMENU: u32 = 0xA4;
pub const VK_RMENU: u32 = 0xA5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fired {
    Start,
    Release,
    Lock,
    StopLocked,
    Cancel,
    CancelSilent,
}

/// What the hook has to do about one key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub fired: Option<Fired>,
    /// Nothing else on the system gets to see this key.
    pub swallow: bool,
    /// Windows would open the Start menu when the Win key comes up, so send it a key
    /// that means nothing first.
    pub mask_start_menu: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub vk: u32,
    pub down: bool,
    /// Sent by a program, including this one, so not somebody's finger.
    pub injected: bool,
    /// Whether Shift or Alt is held, which makes Ctrl+Win some other shortcut.
    pub other_modifier: bool,
}

#[derive(Debug, Default)]
pub struct Hotkeys {
    left_ctrl: bool,
    right_ctrl: bool,
    left_win: bool,
    right_win: bool,
    combo_held: bool,
    active: bool,
    locked: bool,
    swallow_space_up: bool,
    swallow_esc_up: bool,
}

impl Hotkeys {
    pub fn key(&mut self, key: Key) -> Outcome {
        let mut outcome = Outcome::default();
        if key.injected {
            return outcome;
        }
        let Key {
            vk,
            down,
            other_modifier,
            ..
        } = key;
        let is_ctrl_win = match vk {
            VK_LCONTROL | VK_CONTROL => {
                self.left_ctrl = down;
                true
            }
            VK_RCONTROL => {
                self.right_ctrl = down;
                true
            }
            VK_LWIN => {
                self.left_win = down;
                true
            }
            VK_RWIN => {
                self.right_win = down;
                true
            }
            _ => false,
        };
        let both = (self.left_ctrl || self.right_ctrl) && (self.left_win || self.right_win);

        if is_ctrl_win {
            if down && both && !self.combo_held {
                self.combo_held = true;
                outcome.mask_start_menu = true;
                if self.locked {
                    self.locked = false;
                    self.active = false;
                    outcome.fired = Some(Fired::StopLocked);
                } else if !self.active && !other_modifier {
                    self.active = true;
                    outcome.fired = Some(Fired::Start);
                }
            } else if !down && self.combo_held && !both {
                self.combo_held = false;
                if self.active && !self.locked {
                    self.active = false;
                    outcome.fired = Some(Fired::Release);
                }
            }
        } else if vk == VK_ESCAPE {
            if down && (self.active || self.locked) {
                self.active = false;
                self.locked = false;
                self.swallow_esc_up = true;
                outcome.swallow = true;
                outcome.fired = Some(Fired::Cancel);
            } else if !down && self.swallow_esc_up {
                self.swallow_esc_up = false;
                outcome.swallow = true;
            }
        } else if vk == VK_SPACE
            && (if down {
                self.combo_held && self.active && !self.locked
            } else {
                self.swallow_space_up
            })
        {
            if down {
                self.locked = true;
                self.swallow_space_up = true;
                outcome.fired = Some(Fired::Lock);
            } else {
                self.swallow_space_up = false;
            }
            outcome.swallow = true;
        } else if down && self.combo_held && self.active && !self.locked && !is_modifier(vk) {
            self.active = false;
            outcome.fired = Some(Fired::CancelSilent);
        }
        outcome
    }
}

fn is_modifier(vk: u32) -> bool {
    matches!(
        vk,
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_MENU | VK_LMENU | VK_RMENU
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: u32 = 0x56;
    const LEFT: u32 = 0x25;

    fn press(keys: &mut Hotkeys, vk: u32) -> Outcome {
        keys.key(Key {
            vk,
            down: true,
            injected: false,
            other_modifier: false,
        })
    }

    fn lift(keys: &mut Hotkeys, vk: u32) -> Outcome {
        keys.key(Key {
            vk,
            down: false,
            injected: false,
            other_modifier: false,
        })
    }

    fn hold_combo(keys: &mut Hotkeys) -> Outcome {
        press(keys, VK_LCONTROL);
        press(keys, VK_LWIN)
    }

    #[test]
    fn holding_and_letting_go_records_and_finishes() {
        let mut keys = Hotkeys::default();
        assert_eq!(press(&mut keys, VK_LCONTROL), Outcome::default());
        let down = press(&mut keys, VK_LWIN);
        assert_eq!(down.fired, Some(Fired::Start));
        assert!(down.mask_start_menu);
        assert!(!down.swallow);

        assert_eq!(lift(&mut keys, VK_LWIN).fired, Some(Fired::Release));
        assert_eq!(lift(&mut keys, VK_LCONTROL).fired, None);
    }

    #[test]
    fn either_side_and_either_order_count() {
        for (ctrl, win) in [
            (VK_LCONTROL, VK_RWIN),
            (VK_RCONTROL, VK_LWIN),
            (VK_RCONTROL, VK_RWIN),
        ] {
            let mut keys = Hotkeys::default();
            press(&mut keys, win);
            assert_eq!(press(&mut keys, ctrl).fired, Some(Fired::Start));
            assert_eq!(lift(&mut keys, ctrl).fired, Some(Fired::Release));
        }
    }

    #[test]
    fn key_repeat_does_not_start_again() {
        let mut keys = Hotkeys::default();
        assert_eq!(hold_combo(&mut keys).fired, Some(Fired::Start));
        assert_eq!(press(&mut keys, VK_LWIN), Outcome::default());
        assert_eq!(press(&mut keys, VK_LCONTROL), Outcome::default());
    }

    #[test]
    fn letting_go_of_one_of_two_ctrl_keys_keeps_it_going() {
        let mut keys = Hotkeys::default();
        hold_combo(&mut keys);
        press(&mut keys, VK_RCONTROL);
        assert_eq!(lift(&mut keys, VK_LCONTROL).fired, None);
        assert_eq!(lift(&mut keys, VK_RCONTROL).fired, Some(Fired::Release));
    }

    #[test]
    fn space_locks_and_is_hidden_from_everything_else_both_ways() {
        let mut keys = Hotkeys::default();
        hold_combo(&mut keys);
        let down = press(&mut keys, VK_SPACE);
        assert_eq!(down.fired, Some(Fired::Lock));
        assert!(down.swallow);

        // Repeats reach whatever has focus, as they do in the original, but never
        // count again. Only its release is held back.
        assert_eq!(press(&mut keys, VK_SPACE), Outcome::default());
        let up = lift(&mut keys, VK_SPACE);
        assert_eq!(up.fired, None);
        assert!(up.swallow);

        // Letting go doesn't end it.
        assert_eq!(lift(&mut keys, VK_LWIN).fired, None);
        assert_eq!(lift(&mut keys, VK_LCONTROL).fired, None);

        // The combination again does.
        press(&mut keys, VK_LCONTROL);
        let stop = press(&mut keys, VK_LWIN);
        assert_eq!(stop.fired, Some(Fired::StopLocked));
        assert!(stop.mask_start_menu);
        assert_eq!(lift(&mut keys, VK_LWIN).fired, None);
    }

    #[test]
    fn space_alone_is_only_space() {
        let mut keys = Hotkeys::default();
        assert_eq!(press(&mut keys, VK_SPACE), Outcome::default());
        assert_eq!(lift(&mut keys, VK_SPACE), Outcome::default());
    }

    #[test]
    fn esc_cancels_and_is_hidden_only_while_recording() {
        let mut keys = Hotkeys::default();
        assert_eq!(press(&mut keys, VK_ESCAPE), Outcome::default());
        assert_eq!(lift(&mut keys, VK_ESCAPE), Outcome::default());

        hold_combo(&mut keys);
        let cancel = press(&mut keys, VK_ESCAPE);
        assert_eq!(cancel.fired, Some(Fired::Cancel));
        assert!(cancel.swallow);
        let up = lift(&mut keys, VK_ESCAPE);
        assert_eq!(up.fired, None);
        assert!(up.swallow);
        // Nothing is left to end.
        assert_eq!(lift(&mut keys, VK_LWIN).fired, None);
    }

    #[test]
    fn esc_cancels_a_hands_free_recording() {
        let mut keys = Hotkeys::default();
        hold_combo(&mut keys);
        press(&mut keys, VK_SPACE);
        lift(&mut keys, VK_SPACE);
        lift(&mut keys, VK_LWIN);
        lift(&mut keys, VK_LCONTROL);
        assert_eq!(press(&mut keys, VK_ESCAPE).fired, Some(Fired::Cancel));
    }

    #[test]
    fn a_windows_shortcut_is_left_alone_and_cancels_silently() {
        let mut keys = Hotkeys::default();
        hold_combo(&mut keys);
        // Ctrl+Win+Left moves between virtual desktops.
        let other = press(&mut keys, LEFT);
        assert_eq!(other.fired, Some(Fired::CancelSilent));
        assert!(!other.swallow);
        assert_eq!(lift(&mut keys, LEFT), Outcome::default());
        assert_eq!(lift(&mut keys, VK_LWIN).fired, None);
    }

    #[test]
    fn shift_and_alt_do_not_cancel_a_hold() {
        let mut keys = Hotkeys::default();
        hold_combo(&mut keys);
        for vk in [VK_LSHIFT, VK_RSHIFT, VK_LMENU, VK_RMENU] {
            assert_eq!(press(&mut keys, vk), Outcome::default());
        }
        assert_eq!(lift(&mut keys, VK_LWIN).fired, Some(Fired::Release));
    }

    #[test]
    fn shift_or_alt_held_first_means_it_is_some_other_shortcut() {
        let mut keys = Hotkeys::default();
        press(&mut keys, VK_LCONTROL);
        let down = keys.key(Key {
            vk: VK_LWIN,
            down: true,
            injected: false,
            other_modifier: true,
        });
        assert_eq!(down.fired, None);
        // The Start menu is still kept out of it, as in the original.
        assert!(down.mask_start_menu);
        assert_eq!(lift(&mut keys, VK_LWIN).fired, None);
    }

    #[test]
    fn keys_a_program_sent_are_ignored_entirely() {
        let mut keys = Hotkeys::default();
        for vk in [VK_LCONTROL, VK_LWIN, V, VK_ESCAPE] {
            let out = keys.key(Key {
                vk,
                down: true,
                injected: true,
                other_modifier: false,
            });
            assert_eq!(out, Outcome::default());
        }
        // Our own paste during a recording changes nothing.
        hold_combo(&mut keys);
        keys.key(Key {
            vk: V,
            down: true,
            injected: true,
            other_modifier: false,
        });
        assert_eq!(lift(&mut keys, VK_LWIN).fired, Some(Fired::Release));
    }
}
