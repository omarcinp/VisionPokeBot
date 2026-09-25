//! `Battle`: play the battle on screen to its end. Wild: the catch policy
//! decides catch, fight or flee; trainer: fight. New moves, evolution, the
//! battle bag during a throw and the Pokédex page after a catch are all
//! handled here, as in `StoryTask` (whose battle branch this copies; to be
//! removed there once the story runs on tools).

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, Observation, Status};

use super::{BattlePlan, Expects, Intent, StepContext, Tool, ToolContext, ToolOutcome, ToolStep};
use crate::battle::{self, BattleMemory, BattlePolicy};
use crate::catch;
use crate::learn::MoveLearning;
use crate::new_game::{advance_or_wait, select};
use crate::party::{self, Party};
use crate::track::TextTracker;
use crate::{Action, Decision, Expectation, Outcome};

/// Frames a page's text must stay unchanged to count as fully printed.
const PAGE_PRINTED_FRAMES: u32 = 8;
/// Longest wait for a page waiting for A to read the same on a second
/// frame.
const PAGE_READ_WAIT_FRAMES: u64 = 10;

pub struct BattleTool;

/// One battle, from its first frame to the overworld.
pub struct BattleStep {
    data: Arc<GameData>,
    plan: BattlePlan,
    party: Party,
    policy: BattlePolicy,
    pub memory: BattleMemory,
    learning: MoveLearning,
    tracker: TextTracker,
    lead_status: Option<Status>,
    /// A trainer's battle (dialogue just before it).
    trainer: bool,
    started: bool,
    in_battle: bool,
    battle_text_frame: Option<u64>,
    unread_since: Option<u64>,
    /// Trainers the moves are valued against when learning.
    upcoming: Vec<String>,
}

impl BattleStep {
    /// `trainer`: the battle follows dialogue (a trainer's).
    pub fn new(data: Arc<GameData>, plan: BattlePlan, trainer: bool) -> Self {
        Self {
            data,
            plan,
            party: Party::default(),
            policy: BattlePolicy::default(),
            memory: BattleMemory::default(),
            learning: MoveLearning::default(),
            tracker: TextTracker::default(),
            lead_status: None,
            trainer,
            started: false,
            in_battle: false,
            battle_text_frame: None,
            unread_since: None,
            upcoming: Vec::new(),
        }
    }

    /// The species caught in this battle, if one was.
    pub fn caught(&self) -> Option<&str> {
        self.memory.catch.caught.as_deref()
    }

    fn begin(&mut self, events: &mut Vec<GameEvent>) {
        if self.in_battle {
            return;
        }
        self.in_battle = true;
        self.started = true;
        self.memory = BattleMemory {
            trainer: self.trainer,
            ..BattleMemory::default()
        };
        match self.plan {
            BattlePlan::Auto => {}
            // No identification, no catch: fight (or flee) on the HUD.
            BattlePlan::Fight => self.memory.catch.decided = true,
            BattlePlan::Flee => {
                self.memory.catch.decided = true;
                self.memory.catch.flee = true;
            }
        }
        events.push(GameEvent::BattleStarted);
    }

    /// Battle text to the battle's readers, once per frame.
    fn observe_battle_text(&mut self, o: &Observation, events: &mut Vec<GameEvent>) {
        if o.battle.is_none() || self.battle_text_frame == Some(o.frame_id) {
            return;
        }
        self.battle_text_frame = Some(o.frame_id);
        if let Some(page) = catch::observe(&mut self.memory, o) {
            battle::observe_page(&mut self.memory, &page, &self.party, &self.data);
            if let Some(status) = self
                .party
                .lead()
                .and_then(|lead| battle::lead_status_text(&page, &lead.display_name()))
                .filter(|s| self.lead_status != Some(*s))
            {
                self.lead_status = Some(status);
                events.push(GameEvent::PartyObserved {
                    slot: 0,
                    species: None,
                    nickname: None,
                    level: None,
                    hp: None,
                    status: Some(status),
                    held_item: None,
                });
            }
        }
    }
}

impl ToolStep for BattleStep {
    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        self.party = Party::from_state(ctx.state);
        self.lead_status = ctx
            .state
            .party
            .value
            .as_ref()
            .and_then(|p| p.first())
            .and_then(|m| m.status.value);
        let o = ctx.observation;
        let data = Arc::clone(&self.data);

        // New moves and evolution: every finished page is read, and the
        // KNOWN MOVES list and questions about moves are answered.
        if let Some(d) = o.dialogue.as_ref().filter(|d| {
            d.ready_for_a() || o.menu.is_some() || d.stable_frames >= PAGE_PRINTED_FRAMES
        }) {
            let (events, log) =
                self.learning
                    .observe_page(&d.lines, &data, &self.party, &self.upcoming);
            ctx.events.extend(events);
            for detail in log {
                ctx.events.push(super::progress("Party", detail));
            }
            let tracked = self
                .tracker
                .observe_page(&d.lines, &data, self.memory.last_slot);
            ctx.events.extend(tracked);
            if d.ready_for_a() && o.menu.is_none() && !self.tracker.applied(&d.lines) {
                let since = *self.unread_since.get_or_insert(o.frame_id);
                if o.frame_id.saturating_sub(since) < PAGE_READ_WAIT_FRAMES {
                    if o.battle.is_some() {
                        self.begin(ctx.events);
                        self.observe_battle_text(o, ctx.events);
                    }
                    return Decision::Wait("reading the page on a second frame".into());
                }
                self.unread_since = None;
            } else {
                self.unread_since = None;
            }
        }
        if let Some(list) = &o.move_list {
            let (decision, events) =
                self.learning
                    .on_move_list(list, &data, &self.party, &self.upcoming);
            ctx.events.extend(events);
            return decision;
        }
        if let (Some(d), Some(menu)) = (&o.dialogue, &o.menu) {
            if let Some(yes) = self.learning.answer(&d.lines) {
                return select(
                    menu,
                    u8::from(!yes),
                    if yes { "learning: YES" } else { "learning: NO" },
                );
            }
        }

