//! The Start menu as the probes drive it: open it once the scene has
//! settled, put the ▶ on the row whose text reads what we want (the rows
//! are read with the font: `observation.menu_lines`), confirm it, and
//! afterwards close whatever is open with B until the overworld shows
//! again. Every input is checked against the next frames; a phase that
//! makes no progress after [`MAX_RETRIES`] inputs fails.

use pokebot_core::{Button, ControllerCommand};
use pokebot_state::{GameState, MenuObservation, Observation};

use super::SETTLE_FRAMES;
use crate::bag::fits;
use crate::{Action, Decision, Expectation};

/// Start menu rows are 15 px apart.
pub const START_MENU_PITCH: u32 = 15;
/// Frames to wait for each effect (before the executor's latency
/// allowance): menus open and close with fades, full screens take longer.
pub const MENU_FRAMES: u64 = 60;
pub const SCREEN_FRAMES: u64 = 120;
pub const CURSOR_FRAMES: u64 = 45;
/// Inputs in one phase whose effect didn't show before the phase fails.
pub const MAX_RETRIES: u32 = 8;
/// A wait this long (unreadable text, a screen that doesn't come) counts
/// as one retry.
const WAIT_RETRY_FRAMES: u64 = 60;
/// B presses to get back to the overworld: a full screen, its parent page
/// and the Start menu are three; the rest covers fades and bounces.
const MAX_CLOSE_PRESSES: u32 = 12;
/// Frames after a B on an unrecognised screen before the next one (the
/// screen fades out, and the overworld needs a moment to be located).
const CLOSE_GAP_FRAMES: u64 = 90;

/// The Start menu rows: `POKéDEX`, `POKéMON`, `BAG`, the player's name
/// (the Trainer Card), `SAVE`, `OPTION`, `EXIT`.
const KNOWN_ROWS: [&str; 6] = ["POKéDEX", "POKéMON", "BAG", "SAVE", "OPTION", "EXIT"];

/// Which Start menu row to pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuRow {
    /// The row whose text reads like this (`?` in the reading matches any
    /// character).
    Text(&'static str),
    /// The Trainer Card row: the player's name when known, else the row
    /// between BAG and SAVE.
    PlayerName,
}

impl MenuRow {
    /// Index of the row in `lines` (the menu's rows as read).
    pub fn find(&self, lines: &[String], state: &GameState) -> Option<usize> {
        match self {
            MenuRow::Text(text) => lines.iter().position(|l| fits(text, l)),
            MenuRow::PlayerName => {
                if let Some(name) = &state.progression.player_name.value {
                    if let Some(row) = lines.iter().position(|l| fits(name, l)) {
                        return Some(row);
                    }
                }
                let bag = lines.iter().position(|l| fits("BAG", l))?;
                let row = bag + 1;
                let line = lines.get(row)?;
                (!KNOWN_ROWS.iter().any(|k| fits(k, line))).then_some(row)
            }
        }
    }
}

impl std::fmt::Display for MenuRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MenuRow::Text(text) => write!(f, "{text}"),
            MenuRow::PlayerName => write!(f, "the trainer's name"),
        }
    }
}

/// Retry accounting per phase: inputs whose effect didn't show, and long
/// waits, use up [`MAX_RETRIES`]; entering another phase resets them.
#[derive(Debug, Default)]
pub struct Retries {
    phase: Option<&'static str>,
    count: u32,
    waiting_since: Option<u64>,
}

impl Retries {
    /// Notes the phase the step is in; a change resets the count.
    pub fn enter(&mut self, phase: &'static str) {
        if self.phase != Some(phase) {
            self.phase = Some(phase);
            self.count = 0;
            self.waiting_since = None;
        }
    }

    pub fn failed(&mut self) {
        self.count += 1;
    }

    pub fn exhausted(&self) -> bool {
        self.count > MAX_RETRIES
    }

