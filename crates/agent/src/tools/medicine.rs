//! One field medicine use, verified by the inventory and party it shows.
use super::menu::{open_start_menu, pick_row, Closer, MenuRow, Retries, SCREEN_FRAMES};
use super::{Expects, Intent, ProbeFact, StepContext, ToolContext, ToolError, ToolStep};
use crate::{Action, Decision, Expectation, Outcome};
use pokebot_core::Button;
use pokebot_gamedata::GameData;
use pokebot_state::Pocket;
use std::sync::Arc;

pub fn use_item(ctx: &mut ToolContext<'_>, item: &str) -> Result<(), ToolError> {
    let count = |ctx: &ToolContext<'_>| {
        ctx.state()
            .bag
            .pockets
            .get(&Pocket::Items)
            .and_then(|k| k.value.as_ref())
            .and_then(|items| items.iter().find(|(name, _)| name == item))
            .map_or(0, |(_, n)| *n)
    };
    let before_count = count(ctx);
    let before_hp = ctx
        .state()
        .party
        .value
        .as_ref()
        .and_then(|p| p.first())
        .and_then(|m| m.hp.value)
        .map(|hp| hp.0)
        .ok_or_else(|| ToolError::Failed("medicine needs observed HP".into()))?;
    if before_count == 0 {
        return Err(ToolError::Failed(
            "medicine is not in audited inventory".into(),
        ));
    }
    let mut step = UseMedicine {
        item: item.to_owned(),
        data: Arc::clone(&ctx.data),
        retries: Retries::default(),
        closer: Closer::default(),
        applied: None,
        seek: Default::default(),
    };
    ctx.drive(&mut step)?;
    let improved = |ctx: &ToolContext<'_>| {
        ctx.state()
            .party
            .value
            .as_ref()
            .and_then(|p| p.first())
            .and_then(|m| m.hp.value)
            .is_some_and(|hp| hp.0 > before_hp)
    };
    // The use passes the party menu (the lead's HP after it) and the ITEMS
    // pocket (the count after it) on its way out, and the sensor reads
    // both; a probe is only for what those screens didn't show.
    if count(ctx) + 1 != before_count {
        ctx.invoke(&Intent::Probe {
            fact: ProbeFact::Pocket {
                pocket: Pocket::Items,
            },
        })
        .result?;
    }
    if !improved(ctx) {
        ctx.invoke(&Intent::Probe {
            fact: ProbeFact::Party,
        })
        .result?;
    }
    if count(ctx) + 1 != before_count || !improved(ctx) {
        return Err(ToolError::Failed(
            "medicine use was not verified by inventory and HP".into(),
        ));
    }
    Ok(())
}
/// Frames after the A on the lead with the list still showing and no
/// message before it is pressed again.
const MEDICINE_RETRY_FRAMES: u64 = 30;