        if let Some(b) = &o.battle {
            self.begin(ctx.events);
            ctx.events
                .extend(party::battle_events(&data, &self.party, b));
            if self.plan == BattlePlan::Auto {
                catch::identify(
                    o,
                    &data,
                    ctx.state,
                    &self.party,
                    &mut self.memory,
                    ctx.events,
                );
            }
            self.observe_battle_text(o, ctx.events);
            if b.player_hp_numbers.is_some_and(|(hp, _)| hp == 0) {
                return Decision::Fail(format!(
                    "our Pokémon fainted ({})",
                    b.player_name.clone().unwrap_or_default()
                ));
            }
            if let Some(decision) = battle::decide(
                o,
                &self.policy,
                &mut self.memory,
                &self.party,
                &data,
                ctx.events,
            ) {
                return decision;
            }
            if let (Some(d), Some(_)) = (&o.dialogue, &o.menu) {
                // The box may belong to the page before (the nickname
                // question's YES/NO stays while the next page prints):
                // read the question once it is printed (flash-5: "It"
                // of "It was placed in BOX…" taken for an unknown one).
                if !d.ready_for_a() && d.stable_frames < PAGE_PRINTED_FRAMES {
                    return Decision::Wait("the question is printing".into());
                }
                let page = d.lines.join(" ");
                // After a catch: "Give a nickname to the captured X?" → No.
                if catch::is_nickname_question(&page) {
                    return Decision::Act(Action::new(
                        "nickname: NO",
                        vec![ControllerCommand::Press(Button::B)],
                        Expectation::MenuClosed,
                        90,
                    ));
                }
                // "Will RED change POKéMON?" → No (the lead fights).
                if battle::is_switch_question(&page) {
                    return Decision::Act(Action::new(
                        "change Pokémon: NO",
                        vec![ControllerCommand::Press(Button::B)],
                        Expectation::MenuClosed,
                        90,
                    ));
                }
                // The PC transfer text after NO to the nickname, with the
                // YES/NO box still drawn: plain text to advance.
                if catch::is_pc_transfer_text(&page) {
                    return advance_or_wait(o.dialogue.as_ref(), "PC transfer text");
                }
                // A is YES: never answer a question we don't understand.
                return Decision::Fail(format!("unexpected question in battle: {:?}", d.lines));
            }
            if o.dialogue.is_some() {
                return advance_or_wait(o.dialogue.as_ref(), "battle text");
            }
            return Decision::Wait("battle animation".into());
        }
        if self.in_battle && o.player.is_some() {
            self.in_battle = false;
            ctx.events.push(GameEvent::BattleEnded);
            ctx.events
                .extend(self.memory.catch.after_battle(&data, ctx.state));
            return Decision::Done(match self.caught() {
                Some(species) => format!("battle over: caught {species}"),
                None => "battle over".into(),
            });
        }
        // Battle screens without the battle HUD: the Pokédex page after a
        // first catch, and the battle bag during a throw.
        if self.in_battle {
            if let Some(decision) = catch::dismiss_pokedex(&mut self.memory.catch, o) {
                return decision;
            }
            if o.bag.is_some() || self.memory.catch.thrower.is_some() {
                return catch::in_bag(o, &data, &mut self.memory.catch, ctx.events);
            }
            if o.dialogue.is_some() {
                return advance_or_wait(o.dialogue.as_ref(), "battle text");
            }
            return Decision::Wait("battle screen".into());
        }
        if !self.started {
            return Decision::Wait("waiting for the battle".into());
        }
        Decision::Wait("battle ending".into())
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut StepContext<'_>) {
        if action.label == "choose RUN" && outcome == Outcome::Confirmed {
            self.memory.run_attempts += 1;
        }
        if action.label.starts_with("choose move") && outcome == Outcome::Confirmed {
            if let Some((slot, move_slot)) = self.memory.last_slot {
                ctx.events.push(GameEvent::MoveUsed { slot, move_slot });
            }
            self.memory.catch.on_move_confirmed();
        }
    }

    fn expects(&self) -> Expects {
        Expects::BATTLE
    }
}

impl Tool for BattleTool {
    fn name(&self) -> &str {
        "Battle"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Battle { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Battle { policy } = intent else {
            return ToolOutcome::failed("not a Battle");
        };
        let mut step = BattleStep::new(Arc::clone(&ctx.data), *policy, ctx.after_dialogue());
        ctx.drive(&mut step).map(|_| ()).into()
    }
}
