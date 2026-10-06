//! Vermilion Gym's electric locks: two switches under its fifteen trash
//! cans, placed at random (`SetVermilionTrashCans`, `field_specials.c`):
//! the first under any can, the second under a can next to it on the 5×3
//! grid (left, right, above, below). A wrong second can resets both
//! ("The electric locks were reset!"). No plan can pick the cans ahead:
//! the game rolls them. The cans are searched: any can until the first
//! switch answers, then only its neighbours; a reset starts over (fleet
//! and Switch: the planned `TrashCan10` paths never opened the door, and
//! LT. SURGE stayed out of reach).

use std::collections::BTreeSet;

use pokebot_state::{GameEvent, PlayerPose};

use super::dialogue::{start_of, Start};
use super::talk::TalkStep;
use super::{progress, ToolContext, ToolError};

const PREFIX: &str = "VermilionCity_Gym_EventScript_TrashCan";
/// Set once both switches are found; the door stays open.
pub const FLAG: &str = "FLAG_FOUND_BOTH_VERMILION_GYM_SWITCHES";
/// Cans pressed before the search gives up. A search takes about 25 on
/// average (8 cans to the first switch, and a wrong neighbour, 1 in 2 to 3
/// in 4, starts it over), with a long tail: fleet worker 5 found the first
/// switch six times in 60 presses and missed its neighbour five.
const MAX_PRESSES: u32 = 200;

/// The can a trash-can script is for (1..=15).
pub fn can_of(script: &str) -> Option<u8> {
    script
        .strip_prefix(PREFIX)?
        .parse::<u8>()
        .ok()
        .filter(|n| (1..=15).contains(n))
}

/// Where the second switch can be when the first is under `first`: its
/// neighbours on the grid of five cans per row.
pub fn neighbours(first: u8) -> Vec<u8> {
    let (row, col) = ((first - 1) / 5, (first - 1) % 5);
    let mut out = Vec::new();
    if col > 0 {
        out.push(first - 1);
    }
    if col < 4 {
        out.push(first + 1);
    }
    if row > 0 {
        out.push(first - 5);
    }
    if row < 2 {
        out.push(first + 5);
    }
    out
}

/// What a can answered, from the texts recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Trash,
    First,
    Second,
    Reset,
    Unknown,
}

/// What a can answered, from the pages read (the game's texts: the reset
/// begins with the same page as plain trash, so it is checked first).
pub fn answer(pages: &[String]) -> Answer {
    let text = pages.join(" ").to_lowercase().replace('’', "'");
    if text.contains("second electric lock opened") {
        Answer::Second
    } else if text.contains("locks were reset") {
        Answer::Reset
    } else if text.contains("first electric lock opened") || text.contains("switch under the") {
        Answer::First
    } else if text.contains("only trash here") {
        Answer::Trash
    } else {
        Answer::Unknown
    }
}

/// The search's next can: the first switch's untried neighbours once it
/// is found, else any untried can; the nearest first.
pub fn next_can(
    first: Option<u8>,
    tried: &BTreeSet<u8>,
    at: impl Fn(u8) -> Option<(i32, i32)>,
    from: Option<(i32, i32)>,
) -> Option<u8> {
    let pool: Vec<u8> = match first {
        Some(f) => neighbours(f),
        None => (1..=15).collect(),
    };
    pool.into_iter()
        .filter(|c| !tried.contains(c))
        .min_by_key(|c| {
            let d = match (at(*c), from) {
                (Some((x, y)), Some((px, py))) => (x - px).abs() + (y - py).abs(),
                _ => 0,
            };
            (d, *c)
        })
}

/// Every one of the fifteen cans answered plain trash, no first switch
/// found: with the locks shut one of them hides it, so they are open.
fn locks_open(first: Option<u8>, tried: &BTreeSet<u8>) -> bool {
    first.is_none() && (1..=15).all(|c| tried.contains(&c))
}

fn press(ctx: &mut ToolContext<'_>, can: u8) -> Result<Answer, ToolError> {
    let script = format!("{PREFIX}{can}");
    let Some(Start::Sign { map, x, y, facing }) =
        start_of(&ctx.world, &script, ctx.pose().as_ref())
    else {
        return Err(ToolError::Failed(format!(
            "{script}: no trash can to press"
        )));
    };
    super::go::reach_facing(ctx, &map, (x, y))?;
    let mut step =
        TalkStep::toward(ctx, &map, (x, y), facing, Some(script), Vec::new()).with_scene();
    ctx.drive(&mut step)?;
    Ok(answer(&step.conversation().pages))
}

