//! Out of a cave or building to the escape warp (`pokebot_world::escape`):
//! Dig from the party menu (Start → POKéMON → the member → DIG → YES to
//! "Want to escape from here and return to …?"), or an Escape Rope from
//! the ITEMS pocket (USE; the bag closes on "… used the ESCAPE ROPE.").
//! Either way the screen fades and the player stands outside the entrance
//! last taken from outdoors: perception is told so once the old map is
//! gone, as for a flight.

use std::sync::Arc;

use pokebot_core::Button;
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, PlayerPose, Pocket, ScreenState};
use pokebot_world::route::{EdgeKind, ITEM_ESCAPE_ROPE};

use super::field::{carrier, FieldMove, PartyFieldMove};
use super::menu::{open_start_menu, pick_row, Closer, MenuRow, Retries, SCREEN_FRAMES};
use super::{progress, Expects, StepContext, ToolContext, ToolError, ToolStep};
use crate::{Action, Decision, Expectation, Outcome};

/// Frames with the player unlocated after the escape's fade before
/// perception is told where the player landed. The cut-in before it hides
/// the player too (emulator: DIG's Pokémon showed for 200 frames on
/// Mt. Moon B2F before the fade to Route 4) …
const LANDING_HINT_FRAMES: u64 = 45;
/// … or, with no fade seen (it played before the landing was watched),
/// this many.
const LANDING_UNSEEN_FADE_FRAMES: u64 = 240;
/// Frames the landing may take before the escape counts as failed (the
/// dig or rope animation, two fades).
const LANDING_MAX_FRAMES: u64 = 900;

/// Where the belief says an escape leads, as a pose.
fn escape_pose(ctx: &ToolContext<'_>) -> Option<PlayerPose> {
    let e = ctx.state().world.escape.value.as_ref()?;
    Some(PlayerPose {
        map: e.map.clone(),
        x: e.x,
        y: e.y,
    })
}

/// Dig out to where the belief says the escape leads (the `FieldMove`
/// intent's DIG).
pub fn dig(ctx: &mut ToolContext<'_>) -> Result<(), ToolError> {
    let to = escape_pose(ctx).ok_or_else(|| {
        ToolError::Failed("DIG: where it leads is unknown (no entrance seen taken)".into())
    })?;
    escape(ctx, &EdgeKind::Dig, &to)
}

/// Leaves by `kind` ([`EdgeKind::Dig`] or [`EdgeKind::EscapeRope`]) and
/// lands at `to`.
pub fn escape(
    ctx: &mut ToolContext<'_>,
    kind: &EdgeKind,
    to: &PlayerPose,
) -> Result<(), ToolError> {
    let from = ctx.pose().map(|p| p.map);
    match kind {
        EdgeKind::Dig => {
            let slot = carrier(ctx.state(), FieldMove::Dig.move_id())
                .ok_or_else(|| ToolError::Failed("no party member is known to know DIG".into()))?;
            let mut step = PartyFieldMove::new(FieldMove::Dig, slot);
            // The cut-in and the fade leave frames nobody is located on.
            step.unlocated_ok = true;
            ctx.drive(&mut step)?;
        }
        EdgeKind::EscapeRope => {
            let mut step = UseEscapeRope {
                data: Arc::clone(&ctx.data),
                retries: Retries::default(),
                closer: Closer::default(),
                used: false,
                refused: None,
                seek: Default::default(),
            };
            ctx.drive(&mut step)?;
        }
        other => return Err(ToolError::Failed(format!("{other} is no escape"))),
    }
    let mut landing = Landing::new(to.clone(), from);
    ctx.drive(&mut landing)?;
    if *kind == EdgeKind::EscapeRope {
        ctx.emit(GameEvent::ItemsChanged {
            pocket: Pocket::Items,
            item: ITEM_ESCAPE_ROPE.into(),
            delta: -1,
            reason: "used to escape".into(),
        })?;
    }
    ctx.emit(progress("FieldMove", format!("{kind} out to {to}")))?;
    Ok(())
}

