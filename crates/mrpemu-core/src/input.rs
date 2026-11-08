//! Feature-phone key model used by mythroad applications.

use std::collections::BTreeSet;

/// A physical key of a mythroad feature phone.
///
/// Numeric keys are separate variants so dispatch sites can match them
/// exhaustively without range checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

/// Current press state of every [`MrpKey`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputState {
    pressed: BTreeSet<MrpKey>,
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

    /// Releases every key.
    pub fn clear(&mut self) {
        self.pressed.clear();
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
