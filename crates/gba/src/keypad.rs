//! The GBA keypad: the ten buttons, their `KEYINPUT` state, and the `KEYCNT`
//! interrupt configuration.
//!
//! `KEYINPUT` is **active-low**: a set bit means the button is *released*. This
//! module stores the intuitive representation (a set bit = pressed) and inverts on
//! read, so callers press and release keys without thinking about the inversion.

/// A GBA button, numbered by its `KEYINPUT`/`KEYCNT` bit position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    A = 0,
    B = 1,
    Select = 2,
    Start = 3,
    Right = 4,
    Left = 5,
    Up = 6,
    Down = 7,
    /// Right shoulder.
    R = 8,
    /// Left shoulder.
    L = 9,
}

impl Key {
    /// The button's bit within `KEYINPUT`/`KEYCNT`.
    pub fn bit(self) -> u16 {
        1 << (self as u16)
    }

    /// Parse a case-insensitive button name (e.g. `"A"`, `"RIGHT"`), for the
    /// string-keyed API a debug protocol exposes.
    pub fn from_name(name: &str) -> Option<Key> {
        Some(match name.to_ascii_uppercase().as_str() {
            "A" => Key::A,
            "B" => Key::B,
            "SELECT" => Key::Select,
            "START" => Key::Start,
            "RIGHT" => Key::Right,
            "LEFT" => Key::Left,
            "UP" => Key::Up,
            "DOWN" => Key::Down,
            "R" => Key::R,
            "L" => Key::L,
            _ => return None,
        })
    }

    /// The canonical button name.
    pub fn name(self) -> &'static str {
        match self {
            Key::A => "A",
            Key::B => "B",
            Key::Select => "SELECT",
            Key::Start => "START",
            Key::Right => "RIGHT",
            Key::Left => "LEFT",
            Key::Up => "UP",
            Key::Down => "DOWN",
            Key::R => "R",
            Key::L => "L",
        }
    }

    /// All ten buttons, in bit order.
    pub const ALL: [Key; 10] = [
        Key::A,
        Key::B,
        Key::Select,
        Key::Start,
        Key::Right,
        Key::Left,
        Key::Up,
        Key::Down,
        Key::R,
        Key::L,
    ];
}

/// The ten button bits.
const KEY_MASK: u16 = 0x03FF;
/// `KEYCNT` bit 14: keypad interrupt enable.
const IRQ_ENABLE: u16 = 1 << 14;
/// `KEYCNT` bit 15: interrupt condition — set means AND (all selected keys), clear
/// means OR (any selected key).
const IRQ_AND: u16 = 1 << 15;

/// The keypad state and its interrupt configuration.
#[derive(Clone, Copy, Debug, Default)]
pub struct Keypad {
    /// Currently pressed buttons, a set bit meaning pressed (the inverse of what
    /// `KEYINPUT` reports).
    pressed: u16,
    keycnt: u16,
}

impl Keypad {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read `KEYINPUT` (`4000130h`): active-low, a set bit meaning released.
    pub fn read_input(&self) -> u16 {
        !self.pressed & KEY_MASK
    }

    /// Read `KEYCNT` (`4000132h`).
    pub fn read_control(&self) -> u16 {
        self.keycnt
    }

    /// Write `KEYCNT`.
    pub fn write_control(&mut self, value: u16) {
        self.keycnt = value;
    }

    /// Set a button's pressed state.
    pub fn set_key(&mut self, key: Key, pressed: bool) {
        if pressed {
            self.pressed |= key.bit();
        } else {
            self.pressed &= !key.bit();
        }
    }

    /// Whether a button is currently pressed.
    pub fn is_pressed(&self, key: Key) -> bool {
        self.pressed & key.bit() != 0
    }

    /// Whether the `KEYCNT` interrupt condition is currently satisfied.
    pub fn irq_condition_met(&self) -> bool {
        if self.keycnt & IRQ_ENABLE == 0 {
            return false;
        }
        let selected = self.keycnt & KEY_MASK;
        if selected == 0 {
            return false;
        }
        if self.keycnt & IRQ_AND != 0 {
            self.pressed & selected == selected // all selected pressed
        } else {
            self.pressed & selected != 0 // any selected pressed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyinput_is_active_low() {
        let mut pad = Keypad::new();
        // Nothing pressed: all button bits read as 1 (released).
        assert_eq!(pad.read_input(), KEY_MASK);
        pad.set_key(Key::A, true);
        // A pressed clears bit 0.
        assert_eq!(pad.read_input(), KEY_MASK & !1);
        assert!(pad.is_pressed(Key::A));
        pad.set_key(Key::A, false);
        assert_eq!(pad.read_input(), KEY_MASK);
    }

    #[test]
    fn name_round_trips() {
        for key in Key::ALL {
            assert_eq!(Key::from_name(key.name()), Some(key));
        }
        assert_eq!(Key::from_name("start"), Some(Key::Start));
        assert_eq!(Key::from_name("nope"), None);
    }

    #[test]
    fn irq_condition_or_and_and() {
        let mut pad = Keypad::new();
        // OR of {A, B}, IRQ enabled.
        pad.write_control(IRQ_ENABLE | Key::A.bit() | Key::B.bit());
        assert!(!pad.irq_condition_met());
        pad.set_key(Key::B, true);
        assert!(pad.irq_condition_met()); // any selected pressed

        // AND of {A, B}.
        pad.write_control(IRQ_ENABLE | IRQ_AND | Key::A.bit() | Key::B.bit());
        assert!(!pad.irq_condition_met()); // only B pressed
        pad.set_key(Key::A, true);
        assert!(pad.irq_condition_met()); // both pressed
    }

    #[test]
    fn irq_condition_needs_enable_bit() {
        let mut pad = Keypad::new();
        pad.write_control(Key::A.bit()); // selected but IRQ disabled
        pad.set_key(Key::A, true);
        assert!(!pad.irq_condition_met());
    }
}