/// Start → BAG → ITEMS → ESCAPE ROPE → USE, and the page that follows.
struct UseEscapeRope {
    data: Arc<GameData>,
    retries: Retries,
    closer: Closer,
    /// USE was pressed.
    used: bool,
    /// What the game said instead of using it.
    refused: Option<String>,
    seek: crate::bag::Seek,
}

impl ToolStep for UseEscapeRope {
    fn expects(&self) -> Expects {
        Expects::MENUS
    }

    fn on_outcome(&mut self, _: &Action, outcome: Outcome, _: &mut StepContext<'_>) {
        if outcome != Outcome::Confirmed {
            self.retries.failed();
        }
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if self.retries.exhausted() {
            return Decision::Fail(format!(
                "ESCAPE ROPE: no progress in {}",
                self.retries.phase()
            ));
        }
        if let Some(d) = &o.dialogue {
            let page = d.lines.join(" ").replace('’', "'");
            // "OAK: RED! This isn't the time to use that!" in a town.
            if page.contains("time to use") || page.contains("Can't use") {
                self.refused = Some(page);
            }
            return crate::new_game::advance_or_wait(Some(d), "reading");
        }
        if let Some(why) = &self.refused {
            if o.bag.is_some() || o.menu.is_some() {
                return self.closer.next(o, "closed");
            }
            return Decision::Fail(format!("ESCAPE ROPE: {why}"));
        }
        if self.used {
            self.retries.enter("after");
            if o.bag.is_some() || o.menu.is_some() {
                return self.retries.wait(o, "waiting for the bag to close");
            }
            return Decision::Done("ESCAPE ROPE used".into());
        }
        if let Some(bag) = &o.bag {
            self.retries.enter("bag");
            if crate::bag::pocket_from_title(&bag.pocket) != Some(Pocket::Items) {
                return self.retries.act(
                    "ITEMS pocket",
                    Button::Left,
                    Expectation::BagPocket("ITEMS".into()),
                    SCREEN_FRAMES,
                );
            }
            if let Some((options, cursor)) = &bag.prompt {
                if options
                    .get(usize::from(*cursor))
                    .is_some_and(|s| s == "USE")
                {
                    self.used = true;
                }
            }
            return crate::bag::select_item(
                o,
                &self.data,
                ITEM_ESCAPE_ROPE,
                Expectation::InputsDone,
                &mut self.seek,
            );
        }
        if let Some(menu) = &o.menu {
            self.retries.enter("start menu");
            return pick_row(
                &mut self.retries,
                o,
                menu,
                &MenuRow::Text("BAG"),
                ctx.state,
                Expectation::BagPocket(String::new()),
                SCREEN_FRAMES,
            )
            .0;
        }
        self.retries.enter("open");
        open_start_menu(&mut self.retries, o, ctx.quiet_frames)
    }
}

/// Waits for the player to be located on the escape's map; tells
/// perception where once no frame shows the player for a while (the
/// tracker still looks at the cave).
struct Landing {
    to: PlayerPose,
    /// The map left: seen there long after the escape, it didn't happen.
    from: Option<String>,
    start: Option<u64>,
    unlocated_since: Option<u64>,
    /// A fade was seen since the landing has been watched.
    faded: bool,
    hinted: bool,
}

impl Landing {
    fn new(to: PlayerPose, from: Option<String>) -> Self {
        Self {
            to,
            from,
            start: None,
            unlocated_since: None,
            faded: false,
            hinted: false,
        }
    }
}

