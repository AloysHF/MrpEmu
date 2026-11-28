//! Feature-phone key model used by mythroad applications.

use std::collections::BTreeSet;

/// A physical key of a mythroad feature phone.
///
/// Numeric keys are separate variants so dispatch sites can match them
/// exhaustively without range checks.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum MrpKey {
    Up,
    Down,
    Left,
    Right,
    /// Confirmation key (center of the D-pad).
    Ok,
    /// Left soft key, context-dependent menu action.
    LeftSoft,
    /// Right soft key, usually back/cancel.
    RightSoft,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    Star,
    Pound,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TouchState {
    pub x: u16,
    pub y: u16,
    pub pressed: bool,
}

/// Current keypad and touchscreen state.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputState {
    pressed: BTreeSet<MrpKey>,
    touch: TouchState,
}

impl InputState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks a key as held down.
    pub fn press(&mut self, key: MrpKey) {
        self.pressed.insert(key);
    }

    /// Marks a key as released.
    pub fn release(&mut self, key: MrpKey) {
        self.pressed.remove(&key);
    }

    /// Whether the key is currently held down.
    pub fn is_pressed(&self, key: MrpKey) -> bool {
        self.pressed.contains(&key)
    }

    /// All currently held keys in deterministic (sorted) order.
    pub fn pressed(&self) -> impl Iterator<Item = MrpKey> + '_ {
        self.pressed.iter().copied()
    }

    /// Sets the touch position in guest screen pixels and its contact state.
    /// Supply the final position with `pressed = false` when releasing.
    pub fn set_touch(&mut self, x: u16, y: u16, pressed: bool) {
        self.touch = TouchState { x, y, pressed };
    }

    /// Releases the touch at its last known position.
    pub fn release_touch(&mut self) {
        self.touch.pressed = false;
    }

    pub(crate) fn touch(&self) -> TouchState {
        self.touch
    }

    /// Releases every key and any active touch.
    pub fn clear(&mut self) {
        self.pressed.clear();
        self.release_touch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn press_and_release_track_state() {
        let mut input = InputState::new();

        assert!(!input.is_pressed(MrpKey::Ok));
        input.press(MrpKey::Ok);
        input.press(MrpKey::LeftSoft);
        assert!(input.is_pressed(MrpKey::Ok));
        assert_eq!(
            input.pressed().collect::<Vec<_>>(),
            [MrpKey::Ok, MrpKey::LeftSoft]
        );

        input.release(MrpKey::Ok);
        assert!(!input.is_pressed(MrpKey::Ok));
        input.clear();
        assert_eq!(input.pressed().count(), 0);
    }
}