    /// The current phase (for messages).
    pub fn phase(&self) -> &'static str {
        self.phase.unwrap_or("start")
    }

    /// Waits; every [`WAIT_RETRY_FRAMES`] of waiting is one retry.
    pub fn wait(&mut self, o: &Observation, reason: &str) -> Decision {
        let since = *self.waiting_since.get_or_insert(o.frame_id);
        if o.frame_id.saturating_sub(since) >= WAIT_RETRY_FRAMES {
            self.failed();
            self.waiting_since = Some(o.frame_id);
        }
        Decision::Wait(reason.to_owned())
    }

    /// Waits for an input to take effect (the console answers a press
    /// tens of frames later): not held against the phase, but the wait
    /// clock starts over so the next counted wait is measured afresh.
    pub fn wait_for_input(&mut self, reason: &str) -> Decision {
        self.waiting_since = None;
        Decision::Wait(reason.to_owned())
    }

    /// An input: waiting starts over.
    pub fn act(
        &mut self,
        label: impl Into<String>,
        button: Button,
        expect: Expectation,
        timeout: u64,
    ) -> Decision {
        self.waiting_since = None;
        Decision::Act(Action::new(
            label,
            vec![ControllerCommand::Press(button)],
            expect,
            timeout,
        ))
    }
}

/// Opens the Start menu from the overworld, once the player is located and
/// the scene has settled (Start during a scripted pause lands mid-cutscene).
pub fn open_start_menu(retries: &mut Retries, o: &Observation, quiet_frames: u32) -> Decision {
    if o.player.is_none() {
        return retries.wait(o, "locating before opening the Start menu");
    }
    if quiet_frames < SETTLE_FRAMES {
        return retries.wait(o, "letting the scene settle before the Start menu");
    }
    retries.act(
        "open the Start menu",
        Button::Start,
        Expectation::MenuOpen,
        MENU_FRAMES,
    )
}

/// Rows of the Start menu window (its interior is `rows × 15 px`).
pub fn start_menu_rows(menu: &MenuObservation) -> usize {
    ((menu.window.height + START_MENU_PITCH / 2) / START_MENU_PITCH) as usize
}

/// Moves the Start menu's ▶ to `row` (found by reading the rows) and
/// confirms it with A, expecting `expect` within `timeout` frames.
/// `Some(true)` is returned with the A press so the caller knows the row
/// was chosen.
pub fn pick_row(
    retries: &mut Retries,
    o: &Observation,
    menu: &MenuObservation,
    row: &MenuRow,
    state: &GameState,
    expect: Expectation,
    timeout: u64,
) -> (Decision, bool) {
    // Every row must be read: a missing one would shift the ▶'s row.
    if o.menu_lines.len() != start_menu_rows(menu) {
        return (retries.wait(o, "reading the Start menu"), false);
    }
    let Some(target) = row.find(&o.menu_lines, state) else {
        return (
            retries.wait(o, &format!("looking for {row} in the Start menu")),
            false,
        );
    };
    let cursor = (menu.cursor_y.saturating_sub(menu.window.y) / START_MENU_PITCH) as usize;
    if cursor == target {
        return (
            retries.act(
                format!("Start menu: choose {row}"),
                Button::A,
                expect,
                timeout,
            ),
            true,
        );
    }
    let up = cursor > target;
    let button = if up { Button::Up } else { Button::Down };
    (
        retries.act(
            format!("Start menu: {button:?} toward {row}"),
            button,
            Expectation::MenuCursorMoved {
                from_y: menu.cursor_y,
                up,
            },
            CURSOR_FRAMES,
        ),
        false,
    )
}

/// Something other than the overworld is showing.
pub fn screen_open(o: &Observation) -> bool {
    o.menu.is_some()
        || o.dialogue.is_some()
        || o.bag.is_some()
        || o.shop.is_some()
        || o.trainer_card.is_some()
        || o.fly_map.is_some()
        || o.pokedex_list.is_some()
        || o.pokedex_page
}