struct UseMedicine {
    item: String,
    data: Arc<GameData>,
    retries: Retries,
    closer: Closer,
    applied: Option<u64>,
    seek: crate::bag::Seek,
}
impl ToolStep for UseMedicine {
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
        if let Some(frame) = self.applied {
            let since = o.frame_id.saturating_sub(frame);
            // The A on the lead can land while the party screen still
            // fades in and do nothing (fleet worker 4 in Viridian Forest at
            // 4/23 HP: the screen stayed, then closed, the Potion unused).
            // Used, the game answers with its message and goes back to the
            // bag; still on the list without one, the A is pressed again.
            if since >= MEDICINE_RETRY_FRAMES
                && o.party_menu.as_ref().is_some_and(|p| p.selected == Some(0))
                && o.dialogue.is_none()
                && !self.retries.exhausted()
            {
                self.applied = Some(o.frame_id);
                return self.retries.act(
                    "use medicine on lead (again: the A did nothing)",
                    Button::A,
                    Expectation::InputsDone,
                    SCREEN_FRAMES,
                );
            }
            if since < 120 {
                return Decision::Wait("waiting for medicine result".into());
            }
            return self.closer.next(o, "medicine input applied; verify menus");
        }
        if self.retries.exhausted() {
            return Decision::Fail("medicine menu failed".into());
        }
        if let Some(party) = &o.party_menu {
            if party.selected != Some(0) {
                return self.retries.act(
                    "select lead for medicine",
                    Button::Up,
                    Expectation::PartySelected(0),
                    SCREEN_FRAMES,
                );
            }
            self.applied = Some(o.frame_id);
            return self.retries.act(
                "use medicine on lead",
                Button::A,
                Expectation::InputsDone,
                SCREEN_FRAMES,
            );
        }
        if let Some(bag) = &o.bag {
            if crate::bag::pocket_from_title(&bag.pocket) != Some(Pocket::Items) {
                return self.retries.act(
                    "ITEMS pocket",
                    Button::Left,
                    Expectation::BagPocket("ITEMS".into()),
                    SCREEN_FRAMES,
                );
            }
            return crate::bag::select_item(
                o,
                &self.data,
                &self.item,
                Expectation::PartyList,
                &mut self.seek,
            );
        }
        if let Some(menu) = &o.menu {
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
        open_start_menu(&mut self.retries, o, ctx.quiet_frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_state::{
        DialogueKind, DialogueObservation, GameState, Observation, Observed, PartyMenuObservation,
        Region, ScreenState,
    };

    fn run(step: &mut UseMedicine, o: &Observation) -> Decision {
        let state = GameState::default();
        let mut events = Vec::new();
        step.next(&mut StepContext {
            observation: o,
            state: &state,
            events: &mut events,
            quiet_frames: 0,
            frame: None,
            learned: &[],
        })
    }

    fn frame(id: u64, screen: ScreenState) -> Observation {
        Observation::bare(
            id,
            Observed {
                value: screen,
                detector: "test".into(),
            },
            Default::default(),
        )
    }

    fn party(id: u64) -> Observation {
        let mut o = frame(id, ScreenState::PartyMenu);
        o.party_menu = Some(PartyMenuObservation {
            count: 1,
            selected: Some(0),
            prompt: "Use on which POKéMON?".into(),
            ..PartyMenuObservation::default()
        });
        o
    }

    /// Fleet worker 4 in Viridian Forest at 4/23 HP: the A on the lead
    /// landed while the party screen faded in and did nothing; the screen
    /// stayed, then closed, the Potion unused. Still on the list with no
    /// message, the A is pressed again; once the game answers, it isn't.
    #[test]
    fn an_a_the_party_screen_ignored_is_pressed_again() {
        let Ok(data) = GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let mut step = UseMedicine {
            item: "ITEM_POTION".into(),
            data: Arc::new(data),
            retries: Retries::default(),
            closer: Closer::default(),
            applied: None,
            seek: Default::default(),
        };
        let label = |d: Decision| match d {
            Decision::Act(a) => a.label,
            Decision::Wait(w) => format!("wait: {w}"),
            _ => "other".into(),
        };
        assert_eq!(label(run(&mut step, &party(100))), "use medicine on lead");
        // Too soon to tell.
        assert!(matches!(run(&mut step, &party(110)), Decision::Wait(_)));
        assert!(label(run(&mut step, &party(140))).contains("again"));
        // The message came: nothing more.
        let mut used = party(180);
        used.dialogue = Some(DialogueObservation {
            kind: DialogueKind::MessageBox,
            region: Region::new(8, 119, 224, 34),
            waiting_for_input: true,
            arrow: None,
            stable_frames: 10,
            text_cells: vec![1; 4],
            lines: vec![
                "BULBASAUR's HP was restored".into(),
                "by 20 point(s).".into(),
            ],
            help: false,
        });
        assert!(matches!(run(&mut step, &used), Decision::Wait(_)));
    }
}