impl ToolStep for Landing {
    fn expects(&self) -> Expects {
        Expects::MENUS
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        let start = *self.start.get_or_insert(o.frame_id);
        if let Some(p) = &o.player {
            if p.pose.map == self.to.map {
                return Decision::Done(format!("landed on {}", p.pose));
            }
            self.unlocated_since = None;
        } else if o.screen.value != ScreenState::Transition
            && o.dialogue.is_none()
            && o.menu.is_none()
        {
            let since = *self.unlocated_since.get_or_insert(o.frame_id);
            let wait = if self.faded {
                LANDING_HINT_FRAMES
            } else {
                LANDING_UNSEEN_FADE_FRAMES
            };
            if !self.hinted && o.frame_id.saturating_sub(since) >= wait {
                self.hinted = true;
                ctx.events.push(GameEvent::PlayerInferred {
                    pose: self.to.clone(),
                    candidates: Vec::new(),
                });
            }
        } else {
            self.faded |= o.screen.value == ScreenState::Transition;
            self.unlocated_since = None;
        }
        if let Some(d) = &o.dialogue {
            return crate::new_game::advance_or_wait(Some(d), "reading");
        }
        if o.frame_id.saturating_sub(start) > LANDING_MAX_FRAMES {
            let still = o.player.as_ref().map(|p| p.pose.map.clone());
            return Decision::Fail(match (still, &self.from) {
                (Some(m), Some(from)) if m == *from => {
                    format!("still on {from}: the escape didn't take")
                }
                _ => format!("not located on {} after the escape", self.to.map),
            });
        }
        Decision::Wait(format!("escaping to {}", self.to.map))
    }

    fn on_outcome(&mut self, _: &Action, _: Outcome, _: &mut StepContext<'_>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_state::{GameState, Observation, Observed, PoseObservation};

    fn frame(id: u64, screen: ScreenState, at: Option<PlayerPose>) -> Observation {
        let mut o = Observation::bare(
            id,
            Observed {
                value: screen,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.player = at.map(|pose| PoseObservation { pose, score: 900 });
        o
    }

    fn pose(map: &str, x: i32, y: i32) -> PlayerPose {
        PlayerPose {
            map: map.into(),
            x,
            y,
        }
    }

    fn run(step: &mut Landing, o: &Observation, events: &mut Vec<GameEvent>) -> Decision {
        let state = GameState::default();
        step.next(&mut StepContext {
            observation: o,
            state: &state,
            events,
            quiet_frames: 0,
            frame: None,
            learned: &[],
        })
    }

    /// Dug out of Mt. Moon (emulator): the cut-in hides the player for 200
    /// frames, then the fade; the tracker, still on the cave's floors,
    /// finds nobody after it. Perception is told the player is outside the
    /// mouth only then, and the landing is done once it tracks them there.
    #[test]
    fn a_landing_nobody_tracks_is_hinted_after_the_fade() {
        let to = pose("Route4", 19, 6);
        let mut step = Landing::new(to.clone(), Some("MtMoon_B2F".into()));
        let mut events = Vec::new();
        let mut see = |id, screen, at: Option<PlayerPose>| {
            let mut out = Vec::new();
            let d = run(&mut step, &frame(id, screen, at), &mut out);
            events.extend(out);
            (d, events.len())
        };
        let cave = Some(pose("MtMoon_B2F", 17, 30));
        assert!(matches!(
            see(100, ScreenState::Overworld, cave).0,
            Decision::Wait(_)
        ));
        // The cut-in: nobody located, but no fade yet.
        see(110, ScreenState::Overworld, None);
        see(300, ScreenState::Overworld, None);
        see(310, ScreenState::Transition, None);
        assert_eq!(see(340, ScreenState::Overworld, None).1, 0);
        assert_eq!(see(390, ScreenState::Overworld, None).1, 1);
        let (landed, _) = see(400, ScreenState::Overworld, Some(to.clone()));
        assert!(matches!(landed, Decision::Done(_)));
        assert_eq!(
            events,
            [GameEvent::PlayerInferred {
                pose: to,
                candidates: Vec::new(),
            }]
        );
    }

    #[test]
    fn an_escape_that_left_the_player_in_the_cave_fails() {
        let mut step = Landing::new(pose("Route4", 19, 6), Some("MtMoon_B2F".into()));
        let mut events = Vec::new();
        let cave = || Some(pose("MtMoon_B2F", 17, 30));
        run(
            &mut step,
            &frame(100, ScreenState::Overworld, cave()),
            &mut events,
        );
        match run(
            &mut step,
            &frame(1100, ScreenState::Overworld, cave()),
            &mut events,
        ) {
            Decision::Fail(why) => assert!(why.contains("didn't take"), "{why}"),
            _ => panic!("the escape should fail"),
        }
        assert!(events.is_empty());
    }
}