/// Closes everything with B until the player is located in the overworld
/// with nothing open. Unrecognised screens (the Pokédex's TABLE OF CONTENTS,
/// fades) get a B every [`CLOSE_GAP_FRAMES`].
#[derive(Debug, Default)]
pub struct Closer {
    presses: u32,
    last_press: Option<u64>,
}

impl Closer {
    pub fn next(&mut self, o: &Observation, done: &str) -> Decision {
        let open = screen_open(o);
        if !open && o.player.is_some() {
            return Decision::Done(done.to_owned());
        }
        if self.presses >= MAX_CLOSE_PRESSES {
            return Decision::Fail(format!(
                "still not in the overworld after {MAX_CLOSE_PRESSES} B presses"
            ));
        }
        let gap = self
            .last_press
            .map_or(u64::MAX, |f| o.frame_id.saturating_sub(f));
        if !open && gap < CLOSE_GAP_FRAMES {
            return Decision::Wait("closing: locating the player".into());
        }
        if open && o.menu.is_none() && o.dialogue.is_none() && gap < CLOSE_GAP_FRAMES / 2 {
            return Decision::Wait("closing: letting the screen fade".into());
        }
        self.presses += 1;
        self.last_press = Some(o.frame_id);
        let (label, expect) = if o.menu.is_some() {
            ("close the Start menu", Expectation::MenuClosed)
        } else {
            ("close the screen", Expectation::InputsDone)
        };
        Decision::Act(Action::new(
            label,
            vec![ControllerCommand::Press(Button::B)],
            expect,
            MENU_FRAMES,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn rows_are_found_by_their_text() {
        let state = GameState::default();
        let menu = lines(&["POKéDEX", "POKéMON", "BAG", "RED", "SAVE", "OPTION", "EXIT"]);
        assert_eq!(MenuRow::Text("POKéDEX").find(&menu, &state), Some(0));
        assert_eq!(MenuRow::Text("BAG").find(&menu, &state), Some(2));
        assert_eq!(MenuRow::Text("SAVE").find(&menu, &state), Some(4));
        assert_eq!(MenuRow::Text("TRAINER CARD").find(&menu, &state), None);
        // Doubtful glyphs still match.
        let doubtful = lines(&["POK?DEX", "POK?MON", "BAG", "R?D", "SAVE", "OPTION", "EXIT"]);
        assert_eq!(MenuRow::Text("POKéDEX").find(&doubtful, &state), Some(0));
    }

    #[test]
    fn the_trainer_card_row_is_the_name_between_bag_and_save() {
        let mut state = GameState::default();
        let menu = lines(&["POKéDEX", "POKéMON", "BAG", "RED", "SAVE", "OPTION", "EXIT"]);
        assert_eq!(MenuRow::PlayerName.find(&menu, &state), Some(3));
        // Before the Pokédex and the first Pokémon the menu is shorter.
        let early = lines(&["BAG", "RED", "SAVE", "OPTION", "EXIT"]);
        assert_eq!(MenuRow::PlayerName.find(&early, &state), Some(1));
        // A known name wins over the position.
        state.progression.player_name = pokebot_state::Knowledge::observed("GOLD".into(), 1);
        let menu = lines(&[
            "POKéDEX", "POKéMON", "BAG", "GOLD", "SAVE", "OPTION", "EXIT",
        ]);
        assert_eq!(MenuRow::PlayerName.find(&menu, &state), Some(3));
        // A menu without the name row (nothing between BAG and SAVE).
        let none = lines(&["BAG", "SAVE", "OPTION", "EXIT"]);
        assert_eq!(MenuRow::PlayerName.find(&none, &state), None);
    }

    #[test]
    fn retries_reset_on_phase_change_and_run_out() {
        let mut r = Retries::default();
        r.enter("open");
        for _ in 0..=MAX_RETRIES {
            r.failed();
        }
        assert!(r.exhausted());
        r.enter("menu");
        assert!(!r.exhausted());
        r.enter("menu");
        r.failed();
        assert!(!r.exhausted());
    }
}
