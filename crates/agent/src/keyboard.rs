//! The naming-screen keyboard (upper-case page) as static game knowledge.

use std::time::Duration;

use pokebot_core::{Button, ButtonSet, ControllerCommand, TimedInput};

/// FireRed's upper-case keyboard, 8 columns × 4 rows.
const LAYOUT: [&str; 4] = ["ABCDEF .", "GHIJKL ,", "MNOPQRS ", "TUVWXYZ "];

pub const MAX_NAME_LENGTH: usize = 7;

/// Column and row of `c` on the upper-case page.
pub fn key_position(c: char) -> Option<(u8, u8)> {
    LAYOUT.iter().enumerate().find_map(|(row, line)| {
        line.chars()
            .position(|k| k == c && c != ' ')
            .map(|column| (column as u8, row as u8))
    })
}

/// Names the bot can type and verify: 1–7 upper-case letters.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().count() > MAX_NAME_LENGTH {
        return Err(format!(
            "name {name:?} must be 1–{MAX_NAME_LENGTH} characters"
        ));
    }
    if let Some(c) = name.chars().find(|c| !c.is_ascii_uppercase()) {
        return Err(format!(
            "name {name:?}: only A–Z are supported (found {c:?})"
        ));
    }
    Ok(())
}

/// D-pad taps that move the cursor from one key to another without leaving
/// the letter grid.
pub fn moves(from: (u8, u8), to: (u8, u8)) -> Vec<Button> {
    let (dx, dy) = (
        i16::from(to.0) - i16::from(from.0),
        i16::from(to.1) - i16::from(from.1),
    );
    let horizontal = if dx > 0 { Button::Right } else { Button::Left };
    let vertical = if dy > 0 { Button::Down } else { Button::Up };
    std::iter::repeat_n(vertical, dy.unsigned_abs() as usize)
        .chain(std::iter::repeat_n(horizontal, dx.unsigned_abs() as usize))
        .collect()
}

/// Taps as one command: each tap is held then released so every one counts.
pub fn taps(buttons: &[Button]) -> ControllerCommand {
    let hold = Duration::from_millis(67);
    ControllerCommand::Sequence(
        buttons
            .iter()
            .flat_map(|b| {
                [
                    TimedInput {
                        buttons: (*b).into(),
                        duration: hold,
                    },
                    TimedInput {
                        buttons: ButtonSet::NONE,
                        duration: hold,
                    },
                ]
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_positions() {
        assert_eq!(key_position('A'), Some((0, 0)));
        assert_eq!(key_position('S'), Some((6, 2)));
        assert_eq!(key_position('Z'), Some((6, 3)));
        assert_eq!(key_position(' '), None);
        assert_eq!(key_position('a'), None);
    }

    #[test]
    fn moves_stay_in_grid() {
        assert_eq!(
            moves((0, 0), (2, 1)),
            vec![Button::Down, Button::Right, Button::Right]
        );
        assert_eq!(moves((6, 3), (0, 0)).len(), 9);
        assert!(moves((3, 2), (3, 2)).is_empty());
    }

    #[test]
    fn validates_names() {
        assert!(validate_name("RED").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("TOOLONGX").is_err());
        assert!(validate_name("Red").is_err());
    }
}