/// Presses cans until both locks are open.
pub fn solve(ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
    let world = std::sync::Arc::clone(&ctx.world);
    let at = |can: u8| {
        let map = world.map("VermilionCity_Gym")?;
        map.signs
            .iter()
            .find(|s| s.script.as_deref() == Some(format!("{PREFIX}{can}").as_str()))
            .map(|s| (s.x, s.y))
    };
    let mut first: Option<u8> = None;
    let mut tried = BTreeSet::new();
    for _ in 0..MAX_PRESSES {
        let from = ctx.pose().map(|p: PlayerPose| (p.x, p.y));
        let Some(can) = next_can(first, &tried, at, from) else {
            // Every can plain trash, and no first switch among them: the
            // locks are open already, every can answering as trash
            // (`LocksAlreadyOpen`; fleet continue-1, a save inside the gym
            // after they were opened: 200 cans pressed, "still locked",
            // the door open all along).
            if locks_open(first, &tried) {
                ctx.emit(progress("Gym", "every can is trash: the locks are open"))?;
                ctx.emit(GameEvent::FlagObserved {
                    flag: FLAG.into(),
                    value: true,
                })?;
                return Ok(());
            }
            // Every candidate tried without an answer that fits: start over.
            first = None;
            tried.clear();
            continue;
        };
        let got = press(ctx, can)?;
        ctx.emit(progress("Gym", format!("trash can {can}: {got:?}")))?;
        match got {
            Answer::Second => {
                ctx.emit(GameEvent::FlagTracked {
                    flag: FLAG.into(),
                    value: true,
                })?;
                return Ok(());
            }
            Answer::First => {
                first = Some(can);
                tried.clear();
                tried.insert(can);
            }
            Answer::Reset => {
                // Both switches moved: nothing learnt holds.
                first = None;
                tried.clear();
            }
            Answer::Trash => {
                tried.insert(can);
            }
            // Not read: the can is pressed again (ruled out unread, it could
            // have been the switch).
            Answer::Unknown => {}
        }
    }
    Err(ToolError::Failed(format!(
        "the electric locks: {MAX_PRESSES} cans pressed, still locked"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fleet continue-1: all fifteen cans trash, twice over, and no first
    /// switch: the locks were open. A sweep of every can without one says
    /// so; a sweep of a first switch's neighbours doesn't.
    #[test]
    fn every_can_trash_means_the_locks_are_open() {
        let all: BTreeSet<u8> = (1..=15).collect();
        assert!(locks_open(None, &all));
        let some: BTreeSet<u8> = (1..=14).collect();
        assert!(!locks_open(None, &some));
        assert!(!locks_open(Some(8), &all));
    }

    /// `SetVermilionTrashCans`: 1 → 2 or 6; 3 → 2, 4 or 8; 5 → 4 or 10;
    /// 8 → 3, 7, 9 or 13; 15 → 10 or 14.
    #[test]
    fn the_second_switch_is_next_to_the_first() {
        let sorted = |mut v: Vec<u8>| {
            v.sort_unstable();
            v
        };
        assert_eq!(sorted(neighbours(1)), vec![2, 6]);
        assert_eq!(sorted(neighbours(3)), vec![2, 4, 8]);
        assert_eq!(sorted(neighbours(5)), vec![4, 10]);
        assert_eq!(sorted(neighbours(8)), vec![3, 7, 9, 13]);
        assert_eq!(sorted(neighbours(15)), vec![10, 14]);
        assert_eq!(can_of("VermilionCity_Gym_EventScript_TrashCan10"), Some(10));
        assert_eq!(can_of("VermilionCity_Gym_EventScript_LtSurge"), None);
    }

    #[test]
    fn the_search_tries_neighbours_after_the_first_and_starts_over_on_a_reset() {
        let at = |c: u8| {
            Some((
                1 + 2 * i32::from((c - 1) % 5),
                10 + 2 * i32::from((c - 1) / 5),
            ))
        };
        let mut tried = BTreeSet::new();
        // From the entrance side (5, 16): the nearest untried can.
        let c = next_can(None, &tried, at, Some((5, 16))).unwrap();
        assert_eq!(c, 13);
        tried.insert(13);
        assert_ne!(next_can(None, &tried, at, Some((5, 15))), Some(13));
        // The first switch under 8: only 3, 7, 9, 13 are candidates.
        let tried = BTreeSet::from([8]);
        let c = next_can(Some(8), &tried, at, Some((5, 13))).unwrap();
        assert!([3, 7, 9, 13].contains(&c), "{c}");
        let all = BTreeSet::from([8, 3, 7, 9, 13]);
        assert_eq!(next_can(Some(8), &all, at, None), None);
        // As read (the emulator's first run: "There’s only trash here.").
        let pages = |p: &[&str]| p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(answer(&pages(&["There’s only trash here."])), Answer::Trash);
        assert_eq!(
            answer(&pages(&[
                "Nope! There’s only trash here.",
                "Hey! The electric locks were reset!"
            ])),
            Answer::Reset
        );
        assert_eq!(
            answer(&pages(&[
                "Hey! There’s a switch under the trash! Turn it on!",
                "The first electric lock opened!"
            ])),
            Answer::First
        );
        assert_eq!(
            answer(&pages(&[
                "The second electric lock opened! The motorized door opened!"
            ])),
            Answer::Second
        );
    }
}
