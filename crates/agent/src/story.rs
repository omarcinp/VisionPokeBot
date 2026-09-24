//! Story progression as data: milestones made of small declarative steps
//! (walk somewhere, take a door, talk to someone, answer YES/NO, train, heal),
//! executed closed-loop by one task. Battles and dialogue can interrupt any
//! step. If one of our Pokémon faints the task fails, so the caller can
//! reload the last save and retry.

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_planner::{
    plan_preparation, plan_training, Area, PartyMember, PlanStep, Request, OUR_IV,
};
use pokebot_state::{GameEvent, Observation, Pocket, ScreenState, Status};
use pokebot_world::behavior::TALL_GRASS;
use pokebot_world::World;
use serde::Serialize;

use crate::bag::PocketAudit;
use crate::battle::{self, BattleMemory, BattlePolicy};
use crate::catch;
use crate::learn::MoveLearning;
use crate::nav::{Destination, Gone, NavStatus, Navigator};
use crate::new_game::{advance_or_wait, select};
use crate::party::{self, Party};
use crate::shop::{is_mart_menu, nearest_mart, Purchase};
use crate::stock::{ball_count, must_buy, should_buy};
use crate::track::TextTracker;
use crate::{Action, Decision, Expectation, Outcome, Task, TaskContext};

/// Quiet frames (no dialogue) after a conversation or cutscene before the
/// bot starts walking again; scripts often pause between lines.
const SETTLE_FRAMES: u32 = 90;
/// A battle that starts this soon after dialogue is a trainer battle (the
/// trainer talks first); wild battles start without any.
const TRAINER_DIALOGUE_WINDOW: u64 = 600;
/// Go heal when the lead's HP drops below this share (per mille) while
/// training or on the way to a planned battle.
const HEAL_BELOW: u32 = 500;
/// The planner's confidence: a planned battle needs at least this P(win).
const BATTLE_CONFIDENCE: f64 = 0.9;
/// Frames standing on a Battle step's trigger with nothing happening before
/// the step fails (like a Talk's wait for its battle).
const TRIGGER_TIMEOUT_FRAMES: u32 = 600;
/// Training heals when wild battles have fewer attack PP than this to spend
/// (a few battles' worth).
const HEAL_BELOW_PP: u32 = 6;
/// Status and pre-battle heals per story step: a lead that keeps getting
/// worn down (a wild battle after each heal) must not walk to the Pokémon
/// Center forever.
const MAX_HEALS_PER_STEP: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Answer {
    Yes,
    No,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Condition {
    OnMap(String),
}

impl Condition {
    fn holds(&self, observation: &Observation) -> bool {
        match self {
            Condition::OnMap(map) => observation
                .player
                .as_ref()
                .is_some_and(|p| p.pose.map == *map),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum StoryStep {
    /// Walk to a destination.
    Go(Destination),
    /// Walk toward `dest` until `until` holds. Once a cutscene starts (any
    /// dialogue), stop walking and only follow the script.
    GoUntil { dest: Destination, until: Condition },
    /// Talk to map object `object` (its decomp local id), answering YES/NO
    /// questions in order.
    Talk {
        map: String,
        object: u32,
        answers: Vec<Answer>,
    },
    /// Talk to a trainer/leader and win the battle that follows.
    Challenge { map: String, object: u32 },
    /// Follow dialogue/cutscenes until nothing has happened for `frames`.
    Settle { frames: u32 },
    /// Walk toward `trigger` until a battle starts, fight it, and finish once
    /// back in the overworld. With `loss_ok` (the first rival battle), losing
    /// is part of the story and not a failure. `trainer` (a decomp trainer
    /// constant) is who the trigger's battle is against, when known: the
    /// lead heals first if it isn't ready for them.
    Battle {
        trigger: Destination,
        loss_ok: bool,
        trainer: Option<String>,
    },
    /// Heal at a Pokémon Center (the given one, or the nearest).
    Heal { center: Option<String> },
    /// Walk the tall grass of `map`, fighting wild Pokémon, until the lead is
    /// at least `level` (healing when needed).
    Train { map: String, level: u8 },
    /// Ask the readiness planner how to be ready for `targets`, and replace
    /// this step with its plan.
    Prepare {
        targets: Vec<String>,
        areas: Vec<String>,
        confidence: f64,
    },
    /// Open the bag from the Start menu, read this pocket (`PocketObserved`)
    /// and close every menu again.
    AuditPocket(Pocket),
    /// Buy `count` of `item` at `mart` (or the nearest mart selling it that
    /// can be walked to; the clerk is approached like `Talk`, then
    /// [`Purchase`] runs the mart). `count: 0` means "decide on the list":
    /// the ball stock policy's count for the money read there, possibly
    /// none. The mart lives on the step, so a splice (a heal on the way)
    /// keeps it.
    Buy {
        item: String,
        count: u16,
        mart: Option<String>,
    },
    /// Audit if needed, then Buy up to the target at `mart` (or the nearest
    /// one selling balls).
    StockUp { mart: Option<String> },
}

#[derive(Debug, Clone, Serialize)]
pub struct Milestone {
    pub name: String,
    pub description: String,
    pub steps: Vec<StoryStep>,
}

impl Milestone {
    pub fn new(name: &str, description: &str, steps: Vec<StoryStep>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            steps,
        }
    }
}

type Tile = (i32, i32);
/// P(win) per target trainer.
type PWin = Vec<(String, f64)>;

/// Frames a page's text must stay unchanged to count as fully printed.
const PAGE_PRINTED_FRAMES: u32 = 8;
/// Longest wait for a page waiting for A to read the same on a second
/// frame (a flickering misread must not stall the story).
const PAGE_READ_WAIT_FRAMES: u64 = 10;

/// Time each direction of a spin is held: under the 8-frame turn (134 ms),
/// so the direction read when a turn ends is always a new one (holding the
/// facing direction then would walk instead of turning).
const SPIN_STEP_MS: u64 = 100;
/// Direction changes per spin action (~2.4 s; a battle cancels it early).
const SPIN_TURNS: usize = 24;

/// Where to spin for encounters: a grass tile, preferring ones surrounded by
/// grass (a turn that becomes a step still lands in grass), then closeness.
pub(crate) fn spin_tile(
    grass: &impl Fn(i32, i32) -> bool,
    width: i32,
    height: i32,
    near: Tile,
) -> Option<Tile> {
    let mut best: Option<(i32, Tile)> = None;
    for y in 0..height {
        for x in 0..width {
            if !grass(x, y) {
                continue;
            }
            let around = [(0, -1), (1, 0), (0, 1), (-1, 0)]
                .iter()
                .filter(|(dx, dy)| grass(x + dx, y + dy))
                .count() as i32;
            let score = 8 * around - ((x - near.0).abs() + (y - near.1).abs());
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, (x, y)));
            }
        }
    }
    best.map(|(_, t)| t)
}

/// Turning in place rolls for a wild encounter like a step does (the turn's
/// end counts as arriving on the tile; pokefirered field_player_avatar.c
/// UpdatePlayerAvatarTransitionState) in half the time, without moving.
/// Up, Right, Down, Left, ...: every direction differs from the last.
pub(crate) fn spin_sequence(turns: usize) -> Vec<pokebot_core::TimedInput> {
    [Button::Up, Button::Right, Button::Down, Button::Left]
        .into_iter()
        .cycle()
        .take(turns)
        .map(|b| pokebot_core::TimedInput {
            buttons: [b].into_iter().collect(),
            duration: std::time::Duration::from_millis(SPIN_STEP_MS),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TalkPhase {
    Approach,
    Press,
    Talking,
}

pub struct StoryTask {
    world: Arc<World>,
    data: Option<Arc<GameData>>,
    /// The party as last read from the game state (refreshed every tick).
    party: Party,
    milestones: Vec<Milestone>,
    milestone: usize,
    step: usize,
    nav: Option<Navigator>,
    /// The device's timing model, handed to every navigator.
    syncer: Option<crate::motion::SyncerHandle>,
    talk: TalkPhase,
    saw_dialogue: bool,
    answers_used: usize,
    quiet_frames: u32,
    announced: bool,
    battle_seen: bool,
    /// Where the player last stood outside battle, and where the current
    /// (or last) battle began: a Battle step's trigger battle begins on a
    /// trigger tile; battles elsewhere (wild, trainers who spot us) don't
    /// count as it.
    last_pose: Option<pokebot_state::PlayerPose>,
    battle_start_pose: Option<pokebot_state::PlayerPose>,
    /// The lead's major status as last known (from battle text; a heal
    /// clears it).
    lead_status: Option<Status>,
    /// The step just finished was a Heal and no battle has happened since:
    /// healing again would change nothing (guards Heal → step → Heal loops).
    just_healed: bool,
    /// The planned battle's "not ready even healed" warning was given in
    /// this step.
    readiness_warned: bool,
    /// Status and pre-battle heals spliced before the current story step
    /// (capped at [`MAX_HEALS_PER_STEP`]; the Heal steps themselves don't
    /// reset it).
    heals_this_step: u32,
    /// The "heal cap reached" warning was given for the current story step.
    heal_cap_warned: bool,
    /// Frame the Battle step first stood on its trigger (for its timeout).
    trigger_arrived_at: Option<u64>,
    policy: BattlePolicy,
    battle_memory: BattleMemory,
    /// Inside a battle right now (ended once the overworld is seen again).
    in_battle: bool,
    last_dialogue_frame: Option<u64>,
    /// Training: the two grass tiles we pace between, and which is next.
    /// The grass tile training spins on.
    spin_at: Option<Tile>,
    /// New moves and evolution, read from the screen.
    learning: MoveLearning,
    /// Money, items, heals and empty moves, read from dialogue text.
    tracker: TextTracker,
    /// Trainers the last plan prepared for (values moves when learning).
    upcoming: Vec<String>,
    /// Whether the current Heal step's heal was read on screen.
    heal: HealWatch,
    /// The current AuditPocket step's flow.
    audit: Option<PocketAudit>,
    /// The current Buy step's mart flow (from the first mart screen on).
    purchase: Option<Purchase>,
    /// The current Buy step's mart map and clerk.
    buy_at: Option<(String, u32)>,
    /// The current Talk step's conversation gave us an item.
    talk_gained_item: bool,
    /// Objects taken in this run (item balls, fossils): gone from the map.
    taken: Gone,
    /// A mandatory ball buy was tried in this milestone (never retried:
    /// with no money it would loop).
    mandatory_buy_tried: bool,
    /// First frame of the current wait for a menu-less page waiting for A
    /// to be read on two frames: one wait per dialogue box, whatever the
    /// readings (a misread flicker must not restart it).
    unread_since: Option<u64>,
    /// Last frame whose battle text went to the battle's readers.
    battle_text_frame: Option<u64>,
}

/// Whether the nurse's "restored your POKéMON" was read during the current
/// Heal step. The nurse always heals, so a missed line is inferred when the
/// conversation ends: otherwise the bot would keep going back to heal.
#[derive(Debug, Default)]
struct HealWatch {
    seen: bool,
}

impl HealWatch {
    fn observe(&mut self, events: &[GameEvent]) {
        self.seen |= events.contains(&GameEvent::Healed);
    }

    fn reset(&mut self) {
        self.seen = false;
    }

    /// Events for the end of the Heal step's conversation.
    fn finish(&mut self) -> Vec<GameEvent> {
        if std::mem::replace(&mut self.seen, true) {
            return Vec::new();
        }
        vec![
            GameEvent::Healed,
            GameEvent::GoalProgress {
                goal: "Story".into(),
                phase: "Party".into(),
                detail: "heal inferred: the nurse's text was not read".into(),
            },
        ]
    }
}

impl StoryTask {
    /// A battle's first frame: fresh battle memory and `BattleStarted`.
    fn begin_battle(&mut self, o: &Observation, events: &mut Vec<GameEvent>) {
        if self.in_battle {
            return;
        }
        self.in_battle = true;
        self.battle_seen = true;
        self.just_healed = false;
        self.battle_start_pose = self.last_pose.clone();
        let trainer = self
            .last_dialogue_frame
            .is_some_and(|f| o.frame_id.saturating_sub(f) < TRAINER_DIALOGUE_WINDOW);
        self.battle_memory = BattleMemory {
            trainer,
            ..BattleMemory::default()
        };
        events.push(GameEvent::BattleStarted);
    }

    pub fn new(world: Arc<World>, milestones: Vec<Milestone>) -> Self {
        Self {
            world,
            data: None,
            party: Party::default(),
            milestones,
            milestone: 0,
            step: 0,
            nav: None,
            syncer: None,
            talk: TalkPhase::Approach,
            saw_dialogue: false,
            answers_used: 0,
            quiet_frames: SETTLE_FRAMES,
            announced: false,
            battle_seen: false,
            last_pose: None,
            battle_start_pose: None,
            lead_status: None,
            just_healed: false,
            readiness_warned: false,
            heals_this_step: 0,
            heal_cap_warned: false,
            trigger_arrived_at: None,
            policy: BattlePolicy::default(),
            battle_memory: BattleMemory::default(),
            in_battle: false,
            last_dialogue_frame: None,
            spin_at: None,
            learning: MoveLearning::default(),
            tracker: TextTracker::default(),
            upcoming: Vec::new(),
            heal: HealWatch::default(),
            audit: None,
            purchase: None,
            buy_at: None,
            mandatory_buy_tried: false,
            talk_gained_item: false,
            taken: Gone::new(),
            unread_since: None,
            battle_text_frame: None,
        }
    }

    /// Game data enables planning, training and move choice (the party
    /// itself is read from the game state).
    pub fn with_data(mut self, data: Arc<GameData>) -> Self {
        self.data = Some(data);
        self
    }

    /// Walk with this timing model's hold lengths and timeouts.
    pub fn with_syncer(mut self, syncer: crate::motion::SyncerHandle) -> Self {
        self.syncer = Some(syncer);
        self
    }

    fn navigator(&self, dest: &Destination) -> Navigator {
        let nav =
            Navigator::new(Arc::clone(&self.world), dest.clone()).with_gone(self.taken.clone());
        match &self.syncer {
            Some(syncer) => nav.with_syncer(Arc::clone(syncer)),
            None => nav,
        }
    }

    /// The party as last read from the game state.
    pub fn party(&self) -> &Party {
        &self.party
    }

    /// Names of milestones completed so far.
    pub fn completed(&self) -> Vec<String> {
        self.milestones[..self.milestone.min(self.milestones.len())]
            .iter()
            .map(|m| m.name.clone())
            .collect()
    }

    fn current(&self) -> Option<&StoryStep> {
        self.milestones.get(self.milestone)?.steps.get(self.step)
    }

    fn advance_step(&mut self, ctx: &mut TaskContext<'_>) {
        self.just_healed = matches!(self.current(), Some(StoryStep::Heal { .. }));
        if !self.just_healed {
            self.heals_this_step = 0;
            self.heal_cap_warned = false;
        }
        self.readiness_warned = false;
        self.trigger_arrived_at = None;
        self.step += 1;
        self.nav = None;
        self.talk = TalkPhase::Approach;
        self.saw_dialogue = false;
        self.answers_used = 0;
        self.talk_gained_item = false;
        self.battle_seen = false;
        self.spin_at = None;
        self.heal.reset();
        self.audit = None;
        self.purchase = None;
        self.buy_at = None;
        if self.step >= self.milestones[self.milestone].steps.len() {
            ctx.events.push(GameEvent::GoalProgress {
                goal: "Story".into(),
                phase: self.milestones[self.milestone].name.clone(),
                detail: "completed".into(),
            });
            self.milestone += 1;
            self.step = 0;
            self.announced = false;
            self.mandatory_buy_tried = false;
        }
    }

    /// Replaces the current step with `steps` (e.g. a plan, or heal + resume).
    fn splice(&mut self, steps: Vec<StoryStep>, keep_current: bool) {
        if !keep_current {
            self.heals_this_step = 0;
            self.heal_cap_warned = false;
        }
        let list = &mut self.milestones[self.milestone].steps;
        let at = if keep_current {
            self.step
        } else {
            list.remove(self.step);
            self.step
        };
        for (i, s) in steps.into_iter().enumerate() {
            list.insert(at + i, s);
        }
        self.nav = None;
        self.talk = TalkPhase::Approach;
        self.saw_dialogue = false;
        self.answers_used = 0;
        self.talk_gained_item = false;
        self.spin_at = None;
        self.heal.reset();
        self.audit = None;
        self.purchase = None;
        self.buy_at = None;
    }

    fn navigate(&mut self, dest: &Destination, observation: &Observation) -> NavStatusOrDecision {
        if self.quiet_frames < SETTLE_FRAMES {
            return NavStatusOrDecision::Decision(Decision::Wait(
                "letting the scene settle".into(),
            ));
        }
        if self.nav.as_ref().is_none_or(|nav| nav.destination != *dest) {
            self.nav = Some(self.navigator(dest));
        }
        let nav = self.nav.as_mut().expect("navigator set above");
        if nav.stalled() >= 6 {
            return NavStatusOrDecision::Decision(Decision::Fail(format!(
                "cannot move toward {dest:?}"
            )));
        }
        NavStatusOrDecision::Nav(nav.next(observation))
    }

    /// Nearest Pokémon Center nurse from `from` (fewest maps crossed).
    fn nearest_nurse(&self, from: &str) -> Option<(String, u32)> {
        let mut dist = std::collections::HashMap::from([(from.to_owned(), 0u32)]);
        let mut queue = std::collections::VecDeque::from([from.to_owned()]);
        let mut best: Option<(u32, String)> = None;
        while let Some(name) = queue.pop_front() {
            let d = dist[&name];
            if name.ends_with("PokemonCenter_1F")
                && best.as_ref().is_none_or(|(bd, bn)| (d, &name) < (*bd, bn))
            {
                best = Some((d, name.clone()));
            }
            let Some(map) = self.world.map(&name) else {
                continue;
            };
            let next: Vec<String> = map
                .warps
                .iter()
                .filter(|w| w.dest_warp >= 0)
                .filter_map(|w| self.world.name_of(&w.dest_map))
                .chain(
                    map.connections
                        .iter()
                        .filter_map(|c| self.world.name_of(&c.map)),
                )
                .map(str::to_owned)
                .collect();
            for n in next {
                if !dist.contains_key(&n) {
                    dist.insert(n.clone(), d + 1);
                    queue.push_back(n);
                }
            }
        }
        let (_, map) = best?;
        let nurse = self
            .world
            .map(&map)?
            .objects
            .iter()
            .find(|o| o.graphics.as_deref() == Some("OBJ_EVENT_GFX_NURSE"))?
            .local_id;
        Some((map, nurse))
    }

    /// Where to spin on `map` for encounters (tall grass or cave floor),
    /// near `near`.
    fn grass_spot(&self, map: &str, near: Tile) -> Option<Tile> {
        let m = self.world.map(map)?;
        let grass = |x: i32, y: i32| m.tile(x, y).is_some_and(|t| encounter_tile(&t));
        spin_tile(&grass, m.width, m.height, near)
    }

    /// The steps that make the lead ready for `targets`, and the lead-only
    /// P(win) per target the chosen plan reaches.
    ///
    /// Only the lead fights (switching is out of scope: "change POKéMON?"
    /// is answered NO and a lead faint fails the story), so readiness is
    /// planned for the lead alone: caught members never count toward P(win).
    fn plan(
        &self,
        targets: &[String],
        areas: &[String],
        confidence: f64,
    ) -> Result<(Vec<StoryStep>, PWin), String> {
        let data = self.data.as_ref().ok_or("planning needs game data")?;
        let party: Vec<PartyMember> = self
            .party
            .members
            .iter()
            .take(1)
            .map(|m| PartyMember {
                species: m.species.clone(),
                level: m.level,
                exp: None,
                moves: m.moves.clone(),
            })
            .collect();
        if party.is_empty() {
            return Err("no party knowledge".into());
        }
        let request = Request {
            party,
            targets: targets.to_vec(),
            areas: areas
                .iter()
                .map(|a| Area {
                    map: a.clone(),
                    travel_minutes: 1.5,
                    heal_minutes: 2.5,
                })
                .collect(),
            confidence,
            money: 0,
            data,
        };
        let plans = plan_preparation(&request, 1);
        let plan = plans.first().ok_or("no plan found")?;
        if plan.min_confidence() < confidence {
            return Err(format!(
                "best plan only reaches {:.0}%",
                plan.min_confidence() * 100.0
            ));
        }
        let lead = &self.party.members[0].species;
        // Switching is out of scope, so a catch only helps as a place to
        // train: accepted when the lead alone (no catch) reaches the
        // confidence, and trained to the level that plan needs.
        let lead_only = || {
            let plans = plan_training(&request, 1);
            let plan = plans
                .first()
                .ok_or("the plan catches; no lead-only plan found")?;
            if plan.min_confidence() < confidence {
                return Err(format!(
                    "the plan catches, and without catching the lead alone only reaches {:.0}%",
                    plan.min_confidence() * 100.0
                ));
            }
            Ok(plan
                .party
                .first()
                .map_or(self.party.members[0].level, |(_, level, _)| *level))
        };
        let steps = plan_to_steps(&plan.steps, lead, lead_only)?;
        Ok((steps, plan.confidence.clone()))
    }
}

/// The lead's P(win) alone against `trainer` (the planner's model, with
/// [`OUR_IV`]): at its current HP with only moves that have PP left, or
/// `healed` (full HP, every move).
fn lead_p_win(data: &GameData, lead: &party::Member, trainer: &str, healed: bool) -> f64 {
    let moves: Vec<String> = if healed {
        lead.moves.clone()
    } else {
        lead.moves
            .iter()
            .filter(|m| lead.pp_left(data, m) > 0)
            .cloned()
            .collect()
    };
    let hp = lead.hp.filter(|_| !healed).map(|(hp, _)| u32::from(hp));
    pokebot_planner::Combatant::new(data, &lead.species, lead.level, moves, OUR_IV)
        .and_then(|mut us| {
            us.hp = hp.map_or(us.max_hp(), |hp| hp.min(us.max_hp()));
            pokebot_planner::evaluate::battle_vs_trainer(data, std::slice::from_ref(&us), trainer)
        })
        .map_or(0.0, |e| e.p_win)
}

/// "P(win) vs T 0.953, …" for a plan's per-target confidence.
fn format_p_win(p_win: &[(String, f64)]) -> String {
    p_win
        .iter()
        .map(|(t, p)| format!("P(win) vs {t} {p:.3}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The planner's steps as story steps. Only the lead trains (switching is
/// out of scope): training another member fails. A catch becomes training
/// at the catch's area to the level `lead_only` says the lead alone needs
/// (or its error), and the caught species' own training is dropped with it.
fn plan_to_steps(
    plan: &[PlanStep],
    lead: &str,
    mut lead_only: impl FnMut() -> Result<u8, String>,
) -> Result<Vec<StoryStep>, String> {
    let mut caught: Vec<&str> = Vec::new();
    let mut steps = Vec::new();
    for step in plan {
        match step {
            PlanStep::Train {
                species, to, map, ..
            } if species == lead => steps.push(StoryStep::Train {
                map: map.clone(),
                level: *to,
            }),
            PlanStep::Train { species, .. } if caught.contains(&species.as_str()) => {}
            PlanStep::Catch { species, map, .. } => {
                let level = lead_only()?;
                caught.push(species);
                steps.push(StoryStep::Train {
                    map: map.clone(),
                    level,
                });
            }
            other => return Err(format!("plan step not supported yet: {other:?}")),
        }
    }
    Ok(steps)
}

/// Where wild Pokémon appear on foot: tall grass, or cave floor (plain or
/// `MB_CAVE`/`MB_SAND_CAVE`) with land encounters. Never ladders, warps or
/// water.
fn encounter_tile(t: &pokebot_world::Tile) -> bool {
    const CAVE: u16 = 0x08;
    const SAND_CAVE: u16 = 0x2B;
    t.collision == 0
        && (t.behavior == TALL_GRASS
            || (t.encounter == 1 && matches!(t.behavior, 0x00 | CAVE | SAND_CAVE)))
}

enum NavStatusOrDecision {
    Nav(NavStatus),
    Decision(Decision),
}

impl StoryTask {
    /// Battle text to the battle's readers, once per frame: the throw's
    /// result, the foe's status, the PC box ([`catch::observe`]) and
    /// DISABLE on our lead ([`battle::observe_page`]).
    fn observe_battle_text(&mut self, o: &Observation, events: &mut Vec<GameEvent>) {
        if o.battle.is_none() || self.battle_text_frame == Some(o.frame_id) {
            return;
        }
        self.battle_text_frame = Some(o.frame_id);
        if let Some(page) = catch::observe(&mut self.battle_memory, o) {
            if let Some(data) = &self.data {
                battle::observe_page(&mut self.battle_memory, &page, &self.party, data);
            }
            // The lead's major status, from text (the HUD badge isn't read);
            // once per change (the page is seen on many frames).
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

impl Task for StoryTask {
    fn name(&self) -> &str {
        "Story"
    }

    fn next(&mut self, ctx: &mut TaskContext<'_>) -> Decision {
        self.party = Party::from_state(ctx.state);
        self.lead_status = ctx
            .state
            .party
            .value
            .as_ref()
            .and_then(|p| p.first())
            .and_then(|m| m.status.value);
        let o = ctx.observation;
        if let (Some(p), None) = (&o.player, &o.battle) {
            self.last_pose = Some(p.pose.clone());
        }
        let Some(milestone) = self.milestones.get(self.milestone) else {
            let names: Vec<&str> = self.milestones.iter().map(|m| m.name.as_str()).collect();
            return Decision::Done(format!("completed {}", names.join(", ")));
        };
        if !self.announced {
            self.announced = true;
            ctx.events.push(GameEvent::GoalProgress {
                goal: "Story".into(),
                phase: milestone.name.clone(),
                detail: milestone.description.clone(),
            });
        }
        let step = self.current().cloned().expect("milestone has steps");

        // New moves and evolution can come up in and after battles: every
        // finished page is read, and the KNOWN MOVES list and questions
        // about moves are answered from the plan's value of each move.
        if let Some(data) = self.data.clone() {
            // Pages that advance by themselves (the nurse's "We've restored
            // your POKéMON…") show no arrow and are gone before they count as
            // settled: read any page once its text stops changing. Both
            // readers ignore a page they have already seen.
            if let Some(d) = o.dialogue.as_ref().filter(|d| {
                d.ready_for_a() || o.menu.is_some() || d.stable_frames >= PAGE_PRINTED_FRAMES
            }) {
                let (events, log) =
                    self.learning
                        .observe_page(&d.lines, &data, &self.party, &self.upcoming);
                ctx.events.extend(events);
                for detail in log {
                    ctx.events.push(GameEvent::GoalProgress {
                        goal: "Story".into(),
                        phase: "Party".into(),
                        detail,
                    });
                }
                let tracked =
                    self.tracker
                        .observe_page(&d.lines, &data, self.battle_memory.last_slot);
                self.heal.observe(&tracked);
                if matches!(self.current(), Some(StoryStep::Talk { .. }))
                    && tracked
                        .iter()
                        .any(|e| matches!(e, GameEvent::ItemsChanged { delta, .. } if *delta > 0))
                {
                    self.talk_gained_item = true;
                }
                ctx.events.extend(tracked);
                // The executor doesn't show the task the frames while its A
                // press is pending: a page advanced on its first ready frame
                // is read once and its facts (money won, items) are lost.
                // Wait for a second reading first (questions, with a menu,
                // are answered instead).
                if d.ready_for_a() && o.menu.is_none() && !self.tracker.applied(&d.lines) {
                    let since = *self.unread_since.get_or_insert(o.frame_id);
                    if o.frame_id.saturating_sub(since) < PAGE_READ_WAIT_FRAMES {
                        // The battle's readers (catch results, DISABLE) need
                        // this frame too.
                        if o.battle.is_some() {
                            self.begin_battle(o, ctx.events);
                            self.observe_battle_text(o, ctx.events);
                        }
                        return Decision::Wait("reading the page on a second frame".into());
                    }
                    // Given up: advance it; the next page waits afresh.
                    self.unread_since = None;
                } else {
                    // Read (applied), or not a page waiting for A.
                    self.unread_since = None;
                }
            }
            if let Some(list) = &o.move_list {
                self.quiet_frames = 0;
                let (decision, events) =
                    self.learning
                        .on_move_list(list, &data, &self.party, &self.upcoming);
                ctx.events.extend(events);
                return decision;
            }
            if let (Some(d), Some(menu)) = (&o.dialogue, &o.menu) {
                if let Some(yes) = self.learning.answer(&d.lines) {
                    self.quiet_frames = 0;
                    return select(
                        menu,
                        u8::from(!yes),
                        if yes { "learning: YES" } else { "learning: NO" },
                    );
                }
            }
        }

        // Battles interrupt whatever the step is doing (wild encounters,
        // trainers, the rival).
        if let Some(battle) = &o.battle {
            self.begin_battle(o, ctx.events);
            if let Some(data) = &self.data {
                ctx.events
                    .extend(party::battle_events(data, &self.party, battle));
                // Wild opponents are identified once, and a catch decided.
                catch::identify(
                    o,
                    data,
                    ctx.state,
                    &self.party,
                    &mut self.battle_memory,
                    ctx.events,
                );
            }
            // The throw's result, the foe's status, the PC box and DISABLE,
            // from text.
            self.observe_battle_text(o, ctx.events);
            let loss_ok = matches!(step, StoryStep::Battle { loss_ok: true, .. });
            if battle.player_hp_numbers.is_some_and(|(hp, _)| hp == 0) && !loss_ok {
                return Decision::Fail(format!(
                    "our Pokémon fainted ({})",
                    battle.player_name.clone().unwrap_or_default()
                ));
            }
            self.quiet_frames = 0;
            if let Some(data) = self.data.clone() {
                if let Some(decision) = battle::decide(
                    o,
                    &self.policy,
                    &mut self.battle_memory,
                    &self.party,
                    &data,
                    ctx.events,
                ) {
                    return decision;
                }
            }
            if let (Some(d), Some(_)) = (&o.dialogue, &o.menu) {
                // The box may belong to the page before (the nickname
                // question's YES/NO stays while the next page prints):
                // read the question once it is printed.
                if !d.ready_for_a() && d.stable_frames < PAGE_PRINTED_FRAMES {
                    return Decision::Wait("the question is printing".into());
                }
                // After a catch: "Give a nickname to the captured X?" → No
                // (B answers No).
                if catch::is_nickname_question(&d.lines.join(" ")) {
                    return Decision::Act(Action::new(
                        "nickname: NO",
                        vec![ControllerCommand::Press(Button::B)],
                        Expectation::MenuClosed,
                        90,
                    ));
                }
                // A trainer about to send the next Pokémon (Shift style,
                // with 2+ in the party): "Will RED change POKéMON?" → No
                // (the lead fights; switching is out of scope).
                if battle::is_switch_question(&d.lines.join(" ")) {
                    return Decision::Act(Action::new(
                        "change Pokémon: NO",
                        vec![ControllerCommand::Press(Button::B)],
                        Expectation::MenuClosed,
                        90,
                    ));
                }
                // The PC transfer text after NO to the nickname, with the
                // YES/NO box still drawn: plain text to advance.
                if catch::is_pc_transfer_text(&d.lines.join(" ")) {
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
            if let Some(data) = &self.data {
                ctx.events
                    .extend(self.battle_memory.catch.after_battle(data, ctx.state));
            }
        }
        // Battle screens without the battle HUD: the Pokédex page after a
        // first catch, and the battle bag during a throw (the frames around
        // it too: its opening shows a lone ▶ that looks like a menu).
        if self.in_battle {
            if let Some(decision) = catch::dismiss_pokedex(&mut self.battle_memory.catch, o) {
                self.quiet_frames = 0;
                return decision;
            }
            if o.bag.is_some() || self.battle_memory.catch.thrower.is_some() {
                self.quiet_frames = 0;
                if let Some(data) = self.data.clone() {
                    return catch::in_bag(o, &data, &mut self.battle_memory.catch, ctx.events);
                }
            }
        }
        // The pocket audit drives the Start menu and the bag itself (both
        // would otherwise be closed as unexpected menus below); dialogue
        // without a menu still goes to the dialogue handling.
        if let StoryStep::AuditPocket(pocket) = &step {
            // Dialogue *with* a menu goes to the audit too: the Start menu
            // shows a help line under it, and if that ever reads as a
            // message box the handler below would take the Start menu for
            // an unexpected question and fail the step. The audit only
            // acts on a menu whose rows read as the Start menu (with BAG),
            // so a real question never gets an answer from it: the audit
            // waits and fails instead of pressing A on YES.
            if o.dialogue.is_none() || o.menu.is_some() {
                // Settling counts here too (the step never reaches the
                // counter below).
                if o.dialogue.is_some() {
                    self.quiet_frames = 0;
                } else {
                    self.quiet_frames = self.quiet_frames.saturating_add(1);
                }
                return self.audit_pocket(*pocket, o, ctx);
            }
        }
        // Once the clerk talks (or any mart screen shows), the purchase
        // drives every screen until the mart is left: its menu and YES/NO
        // questions would otherwise be unexpected questions below.
        if let StoryStep::Buy { item, count, .. } = &step {
            if self.purchase.is_some()
                || self.talk == TalkPhase::Talking
                || o.shop.is_some()
                || is_mart_menu(o)
            {
                if o.dialogue.is_some() || o.menu.is_some() || o.shop.is_some() {
                    self.quiet_frames = 0;
                } else {
                    self.quiet_frames = self.quiet_frames.saturating_add(1);
                }
                return self.shop(item, *count, o, ctx);
            }
        }
        // Dialogue and menus interrupt whatever the step is doing.
        if o.dialogue.is_some() || (o.menu.is_some() && o.screen.value == ScreenState::Dialogue) {
            self.quiet_frames = 0;
            self.saw_dialogue = true;
            self.trigger_arrived_at = None;
            self.last_dialogue_frame = Some(o.frame_id);
            if let Some(menu) = &o.menu {
                let answers: &[Answer] = match &step {
                    StoryStep::Talk { answers, .. } => answers,
                    // Pokémon Center: "Shall we heal your POKéMON?" → YES.
                    StoryStep::Heal { .. } => &[Answer::Yes],
                    _ => &[],
                };
                return match answers.get(self.answers_used) {
                    Some(Answer::Yes) => select(menu, 0, "answer YES"),
                    Some(Answer::No) => select(menu, 1, "answer NO"),
                    None => Decision::Fail(format!("unexpected question during {step:?}")),
                };
            }
            return advance_or_wait(o.dialogue.as_ref(), "dialogue");
        }
        if o.menu.is_some() {
            return Decision::Act(Action::new(
                "close an unexpected menu",
                vec![ControllerCommand::Press(Button::B)],
                Expectation::MenuClosed,
                45,
            ));
        }
        if matches!(o.screen.value, ScreenState::Transition) {
            return Decision::Wait("screen transition".into());
        }
        self.quiet_frames = self.quiet_frames.saturating_add(1);

        // Below the shiny reserve: buy balls before going on (once per
        // milestone; a buy that finds no money is not retried).
        let starts_out = match &step {
            StoryStep::Go(_) | StoryStep::Train { .. } => true,
            StoryStep::GoUntil { .. } => !self.saw_dialogue,
            StoryStep::Battle { .. } | StoryStep::Challenge { .. } => {
                !self.battle_seen && self.talk == TalkPhase::Approach
            }
            _ => false,
        };
        if starts_out
            && !self.mandatory_buy_tried
            && self.data.is_some()
            && must_buy(ball_count(ctx.state))
        {
            self.mandatory_buy_tried = true;
            ctx.events.push(GameEvent::GoalProgress {
                goal: "Story".into(),
                phase: "Mart".into(),
                detail: format!(
                    "{} Poké Balls, under the reserve: buying before going on",
                    ball_count(ctx.state).unwrap_or(0)
                ),
            });
            self.splice(vec![StoryStep::StockUp { mart: None }], true);
            return Decision::Wait("going to buy Poké Balls".into());
        }
        // A lead with a major status (paralysis, poison, ...) heals before
        // walking on: the plan's battles assume a healthy lead.
        let walking = starts_out
            || matches!(&step, StoryStep::Talk { .. })
                && self.talk == TalkPhase::Approach
                && !self.saw_dialogue;
        if walking && !self.just_healed {
            if let Some(status) = self.lead_status_major() {
                if self.heals_this_step < MAX_HEALS_PER_STEP {
                    self.heals_this_step += 1;
                    ctx.events.push(GameEvent::GoalProgress {
                        goal: "Story".into(),
                        phase: "Heal".into(),
                        detail: format!("the lead is {status:?}: healing"),
                    });
                    self.splice(vec![StoryStep::Heal { center: None }], true);
                    return Decision::Wait("healing a status condition".into());
                }
                if !std::mem::replace(&mut self.heal_cap_warned, true) {
                    ctx.events.push(GameEvent::GoalProgress {
                        goal: "Story".into(),
                        phase: "Warning".into(),
                        detail: format!(
                            "the lead is {status:?}, but {MAX_HEALS_PER_STEP} heals were \
                             already made in this step: going on without healing"
                        ),
                    });
                }
            }
        }

        match step {
            StoryStep::Go(dest) => self.go(&dest, o, ctx),
            StoryStep::GoUntil { dest, until } => {
                if until.holds(o) {
                    self.advance_step(ctx);
                    return Decision::Wait("condition reached".into());
                }
                if self.saw_dialogue {
                    return Decision::Wait("following the cutscene".into());
                }
                match self.navigate(&dest, o) {
                    NavStatusOrDecision::Decision(d) => d,
                    NavStatusOrDecision::Nav(NavStatus::Arrived) => {
                        Decision::Wait("waiting for the event".into())
                    }
                    NavStatusOrDecision::Nav(status) => nav_decision(status),
                }
            }
            StoryStep::Talk { map, object, .. } => self.talk_to(&map, object, false, o, ctx),
            StoryStep::Challenge { map, object } => self.talk_to(&map, object, true, o, ctx),
            StoryStep::Heal { center } => {
                let Some(pose) = &o.player else {
                    return Decision::Wait("locating".into());
                };
                let target = match center {
                    Some(map) => self
                        .world
                        .map(&map)
                        .and_then(|m| {
                            m.objects
                                .iter()
                                .find(|ob| ob.graphics.as_deref() == Some("OBJ_EVENT_GFX_NURSE"))
                        })
                        .map(|ob| (map.clone(), ob.local_id)),
                    None => self.nearest_nurse(&pose.pose.map),
                };
                let Some((map, nurse)) = target else {
                    return Decision::Fail("no Pokémon Center found".into());
                };
                self.talk_to(&map, nurse, false, o, ctx)
            }
            StoryStep::Train { map, level } => self.train(&map, level, o, ctx),
            StoryStep::Prepare {
                targets,
                areas,
                confidence,
            } => match self.plan(&targets, &areas, confidence) {
                Ok((steps, p_win)) => {
                    self.upcoming = targets.clone();
                    ctx.events.push(GameEvent::GoalProgress {
                        goal: "Story".into(),
                        phase: "Plan".into(),
                        detail: if steps.is_empty() {
                            format!("already ready (lead alone): {}", format_p_win(&p_win))
                        } else {
                            format!("{steps:?}")
                        },
                    });
                    self.splice(steps, false);
                    Decision::Wait("planned".into())
                }
                Err(e) => Decision::Fail(format!("planning failed: {e}")),
            },
            StoryStep::Battle {
                trigger,
                loss_ok,
                trainer,
            } => {
                if self.battle_seen {
                    if self.in_battle || self.battle_was_the_triggers(&trigger) {
                        if !self.in_battle && self.quiet_frames >= SETTLE_FRAMES {
                            self.advance_step(ctx);
                        }
                        return Decision::Wait("battle over; settling".into());
                    }
                    // A wild battle or a trainer who spotted us on the way:
                    // not the trigger's battle.
                    self.battle_seen = false;
                }
                // The plan vouched for this battle with a healed party:
                // don't walk into it (or the trainers on the way) weakened.
                if !loss_ok {
                    if let Some(d) = self.heal_before(trainer.as_deref(), "battle", ctx.events) {
                        return d;
                    }
                }
                match self.navigate(&trigger, o) {
                    NavStatusOrDecision::Decision(d) => d,
                    NavStatusOrDecision::Nav(NavStatus::Arrived) => {
                        // On the trigger the script starts at once: time
                        // out from the first frame standing there (cleared
                        // by dialogue, a battle or leaving the tile).
                        let since = *self.trigger_arrived_at.get_or_insert(o.frame_id);
                        if o.frame_id.saturating_sub(since) >= u64::from(TRIGGER_TIMEOUT_FRAMES) {
                            return Decision::Fail(format!(
                                "on the trigger {trigger:?}, but no battle followed"
                            ));
                        }
                        Decision::Wait("waiting for the battle".into())
                    }
                    NavStatusOrDecision::Nav(status) => {
                        self.trigger_arrived_at = None;
                        nav_decision(status)
                    }
                }
            }
            StoryStep::AuditPocket(pocket) => self.audit_pocket(pocket, o, ctx),
            StoryStep::Buy { item, count, mart } => {
                self.go_to_clerk(&item, count, mart.as_deref(), o, ctx)
            }
            StoryStep::StockUp { mart } => self.stock_up(mart, ctx),
            StoryStep::Settle { frames } => {
                if self.quiet_frames >= frames {
                    self.advance_step(ctx);
                }
                Decision::Wait("letting the story play out".into())
            }
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, ctx: &mut TaskContext<'_>) {
        if action.label == "choose RUN" && outcome == Outcome::Confirmed {
            self.battle_memory.run_attempts += 1;
        }
        if action.label.starts_with("choose move") && outcome == Outcome::Confirmed {
            if let Some((slot, move_slot)) = self.battle_memory.last_slot {
                ctx.events.push(GameEvent::MoveUsed { slot, move_slot });
            }
            self.battle_memory.catch.on_move_confirmed();
        }
        match (&action.expect, outcome) {
            (Expectation::MenuClosed, Outcome::Confirmed) if action.label.starts_with("answer") => {
                self.answers_used += 1;
            }
            (Expectation::DialogueOpen, Outcome::Confirmed) => self.talk = TalkPhase::Talking,
            (Expectation::DialogueOpen, Outcome::TimedOut) => {
                // Probably not facing it: approach again.
                self.talk = TalkPhase::Approach;
                self.nav = None;
            }
            _ => {
                if let Some(nav) = &mut self.nav {
                    nav.on_outcome(action, outcome);
                }
            }
        }
    }
}

impl StoryTask {
    fn audit_pocket(
        &mut self,
        pocket: Pocket,
        o: &Observation,
        ctx: &mut TaskContext<'_>,
    ) -> Decision {
        let Some(data) = self.data.clone() else {
            return Decision::Fail("the pocket audit needs game data".into());
        };
        let audit = self.audit.get_or_insert_with(|| PocketAudit::new(pocket));
        // Start opens the menu only once the scene has settled, as walking
        // does: pressed during a scripted pause it lands mid-cutscene.
        if !audit.observed()
            && o.bag.is_none()
            && o.menu.is_none()
            && self.quiet_frames < SETTLE_FRAMES
        {
            return Decision::Wait("letting the scene settle before the bag".into());
        }
        match audit.next(o, &data, ctx.events) {
            Decision::Done(summary) => {
                ctx.events.push(GameEvent::GoalProgress {
                    goal: "Story".into(),
                    phase: "Bag".into(),
                    detail: summary,
                });
                self.advance_step(ctx);
                Decision::Wait("pocket audited".into())
            }
            decision => decision,
        }
    }

    /// Audits a stale or unknown Poké Balls pocket first, skips a full
    /// stock, and otherwise buys on the list (`count: 0`).
    fn stock_up(&mut self, mart: Option<String>, ctx: &mut TaskContext<'_>) -> Decision {
        let stale = ctx
            .state
            .bag
            .pockets
            .get(&Pocket::PokeBalls)
            .is_none_or(|k| k.needs_audit());
        if stale {
            self.splice(vec![StoryStep::AuditPocket(Pocket::PokeBalls)], true);
            return Decision::Wait("auditing the Poké Balls before buying".into());
        }
        let stock = ball_count(ctx.state);
        if !should_buy(stock) {
            ctx.events.push(GameEvent::GoalProgress {
                goal: "Story".into(),
                phase: "Mart".into(),
                detail: format!("{} Poké Balls: no need to buy", stock.unwrap_or(0)),
            });
            self.advance_step(ctx);
            return Decision::Wait("stocked up".into());
        }
        self.splice(
            vec![StoryStep::Buy {
                item: "ITEM_POKE_BALL".into(),
                count: 0,
                mart,
            }],
            false,
        );
        Decision::Wait("going to buy Poké Balls".into())
    }

    /// Walks to the mart's clerk and talks (the purchase takes over once
    /// the clerk's text shows).
    fn go_to_clerk(
        &mut self,
        item: &str,
        count: u16,
        mart: Option<&str>,
        o: &Observation,
        ctx: &mut TaskContext<'_>,
    ) -> Decision {
        let Some(data) = self.data.clone() else {
            return Decision::Fail("buying needs game data".into());
        };
        let Some(pose) = &o.player else {
            return Decision::Wait("locating".into());
        };
        if self.buy_at.is_none() {
            self.buy_at = match mart {
                Some(map) => self.world.map(map).and_then(|m| {
                    m.objects
                        .iter()
                        .filter(|ob| ob.graphics.as_deref() == Some("OBJ_EVENT_GFX_CLERK"))
                        .map(|ob| (map.to_owned(), ob.local_id))
                        .min_by_key(|(_, id)| *id)
                }),
                None => nearest_mart(&self.world, &data, &pose.pose, item, &self.taken),
            };
        }
        let Some((map, clerk)) = self.buy_at.clone() else {
            if count == 0 {
                // A stock-up (mandatory or not) goes on without balls.
                ctx.events.push(GameEvent::GoalProgress {
                    goal: "Story".into(),
                    phase: "Mart".into(),
                    detail: format!("no mart selling {item} found: not buying"),
                });
                self.advance_step(ctx);
                return Decision::Wait("no mart".into());
            }
            return Decision::Fail(format!("no mart selling {item} found"));
        };
        self.talk_to(&map, clerk, false, o, ctx)
    }

    /// Runs the purchase until the mart is left.
    fn shop(
        &mut self,
        item: &str,
        count: u16,
        o: &Observation,
        ctx: &mut TaskContext<'_>,
    ) -> Decision {
        let Some(data) = self.data.clone() else {
            return Decision::Fail("buying needs game data".into());
        };
        let stock = ball_count(ctx.state);
        let purchase = self
            .purchase
            .get_or_insert_with(|| Purchase::new(item, count).with_stock(stock));
        match purchase.next(o, &data, ctx.events) {
            Decision::Done(summary) => {
                ctx.events.push(GameEvent::GoalProgress {
                    goal: "Story".into(),
                    phase: "Mart".into(),
                    detail: summary,
                });
                self.advance_step(ctx);
                Decision::Wait("left the mart".into())
            }
            decision => decision,
        }
    }

    fn go(&mut self, dest: &Destination, o: &Observation, ctx: &mut TaskContext<'_>) -> Decision {
        match self.navigate(dest, o) {
            NavStatusOrDecision::Decision(d) => d,
            NavStatusOrDecision::Nav(NavStatus::Arrived) => {
                self.advance_step(ctx);
                Decision::Wait("arrived".into())
            }
            NavStatusOrDecision::Nav(status) => nav_decision(status),
        }
    }

    fn talk_to(
        &mut self,
        map: &str,
        object: u32,
        challenge: bool,
        o: &Observation,
        ctx: &mut TaskContext<'_>,
    ) -> Decision {
        match self.talk {
            TalkPhase::Approach => {
                if challenge {
                    let trainer = self
                        .data
                        .as_ref()
                        .and_then(|d| d.map_trainers.get(map))
                        .and_then(|list| list.iter().find(|t| t.local_id == object))
                        .map(|t| t.trainer.clone());
                    if let Some(d) = self.heal_before(trainer.as_deref(), "challenge", ctx.events) {
                        return d;
                    }
                }
                let Some((x, y)) = self
                    .world
                    .map(map)
                    .and_then(|m| m.objects.iter().find(|ob| ob.local_id == object))
                    .and_then(|ob| Some((ob.x?, ob.y?)))
                else {
                    return Decision::Fail(format!("{map} has no object {object}"));
                };
                match self.navigate(
                    &Destination::Facing {
                        map: map.to_owned(),
                        x,
                        y,
                    },
                    o,
                ) {
                    NavStatusOrDecision::Decision(d) => d,
                    NavStatusOrDecision::Nav(NavStatus::Arrived) => {
                        self.talk = TalkPhase::Press;
                        Decision::Wait("facing them".into())
                    }
                    NavStatusOrDecision::Nav(status) => nav_decision(status),
                }
            }
            TalkPhase::Press => {
                // Battles on the way (trainers who spot us) don't count as
                // the one this conversation is about.
                self.battle_seen = false;
                Decision::Act(Action::new(
                    "press A to talk",
                    vec![ControllerCommand::Press(Button::A)],
                    Expectation::DialogueOpen,
                    45,
                ))
            }
            TalkPhase::Talking => {
                if challenge && !self.battle_seen {
                    // The battle intro (swirl) can take a while after the
                    // last line of dialogue.
                    if self.quiet_frames >= 600 {
                        return Decision::Fail("talked, but no battle followed".into());
                    }
                    return Decision::Wait("waiting for the battle to start".into());
                }
                if self.quiet_frames >= SETTLE_FRAMES {
                    if matches!(self.current(), Some(StoryStep::Heal { .. })) {
                        ctx.events.extend(self.heal.finish());
                    }
                    // An item ball or fossil whose item we got is gone:
                    // its tile is free (the Helix Fossil opens the way
                    // north on MtMoon_B2F).
                    if self.talk_gained_item && vanishes_when_taken(&self.world, map, object) {
                        self.taken.insert((map.to_owned(), object));
                    }
                    self.advance_step(ctx);
                }
                Decision::Wait("conversation ending".into())
            }
        }
    }

    /// Training's heal rule: the lead's HP is below [`HEAL_BELOW`]
    /// (knowledge from the last battle), or it has too few attack PP left
    /// for wild battles (keeping the trainer reserve).
    fn lead_needs_heal(&self) -> bool {
        let Some(lead) = self.party.lead() else {
            return false;
        };
        let out_of_pp = self.data.as_ref().is_some_and(|data| {
            lead.wild_attack_pp(data, self.policy.wild_pp_reserve) < HEAL_BELOW_PP
        });
        out_of_pp
            || lead
                .hp
                .is_some_and(|(hp, max)| u32::from(hp) * 1000 < u32::from(max) * HEAL_BELOW)
    }

    /// The lead has a known major status (sleep, poison, burn, paralysis,
    /// freeze).
    fn lead_status_major(&self) -> Option<Status> {
        self.lead_status
            .filter(|s| !matches!(s, Status::Healthy | Status::Fainted))
    }

    /// Why the lead should heal before a planned battle against
    /// `trainer`, if it should: a major status, or P(win) for the lead alone
    /// (it fights; a faint fails the story), at its current HP with only
    /// moves that have PP left, below [`BATTLE_CONFIDENCE`] while a full heal
    /// would reach it. When even a healed lead stays below, healing can't
    /// help (and a wild battle after each heal would repeat it forever): it
    /// goes on, as the Prepare plan vouched for the battle, and says so once
    /// per step. Without a known trainer: below [`HEAL_BELOW`] HP. Never right
    /// after a heal with no battle since.
    fn unready_for(
        &mut self,
        trainer: Option<&str>,
        events: &mut Vec<GameEvent>,
    ) -> Option<String> {
        if self.just_healed {
            return None;
        }
        let lead = self.party.lead()?.clone();
        if let Some(status) = self.lead_status_major() {
            return Some(format!("{} is {status:?}", lead.display_name()));
        }
        let (hp, max) = lead.hp?;
        let (Some(trainer), Some(data)) = (trainer, self.data.clone()) else {
            return (u32::from(hp) * 1000 < u32::from(max) * HEAL_BELOW)
                .then(|| format!("{} at {hp}/{max} HP", lead.display_name()));
        };
        let p_now = lead_p_win(&data, &lead, trainer, false);
        if p_now >= BATTLE_CONFIDENCE {
            return None;
        }
        let p_full = lead_p_win(&data, &lead, trainer, true);
        if p_full < BATTLE_CONFIDENCE {
            if !self.readiness_warned {
                self.readiness_warned = true;
                events.push(GameEvent::GoalProgress {
                    goal: "Story".into(),
                    phase: "Warning".into(),
                    detail: format!(
                        "P(win) vs {trainer} is {p_full:.3} even healed (now {p_now:.3}): \
                         going on as planned"
                    ),
                });
            }
            return None;
        }
        Some(format!(
            "P(win) {p_now:.3} vs {trainer} at {hp}/{max} HP, {p_full:.3} healed"
        ))
    }

    /// Before a planned battle against `trainer`: heal when
    /// [`Self::unready_for`] says so, at most [`MAX_HEALS_PER_STEP`] times
    /// per story step. Past the cap the lead fights when its P(win) alone
    /// now is at least [`BATTLE_CONFIDENCE`] (a status the model can't
    /// weigh, say), and otherwise the step fails. `None`: go on.
    fn heal_before(
        &mut self,
        trainer: Option<&str>,
        what: &str,
        events: &mut Vec<GameEvent>,
    ) -> Option<Decision> {
        let reason = self.unready_for(trainer, events)?;
        if self.heals_this_step < MAX_HEALS_PER_STEP {
            self.heals_this_step += 1;
            events.push(GameEvent::GoalProgress {
                goal: "Story".into(),
                phase: "Heal".into(),
                detail: format!("healing before the {what}: {reason}"),
            });
            self.splice(vec![StoryStep::Heal { center: None }], true);
            return Some(Decision::Wait("healing before the battle".into()));
        }
        let p_now = match (trainer, self.data.clone(), self.party.lead()) {
            (Some(trainer), Some(data), Some(lead)) => {
                Some(lead_p_win(&data, lead, trainer, false))
            }
            _ => None,
        };
        match p_now {
            Some(p) if p >= BATTLE_CONFIDENCE => {
                if !std::mem::replace(&mut self.heal_cap_warned, true) {
                    events.push(GameEvent::GoalProgress {
                        goal: "Story".into(),
                        phase: "Warning".into(),
                        detail: format!(
                            "{MAX_HEALS_PER_STEP} heals already made in this step ({reason}); \
                             the lead alone has P(win) {p:.3} now: fighting"
                        ),
                    });
                }
                None
            }
            p => Some(Decision::Fail(format!(
                "not ready for the {what} after {MAX_HEALS_PER_STEP} heals in this step: \
                 {reason}{}",
                p.map(|p| format!(" (lead-only P(win) now {p:.3})"))
                    .unwrap_or_default()
            ))),
        }
    }

    /// Whether the last battle is the trigger's: it began with the player on
    /// one of the trigger map's trigger tiles, or it is a trainer battle
    /// that began within a tile of the trigger (a pose missed on the last
    /// step onto it).
    fn battle_was_the_triggers(&self, trigger: &Destination) -> bool {
        let Some(pose) = &self.battle_start_pose else {
            return false;
        };
        if pose.map != trigger.map() {
            return false;
        }
        let on_trigger = self
            .world
            .map(trigger.map())
            .is_some_and(|m| m.triggers.iter().any(|t| (t.x, t.y) == (pose.x, pose.y)));
        let near = match trigger {
            Destination::Tile { x, y, .. } => (pose.x - x).abs() + (pose.y - y).abs() <= 1,
            _ => false,
        };
        on_trigger || (self.battle_memory.trainer && near)
    }

    fn train(
        &mut self,
        map: &str,
        level: u8,
        o: &Observation,
        ctx: &mut TaskContext<'_>,
    ) -> Decision {
        let Some(lead) = self.party.lead().cloned() else {
            return Decision::Fail("no party knowledge".into());
        };
        if lead.level >= level {
            ctx.events.push(GameEvent::GoalProgress {
                goal: "Story".into(),
                phase: "Train".into(),
                detail: format!("{} reached Lv{}", lead.display_name(), lead.level),
            });
            self.advance_step(ctx);
            return Decision::Wait("training done".into());
        }
        // Heal before continuing when HP is low (knowledge from the last
        // battle) or wild battles have no attack PP left to spend.
        if self.lead_needs_heal() {
            self.splice(vec![StoryStep::Heal { center: None }], true);
            return Decision::Wait("going to heal".into());
        }
        let Some(pose) = o.player.as_ref().map(|p| p.pose.clone()) else {
            return Decision::Wait("locating".into());
        };
        if pose.map != map {
            let dest = self.world.map(map).and_then(|m| {
                let entry = (m.width / 2, m.height / 2);
                self.grass_spot(map, entry)
            });
            let Some(a) = dest else {
                return Decision::Fail(format!("no tall grass on {map}"));
            };
            return match self.navigate(
                &Destination::Tile {
                    map: map.to_owned(),
                    x: a.0,
                    y: a.1,
                },
                o,
            ) {
                NavStatusOrDecision::Decision(d) => d,
                NavStatusOrDecision::Nav(NavStatus::Arrived) => {
                    Decision::Wait("at the grass".into())
                }
                NavStatusOrDecision::Nav(status) => nav_decision(status),
            };
        }
        let target = match self.spin_at {
            Some(t) => t,
            None => {
                let Some(t) = self.grass_spot(map, (pose.x, pose.y)) else {
                    return Decision::Fail(format!("no tall grass on {map}"));
                };
                self.spin_at = Some(t);
                t
            }
        };
        if (pose.x, pose.y) == target {
            self.nav = None;
            return Decision::Act(
                Action::new(
                    "spin in the grass for encounters",
                    vec![ControllerCommand::Sequence(spin_sequence(SPIN_TURNS))],
                    Expectation::InputsDone,
                    10,
                )
                .interruptible(),
            );
        }
        match self.navigate(
            &Destination::Tile {
                map: map.to_owned(),
                x: target.0,
                y: target.1,
            },
            o,
        ) {
            NavStatusOrDecision::Decision(d) => d,
            NavStatusOrDecision::Nav(NavStatus::Arrived) => {
                Decision::Wait("at the spin tile".into())
            }
            NavStatusOrDecision::Nav(status) => nav_decision(status),
        }
    }
}

fn nav_decision(status: NavStatus) -> Decision {
    match status {
        NavStatus::Act(action) => Decision::Act(action),
        NavStatus::Wait(reason) => Decision::Wait(reason),
        NavStatus::Fail(reason) => Decision::Fail(reason),
        NavStatus::Arrived => Decision::Wait("arrived".into()),
    }
}

/// Which Poké Ball to take in Oak's lab (decomp object ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, clap::ValueEnum)]
pub enum Starter {
    Bulbasaur,
    Squirtle,
    Charmander,
}

impl Starter {
    pub fn species(self) -> &'static str {
        match self {
            Starter::Bulbasaur => "SPECIES_BULBASAUR",
            Starter::Squirtle => "SPECIES_SQUIRTLE",
            Starter::Charmander => "SPECIES_CHARMANDER",
        }
    }

    fn object(self) -> u32 {
        match self {
            Starter::Bulbasaur => 5,
            Starter::Squirtle => 6,
            Starter::Charmander => 7,
        }
    }
}

const HOUSE_2F: &str = "PalletTown_PlayersHouse_2F";
const HOUSE_1F: &str = "PalletTown_PlayersHouse_1F";
const PALLET: &str = "PalletTown";
const LAB: &str = "PalletTown_ProfessorOaksLab";

/// From the bedroom (right after a new game) to receiving the starter.
pub fn opening(starter: Starter) -> Vec<Milestone> {
    vec![
        Milestone::new(
            "LeaveBedroom",
            "take the stairs down",
            vec![StoryStep::Go(Destination::Warp { map: HOUSE_2F.into(), warp: 0 })],
        ),
        Milestone::new(
            "TalkToMom",
            "talk to Mom",
            vec![StoryStep::Talk { map: HOUSE_1F.into(), object: 1, answers: vec![] }],
        ),
        Milestone::new(
            "LeaveHouse",
            "walk out the front door",
            vec![StoryStep::Go(Destination::Warp { map: HOUSE_1F.into(), warp: 1 })],
        ),
        Milestone::new(
            "MeetOak",
            "head for Route 1 until Professor Oak stops us and takes us to his lab",
            vec![StoryStep::GoUntil {
                dest: Destination::Tile { map: PALLET.into(), x: 12, y: 1 },
                until: Condition::OnMap(LAB.into()),
            }],
        ),
        Milestone::new(
            "ChooseStarter",
            "take a Poké Ball from Oak's table (YES), no nickname (NO)",
            vec![
                StoryStep::Settle { frames: SETTLE_FRAMES },
                StoryStep::Talk { map: LAB.into(), object: starter.object(), answers: vec![Answer::Yes, Answer::No] },
                // The rival walks to the table and takes his ball.
                StoryStep::Settle { frames: 240 },
            ],
        ),
        Milestone::new(
            "RivalBattle",
            "head for the exit; the rival challenges us",
            vec![
                StoryStep::Battle {
                    trigger: Destination::Tile { map: LAB.into(), x: 6, y: 8 },
                    loss_ok: true,
                    trainer: None,
                },
                StoryStep::Settle { frames: 180 },
            ],
        ),
        Milestone::new(
            "HealAtHome",
            "go home and talk to Mom, who heals the party",
            vec![StoryStep::Talk { map: HOUSE_1F.into(), object: 1, answers: vec![] }],
        ),
        Milestone::new(
            "GetParcel",
            "walk Route 1 to Viridian City and enter the Poké Mart; the clerk hands over Oak's Parcel",
            vec![
                StoryStep::Go(Destination::Warp { map: "ViridianCity".into(), warp: 4 }),
                StoryStep::Settle { frames: SETTLE_FRAMES },
            ],
        ),
        Milestone::new(
            "DeliverParcel",
            "take the parcel back to Professor Oak in Pallet Town",
            vec![
                StoryStep::Talk { map: LAB.into(), object: 4, answers: vec![] },
                StoryStep::Settle { frames: 240 },
            ],
        ),
    ]
}

/// Viridian Forest trainers (all are included as planning targets; some can
/// be walked past, but the plan shouldn't depend on that).
const FOREST_TRAINERS: [&str; 5] = [
    "TRAINER_BUG_CATCHER_RICK",
    "TRAINER_BUG_CATCHER_DOUG",
    "TRAINER_BUG_CATCHER_SAMMY",
    "TRAINER_BUG_CATCHER_ANTHONY",
    "TRAINER_BUG_CATCHER_CHARLIE",
];

/// From the Pokédex to the Boulder Badge: plan and train, cross Viridian
/// Forest to Pewter City, then beat the gym.
pub fn to_brock() -> Vec<Milestone> {
    let mut targets: Vec<String> = FOREST_TRAINERS.iter().map(|t| (*t).to_owned()).collect();
    targets.push("TRAINER_CAMPER_LIAM".into());
    targets.push("TRAINER_LEADER_BROCK".into());
    vec![
        Milestone::new(
            "PrepareForBrock",
            "ask the planner what it takes to beat Viridian Forest and Brock, then train",
            vec![
                StoryStep::Prepare {
                    targets,
                    areas: ["Route1", "Route22"]
                        .iter()
                        .map(|a| (*a).to_owned())
                        .collect(),
                    confidence: 0.9,
                },
                StoryStep::Heal { center: None },
            ],
        ),
        Milestone::new(
            "ReachPewter",
            "Route 2 and Viridian Forest to Pewter City; heal at its Pokémon Center",
            vec![StoryStep::Heal {
                center: Some("PewterCity_PokemonCenter_1F".into()),
            }],
        ),
        Milestone::new(
            "BeatBrock",
            "challenge Brock in the Pewter Gym and win",
            vec![
                StoryStep::Challenge {
                    map: "PewterCity_Gym".into(),
                    object: 1,
                },
                StoryStep::Settle { frames: 180 },
            ],
        ),
    ]
}

const ROUTE3_TRAINERS: [&str; 8] = [
    "TRAINER_LASS_ROBIN",
    "TRAINER_BUG_CATCHER_JAMES",
    "TRAINER_LASS_SALLY",
    "TRAINER_BUG_CATCHER_GREG",
    "TRAINER_YOUNGSTER_CALVIN",
    "TRAINER_LASS_JANICE",
    "TRAINER_BUG_CATCHER_COLTON",
    "TRAINER_YOUNGSTER_BEN",
];

/// From the Boulder Badge toward Mt. Moon: get ready for Route 3's trainers.
pub fn to_mt_moon() -> Vec<Milestone> {
    vec![Milestone::new(
        "PrepareForRoute3",
        "ask the planner what it takes to beat Route 3's trainers, then train",
        vec![
            StoryStep::Prepare {
                targets: ROUTE3_TRAINERS.iter().map(|t| (*t).to_owned()).collect(),
                areas: ["Route22", "Route2", "ViridianForest", "Route1"]
                    .iter()
                    .map(|a| (*a).to_owned())
                    .collect(),
                confidence: 0.9,
            },
            StoryStep::Heal {
                center: Some("PewterCity_PokemonCenter_1F".into()),
            },
        ],
    )]
}

/// Objects that disappear once their item is taken: item balls and fossils.
fn vanishes_when_taken(world: &World, map: &str, object: u32) -> bool {
    world
        .map(map)
        .and_then(|m| m.objects.iter().find(|o| o.local_id == object))
        .and_then(|o| o.graphics.as_deref())
        .is_some_and(|g| matches!(g, "OBJ_EVENT_GFX_ITEM_BALL" | "OBJ_EVENT_GFX_FOSSIL"))
}

/// Mt. Moon's trainers: all are planning targets (some can be walked past,
/// but the plan shouldn't depend on that).
const MT_MOON_TRAINERS: [&str; 12] = [
    "TRAINER_LASS_IRIS",
    "TRAINER_BUG_CATCHER_ROBBY",
    "TRAINER_SUPER_NERD_JOVAN",
    "TRAINER_LASS_MIRIAM",
    "TRAINER_BUG_CATCHER_KENT",
    "TRAINER_YOUNGSTER_JOSH",
    "TRAINER_HIKER_MARCOS",
    // Floor trigger at MtMoon_B2F (14, 11); not in map_trainers.
    "TRAINER_SUPER_NERD_MIGUEL",
    "TRAINER_TEAM_ROCKET_GRUNT",
    "TRAINER_TEAM_ROCKET_GRUNT_2",
    "TRAINER_TEAM_ROCKET_GRUNT_3",
    "TRAINER_TEAM_ROCKET_GRUNT_4",
];

const ROUTE4_CENTER: &str = "Route4_PokemonCenter_1F";
const CERULEAN_CENTER: &str = "CeruleanCity_PokemonCenter_1F";

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// From Pewter City (Boulder Badge, ready for Route 3) to the Cascade Badge:
/// stock up on balls, cross Route 3 and Mt. Moon (Miguel, the Helix Fossil),
/// then Cerulean City and Misty.
///
/// Training areas are the ones reachable without Nugget Bridge: Route 24's
/// grass (and Route 25 beyond it) is only reachable across the bridge's
/// trainers, which are out of scope.
pub fn to_cerulean() -> Vec<Milestone> {
    vec![
        Milestone::new(
            "StockUpPewter",
            "buy Poké Balls at the Pewter Mart before Route 3",
            vec![StoryStep::StockUp {
                mart: Some("PewterCity_Mart".into()),
            }],
        ),
        Milestone::new(
            "CrossRoute3",
            "cross Route 3 (its trainers can't be walked around) to the Route 4 Pokémon Center",
            vec![StoryStep::Heal {
                center: Some(ROUTE4_CENTER.into()),
            }],
        ),
        Milestone::new(
            "PrepareForMtMoon",
            "ask the planner what it takes to beat Mt. Moon's trainers, then train",
            vec![
                StoryStep::Prepare {
                    targets: names(&MT_MOON_TRAINERS),
                    areas: names(&["Route3", "Route4", "MtMoon_1F"]),
                    confidence: 0.9,
                },
                StoryStep::Heal {
                    center: Some(ROUTE4_CENTER.into()),
                },
            ],
        ),
        Milestone::new(
            "CrossMtMoon",
            "beat Miguel, take the Helix Fossil and leave Mt. Moon for Route 4",
            vec![
                // Miguel: the floor trigger in front of the fossils.
                StoryStep::Battle {
                    trigger: Destination::Tile {
                        map: "MtMoon_B2F".into(),
                        x: 14,
                        y: 11,
                    },
                    loss_ok: false,
                    trainer: Some("TRAINER_SUPER_NERD_MIGUEL".into()),
                },
                // The Helix Fossil: "You want the HELIX FOSSIL?" → YES.
                StoryStep::Talk {
                    map: "MtMoon_B2F".into(),
                    object: 2,
                    answers: vec![Answer::Yes],
                },
                // Miguel walks up to the Dome Fossil and takes it.
                StoryStep::Settle { frames: 180 },
                StoryStep::Go(Destination::Warp {
                    map: "MtMoon_B1F".into(),
                    warp: 7,
                }),
            ],
        ),
        Milestone::new(
            "ReachCerulean",
            "Route 4 to Cerulean City; heal and stock up on Poké Balls",
            vec![
                StoryStep::Heal {
                    center: Some(CERULEAN_CENTER.into()),
                },
                StoryStep::StockUp {
                    mart: Some("CeruleanCity_Mart".into()),
                },
            ],
        ),
        Milestone::new(
            "PrepareForMisty",
            "ask the planner what it takes to beat the Cerulean Gym, then train",
            vec![
                StoryStep::Prepare {
                    targets: names(&[
                        "TRAINER_SWIMMER_MALE_LUIS",
                        "TRAINER_PICNICKER_DIANA",
                        "TRAINER_LEADER_MISTY",
                    ]),
                    // Route24/Route25 dropped: their grass is behind the
                    // Nugget Bridge trainers.
                    areas: names(&["Route4", "Route3", "MtMoon_1F"]),
                    confidence: 0.9,
                },
                StoryStep::Heal {
                    center: Some(CERULEAN_CENTER.into()),
                },
            ],
        ),
        Milestone::new(
            "BeatMisty",
            "challenge Misty in the Cerulean Gym and win",
            vec![
                StoryStep::Challenge {
                    map: "CeruleanCity_Gym".into(),
                    object: 3,
                },
                StoryStep::Settle { frames: 180 },
            ],
        ),
    ]
}

/// Every milestone implemented so far, in order.
pub fn all_milestones(starter: Starter) -> Vec<Milestone> {
    let mut list = opening(starter);
    list.extend(to_brock());
    list.extend(to_mt_moon());
    list.extend(to_cerulean());
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `#` grass, `.` other.
    fn grid<'a>(rows: &'a [&'a str]) -> impl Fn(i32, i32) -> bool + 'a {
        move |x, y| {
            usize::try_from(y)
                .ok()
                .and_then(|y| rows.get(y))
                .zip(usize::try_from(x).ok())
                .is_some_and(|(row, x)| row.as_bytes().get(x) == Some(&b'#'))
        }
    }

    #[test]
    fn spin_tile_prefers_grass_surrounded_by_grass() {
        let g = grid(&["#.......", "........", "....###.", "....###.", "....###."]);
        // The centre of the 3×3 patch beats the lone tile next to us.
        assert_eq!(spin_tile(&g, 8, 5, (0, 0)), Some((5, 3)));
        assert_eq!(spin_tile(&grid(&["...."]), 4, 1, (0, 0)), None);
    }

    fn tile(collision: u8, behavior: u16, encounter: u8) -> pokebot_world::Tile {
        pokebot_world::Tile {
            collision,
            elevation: 3,
            behavior,
            encounter,
        }
    }

    #[test]
    fn cave_floor_with_encounters_counts_for_training() {
        // Mt. Moon: MB_CAVE floor (0x08) with land encounters, and plain floor.
        assert!(encounter_tile(&tile(0, 0x08, 1)));
        assert!(encounter_tile(&tile(0, 0x00, 1)));
        assert!(encounter_tile(&tile(0, TALL_GRASS, 1)));
        // Blocked, water encounters, no encounters, ladders.
        assert!(!encounter_tile(&tile(1, 0x08, 1)));
        assert!(!encounter_tile(&tile(0, 0x15, 2)));
        assert!(!encounter_tile(&tile(0, 0x00, 0)));
        assert!(!encounter_tile(&tile(0, 0x61, 1)));
    }

    fn catch(species: &str, map: &str) -> PlanStep {
        PlanStep::Catch {
            species: species.into(),
            map: map.into(),
            level: 8,
            minutes: 3.0,
            balls: 2,
        }
    }

    fn train_plan(species: &str, to: u8, map: &str) -> PlanStep {
        PlanStep::Train {
            species: species.into(),
            from: 14,
            to,
            map: map.into(),
            minutes: 5.0,
            battles: 10,
        }
    }

    #[test]
    fn lead_training_becomes_train_steps() {
        let steps = plan_to_steps(
            &[train_plan("SPECIES_BULBASAUR", 16, "Route3")],
            "SPECIES_BULBASAUR",
            || panic!("no catch: the lead-only plan is not needed"),
        );
        assert_eq!(
            steps,
            Ok(vec![StoryStep::Train {
                map: "Route3".into(),
                level: 16
            }])
        );
    }

    #[test]
    fn catch_step_trains_the_lead_there_when_the_lead_alone_is_enough() {
        let steps = plan_to_steps(
            &[
                catch("SPECIES_ZUBAT", "MtMoon_1F"),
                train_plan("SPECIES_ZUBAT", 12, "MtMoon_1F"),
                train_plan("SPECIES_BULBASAUR", 16, "Route3"),
            ],
            "SPECIES_BULBASAUR",
            || Ok(17),
        );
        assert_eq!(
            steps,
            Ok(vec![
                StoryStep::Train {
                    map: "MtMoon_1F".into(),
                    level: 17
                },
                StoryStep::Train {
                    map: "Route3".into(),
                    level: 16
                },
            ])
        );
    }

    #[test]
    fn catch_step_fails_when_the_lead_alone_falls_short() {
        let steps = plan_to_steps(
            &[catch("SPECIES_ZUBAT", "MtMoon_1F")],
            "SPECIES_BULBASAUR",
            || Err("the lead alone only reaches 80%".into()),
        );
        let err = steps.unwrap_err();
        assert!(err.contains("only reaches 80%"), "{err}");
    }

    const MIGUEL: &str = "TRAINER_SUPER_NERD_MIGUEL";

    /// Review (critical): the planner combined P(win) over the whole party,
    /// so strong catches made a weak lead "already ready" — but only the
    /// lead fights ("change POKéMON?" is NO; a lead faint fails the story).
    #[test]
    fn a_strong_party_does_not_make_a_weak_lead_ready() {
        let Some((world, data)) = story_fixture() else {
            return;
        };
        // IVYSAUR Lv18 with its default moves stays below 0.9 vs Miguel even
        // healed; four Lv50 PIDGEOT behind it win for sure.
        let lead = party::Member::new(&data, "SPECIES_IVYSAUR", 18);
        let mut members = vec![lead.clone()];
        for slot in 1..5 {
            members.push(party::Member {
                slot,
                ..party::Member::new(&data, "SPECIES_PIDGEOT", 50)
            });
        }
        let whole: Vec<_> = members
            .iter()
            .filter_map(|m| {
                pokebot_planner::Combatant::new(&data, &m.species, m.level, m.moves.clone(), OUR_IV)
            })
            .collect();
        let p_party = pokebot_planner::battle_vs_trainer(&data, &whole, MIGUEL).map(|e| e.p_win);
        assert!(p_party.is_some_and(|p| p >= 0.9), "{p_party:?}");
        assert!(lead_p_win(&data, &lead, MIGUEL, true) < 0.9);
        let mut task = one_milestone(world, data, Vec::new());
        task.party = Party { members };
        match task.plan(&[MIGUEL.into()], &["MtMoon_1F".into()], 0.9) {
            Ok((steps, p_win)) => {
                assert!(!steps.is_empty(), "already ready with {p_win:?}");
                assert!(steps
                    .iter()
                    .all(|s| matches!(s, StoryStep::Train { level, .. } if *level > 18)));
            }
            Err(e) => assert!(!e.is_empty()),
        }
    }

    /// "already ready" says the lead-only P(win) per target.
    #[test]
    fn already_ready_logs_the_lead_only_p_win() {
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let state = party_state(&data, "SPECIES_IVYSAUR", 40);
        let mut task = one_milestone(
            world,
            data,
            vec![
                StoryStep::Prepare {
                    targets: vec![MIGUEL.into()],
                    areas: vec!["MtMoon_1F".into()],
                    confidence: 0.9,
                },
                StoryStep::Settle { frames: 100_000 },
            ],
        );
        let (label, events) = tick_events(&mut task, &located(1, "MtMoon_1F", 18, 36), &state);
        assert_eq!(label, "wait: planned");
        let detail = events.iter().find_map(|e| match e {
            GameEvent::GoalProgress { phase, detail, .. } if phase == "Plan" => Some(detail),
            _ => None,
        });
        let detail = detail.expect("a Plan event");
        assert!(
            detail.starts_with("already ready (lead alone): P(win) vs TRAINER_SUPER_NERD_MIGUEL "),
            "{detail}"
        );
    }

    #[test]
    fn training_another_member_still_fails() {
        let steps = plan_to_steps(
            &[train_plan("SPECIES_PIDGEY", 12, "Route3")],
            "SPECIES_BULBASAUR",
            || Ok(20),
        );
        assert!(steps.is_err());
    }

    #[test]
    fn spin_sequence_never_repeats_a_direction() {
        let seq = spin_sequence(9);
        assert_eq!(seq.len(), 9);
        for pair in seq.windows(2) {
            assert_ne!(pair[0].buttons, pair[1].buttons);
            // Shorter than a turn (8 frames at 59.7 Hz = 134 ms).
            assert!(pair[0].duration.as_millis() < 134);
        }
    }

    #[test]
    fn audit_pocket_step_reads_the_pocket_and_moves_on() {
        use pokebot_state::{
            BagObservation, GameState, MenuObservation, Observed, PlayerPose, PoseObservation,
            Region,
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(world), Ok(data)) = (
            World::load(root.join("data/world")),
            GameData::load(root.join("data/world/gamedata.json")),
        ) else {
            return;
        };
        let mut task = StoryTask::new(
            Arc::new(world),
            vec![Milestone::new(
                "Audit",
                "read the POKé BALLS pocket",
                vec![
                    StoryStep::AuditPocket(Pocket::PokeBalls),
                    StoryStep::Settle { frames: 1 },
                ],
            )],
        )
        .with_data(Arc::new(data));
        let bare = |frame, value| {
            Observation::bare(
                frame,
                Observed {
                    value,
                    detector: "test".into(),
                },
                Default::default(),
            )
        };
        let overworld = |frame| {
            let mut o = bare(frame, ScreenState::Unknown);
            o.player = Some(PoseObservation {
                pose: PlayerPose {
                    map: "PewterCity".into(),
                    x: 17,
                    y: 26,
                },
                score: 980,
            });
            o
        };
        let mut start = bare(2, ScreenState::Menu);
        start.menu = Some(MenuObservation {
            window: Region::new(174, 6, 60, 108),
            rows: 6,
            cursor_row: 2,
            cursor_y: 40,
        });
        start.menu_lines = ["POKéDEX", "POKéMON", "BAG", "RED", "SAVE", "OPTION", "EXIT"]
            .map(String::from)
            .to_vec();
        let mut bag = bare(3, ScreenState::Bag);
        bag.bag = Some(BagObservation {
            pocket: "POKé BALLS".into(),
            rows: vec![("POKé BALL".into(), Some(5)), ("CANCEL".into(), None)],
            cursor: Some(0),
            prompt: None,
        });
        let state = GameState::default();
        let mut labels = Vec::new();
        let mut events = Vec::new();
        let mut bag_again = bag.clone();
        bag_again.frame_id = 4;
        for o in [
            overworld(1),
            start.clone(),
            bag,
            bag_again,
            start,
            overworld(5),
        ] {
            let decision = task.next(&mut TaskContext {
                observation: &o,
                state: &state,
                events: &mut events,
            });
            labels.push(match decision {
                Decision::Act(a) => a.label,
                Decision::Wait(r) => format!("wait: {r}"),
                Decision::Done(r) => format!("done: {r}"),
                Decision::Fail(r) => panic!("failed: {r}"),
            });
        }
        assert_eq!(
            labels,
            [
                "open the Start menu",
                "open the BAG",
                "wait: confirming the rows on a later frame",
                "close the bag",
                "close the Start menu",
                "wait: pocket audited",
            ]
        );
        let observed: Vec<&GameEvent> = events
            .iter()
            .filter(|e| matches!(e, GameEvent::PocketObserved { .. }))
            .collect();
        assert_eq!(
            observed,
            [&GameEvent::PocketObserved {
                pocket: Pocket::PokeBalls,
                items: vec![("ITEM_POKE_BALL".into(), 5)],
            }]
        );
        assert_eq!(task.current(), Some(&StoryStep::Settle { frames: 1 }));
    }

    #[test]
    fn audit_waits_for_the_scene_to_settle_before_pressing_start() {
        use pokebot_state::{GameState, Observed, PlayerPose, PoseObservation};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(world), Ok(data)) = (
            World::load(root.join("data/world")),
            GameData::load(root.join("data/world/gamedata.json")),
        ) else {
            return;
        };
        let mut task = StoryTask::new(
            Arc::new(world),
            vec![Milestone::new(
                "Audit",
                "read the POKé BALLS pocket",
                vec![StoryStep::AuditPocket(Pocket::PokeBalls)],
            )],
        )
        .with_data(Arc::new(data));
        // A conversation just ended (a scripted pause may follow).
        task.quiet_frames = 0;
        let state = GameState::default();
        let mut events = Vec::new();
        let mut first_start = None;
        for frame in 0..200u64 {
            let mut o = Observation::bare(
                frame,
                Observed {
                    value: ScreenState::Unknown,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.player = Some(PoseObservation {
                pose: PlayerPose {
                    map: "PewterCity".into(),
                    x: 17,
                    y: 26,
                },
                score: 980,
            });
            let decision = task.next(&mut TaskContext {
                observation: &o,
                state: &state,
                events: &mut events,
            });
            if let Decision::Act(a) = decision {
                assert_eq!(a.label, "open the Start menu");
                first_start = Some(frame);
                break;
            }
        }
        // quiet_frames counts one per observation: the first press comes
        // once SETTLE_FRAMES quiet observations have passed.
        assert_eq!(first_start, Some(u64::from(SETTLE_FRAMES) - 1));
    }

    #[test]
    fn heal_is_inferred_when_the_nurse_text_was_missed() {
        let mut watch = HealWatch::default();
        let events = watch.finish();
        assert_eq!(events[0], GameEvent::Healed);
        assert!(matches!(
            &events[1],
            GameEvent::GoalProgress { detail, .. }
                if detail == "heal inferred: the nurse's text was not read"
        ));
    }

    #[test]
    fn heal_seen_in_text_is_not_repeated() {
        let mut watch = HealWatch::default();
        watch.observe(&[GameEvent::MoneyChanged {
            delta: 1,
            reason: "x".into(),
        }]);
        watch.observe(&[GameEvent::Healed]);
        assert!(watch.finish().is_empty());
        // The next Heal step starts over.
        watch.reset();
        assert_eq!(watch.finish()[0], GameEvent::Healed);
    }

    fn story_fixture() -> Option<(World, GameData)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some((
            World::load(root.join("data/world")).ok()?,
            GameData::load(root.join("data/world/gamedata.json")).ok()?,
        ))
    }

    fn located(frame: u64, map: &str, x: i32, y: i32) -> Observation {
        use pokebot_state::{Observed, PlayerPose, PoseObservation};
        let mut o = Observation::bare(
            frame,
            Observed {
                value: ScreenState::Unknown,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.player = Some(PoseObservation {
            pose: PlayerPose {
                map: map.into(),
                x,
                y,
            },
            score: 980,
        });
        o
    }

    fn with_balls(n: u16) -> pokebot_state::GameState {
        use pokebot_state::{DefaultReducer, EventRecord, GameState, StateReducer};
        DefaultReducer.reduce(
            &GameState::default(),
            &[EventRecord {
                frame_id: 1,
                event: GameEvent::PocketObserved {
                    pocket: Pocket::PokeBalls,
                    items: vec![("ITEM_POKE_BALL".into(), n)],
                },
            }],
        )
    }

    fn tick(task: &mut StoryTask, o: &Observation, state: &pokebot_state::GameState) -> String {
        let mut events = Vec::new();
        match task.next(&mut TaskContext {
            observation: o,
            state,
            events: &mut events,
        }) {
            Decision::Act(a) => a.label,
            Decision::Wait(r) => format!("wait: {r}"),
            Decision::Done(r) => format!("done: {r}"),
            Decision::Fail(r) => format!("fail: {r}"),
        }
    }

    fn one_milestone(world: World, data: GameData, steps: Vec<StoryStep>) -> StoryTask {
        StoryTask::new(Arc::new(world), vec![Milestone::new("Test", "test", steps)])
            .with_data(Arc::new(data))
    }

    #[test]
    fn stock_up_audits_a_stale_or_unknown_pocket_first() {
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let mut task = one_milestone(world, data, vec![StoryStep::StockUp { mart: None }]);
        let state = pokebot_state::GameState::default();
        tick(&mut task, &located(1, "PewterCity", 17, 26), &state);
        assert_eq!(
            task.current(),
            Some(&StoryStep::AuditPocket(Pocket::PokeBalls))
        );
        assert_eq!(
            task.milestones[0].steps[1],
            StoryStep::StockUp { mart: None }
        );
    }

    #[test]
    fn stock_up_skips_a_full_stock_and_buys_otherwise() {
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let steps = vec![
            StoryStep::StockUp { mart: None },
            StoryStep::Settle { frames: 1 },
        ];
        let mut task = one_milestone(world, data, steps.clone());
        tick(
            &mut task,
            &located(1, "PewterCity", 17, 26),
            &with_balls(15),
        );
        assert_eq!(task.current(), Some(&StoryStep::Settle { frames: 1 }));

        let Some((world, data)) = story_fixture() else {
            return;
        };
        let mut task = one_milestone(world, data, steps);
        tick(&mut task, &located(1, "PewterCity", 17, 26), &with_balls(3));
        assert_eq!(
            task.current(),
            Some(&StoryStep::Buy {
                item: "ITEM_POKE_BALL".into(),
                count: 0,
                mart: None,
            })
        );
    }

    /// Review: StockUp's mart lived in a task field that every splice
    /// reset, so a heal on the way sent the Buy to the nearest mart.
    #[test]
    fn stock_up_mart_survives_a_splice() {
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let steps = vec![
            StoryStep::StockUp {
                mart: Some("ViridianCity_Mart".into()),
            },
            StoryStep::Settle { frames: 1 },
        ];
        let mut task = one_milestone(world, data, steps);
        let state = with_balls(3);
        tick(&mut task, &located(1, "PewterCity", 17, 26), &state);
        let buy = StoryStep::Buy {
            item: "ITEM_POKE_BALL".into(),
            count: 0,
            mart: Some("ViridianCity_Mart".into()),
        };
        assert_eq!(task.current(), Some(&buy));
        // A heal is spliced in on the way, and done.
        task.splice(vec![StoryStep::Heal { center: None }], true);
        let mut events = Vec::new();
        task.advance_step(&mut TaskContext {
            observation: &located(2, "PewterCity", 17, 26),
            state: &state,
            events: &mut events,
        });
        assert_eq!(task.current(), Some(&buy));
        tick(&mut task, &located(3, "PewterCity", 17, 26), &state);
        assert_eq!(task.buy_at, Some(("ViridianCity_Mart".into(), 1)));
    }

    #[test]
    fn mandatory_buy_is_tried_once_per_milestone() {
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let go = StoryStep::Go(Destination::Tile {
            map: "PewterCity".into(),
            x: 20,
            y: 26,
        });
        let mut task = one_milestone(world, data, vec![go.clone()]);
        let state = with_balls(2);
        tick(&mut task, &located(1, "PewterCity", 17, 26), &state);
        assert_eq!(task.current(), Some(&StoryStep::StockUp { mart: None }));
        // The buy ended (say it bought nothing): the Go step runs, no retry.
        let mut events = Vec::new();
        task.advance_step(&mut TaskContext {
            observation: &located(2, "PewterCity", 17, 26),
            state: &state,
            events: &mut events,
        });
        assert_eq!(task.current(), Some(&go));
        tick(&mut task, &located(3, "PewterCity", 17, 26), &state);
        assert_eq!(task.current(), Some(&go));
        // Enough balls: no buy at all.
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let mut task = one_milestone(world, data, vec![go.clone()]);
        tick(&mut task, &located(1, "PewterCity", 17, 26), &with_balls(5));
        assert_eq!(task.current(), Some(&go));
    }

    #[test]
    fn buy_step_hands_the_mart_menu_to_the_purchase() {
        use pokebot_state::{DialogueKind, DialogueObservation, MenuObservation, Region};
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let mut task = one_milestone(
            world,
            data,
            vec![StoryStep::Buy {
                item: "ITEM_POKE_BALL".into(),
                count: 3,
                mart: None,
            }],
        );
        let mut o = located(1, "PewterCity_Mart", 4, 3);
        o.player = None;
        o.screen.value = ScreenState::Dialogue;
        o.dialogue = Some(DialogueObservation {
            kind: DialogueKind::MessageBox,
            region: Region::new(8, 118, 224, 36),
            waiting_for_input: false,
            arrow: None,
            stable_frames: 60,
            text_cells: vec![1; 4],
            lines: vec!["Hi, there!".into(), "May I help you?".into()],
            help: false,
        });
        o.menu = Some(MenuObservation {
            window: Region::new(14, 6, 100, 52),
            rows: 3,
            cursor_row: 0,
            cursor_y: 12,
        });
        o.menu_lines = ["BUY", "SELL", "SEE YA!"].map(String::from).to_vec();
        // Not "unexpected question": the purchase answers the mart menu.
        assert_eq!(
            tick(&mut task, &o, &pokebot_state::GameState::default()),
            "mart: BUY"
        );
    }
    fn tick_events(
        task: &mut StoryTask,
        o: &Observation,
        state: &pokebot_state::GameState,
    ) -> (String, Vec<GameEvent>) {
        let mut events = Vec::new();
        let label = match task.next(&mut TaskContext {
            observation: o,
            state,
            events: &mut events,
        }) {
            Decision::Act(a) => a.label,
            Decision::Wait(r) => format!("wait: {r}"),
            Decision::Done(r) => format!("done: {r}"),
            Decision::Fail(r) => format!("fail: {r}"),
        };
        (label, events)
    }

    /// IVYSAUR Lv18 as the only party member, and 10 Poké Balls.
    fn catch_state(data: &GameData) -> pokebot_state::GameState {
        use pokebot_state::{DefaultReducer, EventRecord, StateReducer};
        let mut mon = party::starter_mon(data, "SPECIES_IVYSAUR", 18);
        mon.hp = pokebot_state::Knowledge::observed((54, 54), 1);
        DefaultReducer.reduce(
            &with_balls(10),
            &[EventRecord {
                frame_id: 1,
                event: GameEvent::PartyMonDerived {
                    slot: 0,
                    mon: Box::new(mon),
                },
            }],
        )
    }

    /// A wild-battle frame against PIDGEY Lv6 (not caught yet).
    fn wild_frame(
        frame: u64,
        menu: Option<pokebot_state::BattleMenu>,
        text: &[&str],
    ) -> Observation {
        use pokebot_state::{
            BattleObservation, DialogueKind, DialogueObservation, Observed, Region,
        };
        let mut o = Observation::bare(
            frame,
            Observed {
                value: ScreenState::BattleText,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.battle = Some(BattleObservation {
            menu,
            player_name: Some("IVYSAUR".into()),
            player_level: Some(18),
            player_hp_numbers: Some((54, 54)),
            opponent_name: Some("PIDGEY".into()),
            opponent_level: Some(6),
            player_hp: Some(1000),
            opponent_hp: Some(1000),
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: Some(false),
            opponent_shiny: menu.map(|_| pokebot_state::ShinyReading::Normal),
        });
        if !text.is_empty() {
            o.dialogue = Some(DialogueObservation {
                kind: DialogueKind::BattleText,
                region: Region::new(8, 119, 224, 34),
                waiting_for_input: false,
                arrow: None,
                stable_frames: 10,
                text_cells: vec![1; 4],
                lines: text.iter().map(|l| (*l).to_owned()).collect(),
                help: false,
            });
        }
        o
    }

    /// `species` at `level` as the only party member (full HP).
    fn party_state(data: &GameData, species: &str, level: u8) -> pokebot_state::GameState {
        use pokebot_state::{DefaultReducer, EventRecord, StateReducer};
        let mut mon = party::starter_mon(data, species, level);
        let max = mon.hp.value.map_or(100, |(_, max)| max);
        mon.hp = pokebot_state::Knowledge::observed((max, max), 1);
        DefaultReducer.reduce(
            &with_balls(10),
            &[EventRecord {
                frame_id: 1,
                event: GameEvent::PartyMonDerived {
                    slot: 0,
                    mon: Box::new(mon),
                },
            }],
        )
    }

    fn battle_task() -> Option<(StoryTask, pokebot_state::GameState)> {
        let (world, data) = story_fixture()?;
        let state = catch_state(&data);
        let task = one_milestone(world, data, vec![StoryStep::Settle { frames: 100_000 }]);
        Some((task, state))
    }

    #[test]
    fn a_catch_becomes_a_party_member_when_the_battle_ends() {
        let Some((mut task, state)) = battle_task() else {
            return;
        };
        let command = Some(pokebot_state::BattleMenu::Command { column: 0, row: 0 });
        let mut all = Vec::new();
        for f in [1, 2] {
            let (_, events) = tick_events(&mut task, &wild_frame(f, command, &[]), &state);
            all.extend(events);
        }
        assert!(all.contains(&GameEvent::SpeciesSeen {
            species: "SPECIES_PIDGEY".into()
        }));
        assert!(task.battle_memory.catch.attempt.is_some(), "{all:?}");
        for f in [3, 4] {
            tick_events(
                &mut task,
                &wild_frame(f, None, &["Gotcha!", "PIDGEY was caught!"]),
                &state,
            );
        }
        // The Pokédex page: A once two frames show it.
        let mut page = Observation::bare(5, wild_frame(5, None, &[]).screen, Default::default());
        page.pokedex_page = true;
        assert_eq!(
            tick_events(&mut task, &page, &state).0,
            "wait: confirming the Pokédex page"
        );
        page.frame_id = 6;
        assert_eq!(
            tick_events(&mut task, &page, &state).0,
            "close the Pokédex page"
        );
        // "Give a nickname to the captured PIDGEY?" Yes/No → No.
        let mut ask = wild_frame(7, None, &["Give a nickname to the", "captured PIDGEY?"]);
        ask.battle.as_mut().unwrap().opponent_name = None;
        ask.menu = Some(pokebot_state::MenuObservation {
            window: pokebot_state::Region::new(190, 70, 44, 36),
            rows: 2,
            cursor_row: 0,
            cursor_y: 76,
        });
        assert_eq!(tick_events(&mut task, &ask, &state).0, "nickname: NO");
        // Back in the overworld: the catch joins the party.
        let (_, events) = tick_events(&mut task, &located(8, "Route2", 7, 3), &state);
        assert!(events.contains(&GameEvent::BattleEnded), "{events:?}");
        assert!(events.contains(&GameEvent::SpeciesCaught {
            species: "SPECIES_PIDGEY".into()
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            GameEvent::PartyMonDerived { slot: 1, mon } if mon.species.value.as_deref() == Some("SPECIES_PIDGEY")
        )), "{events:?}");
    }

    /// Live (Route 3, every trainer won): "RED got ¥120 for winning!" was
    /// advanced on its first ready frame, so the tracker read it once and
    /// never emitted `MoneyChanged`.
    #[test]
    fn a_page_is_read_on_two_frames_before_it_is_advanced() {
        let Some((mut task, state)) = battle_task() else {
            return;
        };
        let money = |f| {
            let mut o = wild_frame(f, None, &["RED got ¥120", "for winning!"]);
            o.battle.as_mut().unwrap().opponent_name = None;
            let d = o.dialogue.as_mut().unwrap();
            d.waiting_for_input = true;
            d.arrow = Some(pokebot_state::Region::new(220, 150, 8, 8));
            o
        };
        let (label, events) = tick_events(&mut task, &money(1), &state);
        assert_eq!(label, "wait: reading the page on a second frame");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GameEvent::MoneyChanged { .. })),
            "{events:?}"
        );
        let (label, events) = tick_events(&mut task, &money(2), &state);
        assert!(events.contains(&GameEvent::MoneyChanged {
            delta: 120,
            reason: "won a battle".into()
        }));
        assert_eq!(label, "advance text");
        // A page that keeps misreading is advanced after a bounded wait.
        let flicker = |f: u64| {
            let mut o = money(f);
            let d = o.dialogue.as_mut().unwrap();
            d.lines[1] = if f % 2 == 0 {
                "for winning!"
            } else {
                "for winnin"
            }
            .into();
            d.lines[0] = "RED got ¥80".into();
            o
        };
        for f in 3..3 + PAGE_READ_WAIT_FRAMES {
            assert_eq!(
                tick_events(&mut task, &flicker(f), &state).0,
                "wait: reading the page on a second frame"
            );
        }
        let f = 3 + PAGE_READ_WAIT_FRAMES;
        assert_eq!(
            tick_events(&mut task, &flicker(f), &state).0,
            "advance text"
        );
        // A reading that alternates in a middle character every frame
        // (¥80 / ¥8O) is still advanced within the bounded wait.
        let middle = |f: u64| {
            let mut o = money(f);
            o.dialogue.as_mut().unwrap().lines = [
                if f % 2 == 0 {
                    "RED got ¥80"
                } else {
                    "RED got ¥8O"
                },
                "for winning!",
            ]
            .map(String::from)
            .to_vec();
            o
        };
        let start = f + 10;
        let mut advanced = None;
        for g in start..=start + PAGE_READ_WAIT_FRAMES {
            if tick_events(&mut task, &middle(g), &state).0 == "advance text" {
                advanced = Some(g - start);
                break;
            }
        }
        assert_eq!(advanced, Some(PAGE_READ_WAIT_FRAMES));
        let f = start + PAGE_READ_WAIT_FRAMES;
        // The next page (after the executor's pending A) still waits for
        // its own second reading: the timed-out wait doesn't carry over.
        let next = |f: u64| {
            let mut o = money(f);
            o.dialogue.as_mut().unwrap().lines =
                ["RED got ¥64", "for winning!"].map(String::from).to_vec();
            o
        };
        let f = f + 20;
        let (label, events) = tick_events(&mut task, &next(f), &state);
        assert_eq!(label, "wait: reading the page on a second frame");
        assert!(!events
            .iter()
            .any(|e| matches!(e, GameEvent::MoneyChanged { .. })));
        let (label, events) = tick_events(&mut task, &next(f + 1), &state);
        assert!(events.contains(&GameEvent::MoneyChanged {
            delta: 64,
            reason: "won a battle".into()
        }));
        assert_eq!(label, "advance text");
    }

    /// Review: the early wait for a second reading returned before the
    /// battle's text readers ran, so "IVYSAUR's VINE WHIP is disabled!"
    /// (read once, then advanced) never marked the move.
    #[test]
    fn a_disable_refusal_page_marks_the_move_through_the_story() {
        let Some((mut task, state)) = battle_task() else {
            return;
        };
        let refusal = |f| {
            let mut o = wild_frame(f, None, &["IVYSAUR’s VINE WHIP", "is disabled!"]);
            let d = o.dialogue.as_mut().unwrap();
            d.waiting_for_input = true;
            d.arrow = Some(pokebot_state::Region::new(220, 150, 8, 8));
            o
        };
        assert_eq!(
            tick_events(&mut task, &refusal(1), &state).0,
            "wait: reading the page on a second frame"
        );
        assert_eq!(task.battle_memory.disabled, None);
        assert_eq!(
            tick_events(&mut task, &refusal(2), &state).0,
            "advance text"
        );
        assert_eq!(
            task.battle_memory.disabled.as_deref(),
            Some("MOVE_VINE_WHIP")
        );
    }

    /// Live: CrossMtMoon's way out after the Helix Fossil ran into the
    /// fossils' tiles (static objects), so the router fell back to a ladder
    /// into a part of MtMoon_B1F without the exit ("no path to warp").
    #[test]
    fn a_taken_fossil_no_longer_blocks_the_way() {
        use pokebot_state::{DialogueKind, DialogueObservation, Region};
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let mut task = one_milestone(
            world,
            data,
            vec![
                StoryStep::Talk {
                    map: "MtMoon_B2F".into(),
                    object: 2,
                    answers: vec![Answer::Yes],
                },
                StoryStep::Go(Destination::Warp {
                    map: "MtMoon_B1F".into(),
                    warp: 7,
                }),
            ],
        );
        let state = pokebot_state::GameState::default();
        task.talk = TalkPhase::Talking;
        let obtained = |f| {
            let mut o = located(f, "MtMoon_B2F", 14, 8);
            o.player = None;
            o.screen.value = ScreenState::Dialogue;
            o.dialogue = Some(DialogueObservation {
                kind: DialogueKind::MessageBox,
                region: Region::new(8, 119, 224, 34),
                waiting_for_input: true,
                arrow: Some(Region::new(220, 150, 8, 8)),
                stable_frames: 10,
                text_cells: vec![1; 4],
                // As read live.
                lines: vec!["Obtained the HELIX FOSSIL!".into()],
                help: false,
            });
            o
        };
        tick_events(&mut task, &obtained(1), &state);
        let (label, events) = tick_events(&mut task, &obtained(2), &state);
        assert_eq!(label, "advance text");
        assert!(
            events.iter().any(|e| matches!(e, GameEvent::ItemsChanged { item, delta: 1, .. } if item == "ITEM_HELIX_FOSSIL")),
            "{events:?}"
        );
        let mut f = 3;
        while task.step == 0 {
            tick(&mut task, &located(f, "MtMoon_B2F", 14, 8), &state);
            f += 1;
            assert!(f < 400, "the talk never ended");
        }
        assert!(task.taken.contains(&("MtMoon_B2F".to_owned(), 2)));
        // North through the fossil's tile, toward the ladder at (5, 10)
        // that leads to B1F's exit part.
        let label = tick(&mut task, &located(f, "MtMoon_B2F", 14, 8), &state);
        assert!(label.contains("Up"), "{label}");
        assert!(label.contains("(5, 10)"), "{label}");
    }

    /// Live: with a second party member (a catch), a trainer's next
    /// Pokémon brings "Will RED change POKéMON?" Yes/No, and the story
    /// failed on it as an unexpected question.
    #[test]
    fn the_change_pokemon_question_is_answered_no() {
        let Some((mut task, state)) = battle_task() else {
            return;
        };
        let mut ask = wild_frame(1, None, &["Will RED change", "POKéMON?"]);
        ask.menu = Some(pokebot_state::MenuObservation {
            window: pokebot_state::Region::new(190, 70, 44, 36),
            rows: 2,
            cursor_row: 0,
            cursor_y: 76,
        });
        assert_eq!(tick_events(&mut task, &ask, &state).0, "change Pokémon: NO");
    }

    fn miguel_task() -> Option<(StoryTask, pokebot_state::GameState)> {
        let (world, data) = story_fixture()?;
        // IVYSAUR Lv18 with its default moves: VINE WHIP is the only attack.
        let state = catch_state(&data);
        let task = one_milestone(
            world,
            data,
            vec![
                StoryStep::Battle {
                    trigger: Destination::Tile {
                        map: "MtMoon_B2F".into(),
                        x: 14,
                        y: 11,
                    },
                    loss_ok: false,
                    trainer: Some("TRAINER_SUPER_NERD_MIGUEL".into()),
                },
                StoryStep::Settle { frames: 100_000 },
            ],
        );
        Some((task, state))
    }

    /// The live lead's moves (TACKLE, SLEEP POWDER, LEECH SEED, VINE WHIP)
    /// at Lv19 and `hp`/60: ready for Miguel at full HP.
    fn live_lead(state: &mut pokebot_state::GameState, hp: u16) {
        use pokebot_state::{Knowledge, MoveSlot};
        let slot = |mv: &str, pp: u8| {
            Some(MoveSlot {
                mv: Knowledge::observed(mv.into(), 1),
                pp: Knowledge::observed((pp, pp), 1),
            })
        };
        if let Some(party) = state.party.value.as_mut() {
            party[0].moves = [
                slot("MOVE_TACKLE", 35),
                slot("MOVE_SLEEP_POWDER", 15),
                slot("MOVE_LEECH_SEED", 10),
                slot("MOVE_VINE_WHIP", 10),
            ];
            party[0].level = Knowledge::observed(19, 1);
            party[0].hp = Knowledge::observed((hp, 60), 1);
        }
    }

    /// Plays a battle that begins after `pose` and ends back there.
    fn battle_from(
        task: &mut StoryTask,
        state: &pokebot_state::GameState,
        pose: (&str, i32, i32),
        from: u64,
    ) -> u64 {
        tick(task, &located(from, pose.0, pose.1, pose.2), state);
        tick(task, &wild_frame(from + 1, None, &[]), state);
        assert!(task.in_battle);
        let mut f = from + 2;
        while f < from + 2 + u64::from(SETTLE_FRAMES) + 5 {
            tick(task, &located(f, pose.0, pose.1, pose.2), state);
            f += 1;
        }
        f
    }

    /// Live: a wild battle on 1F (the first of the way) completed the Miguel
    /// step, so the bot went for the fossil, crossed Miguel's trigger at
    /// 6/60 HP and fainted.
    #[test]
    fn a_battle_away_from_the_trigger_is_not_the_trigger_battle() {
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        live_lead(&mut state, 60);
        let f = battle_from(&mut task, &state, ("MtMoon_1F", 19, 27), 1);
        assert_eq!(task.step, 0, "a wild battle on 1F ended the Miguel step");
        assert!(!task.battle_seen);
        // The battle that begins on (14, 11) is Miguel's.
        battle_from(&mut task, &state, ("MtMoon_B2F", 14, 11), f);
        assert_eq!(task.step, 1);
    }

    /// Review: one missed pose on the step onto the trigger made Miguel's
    /// battle look like a wild one, and the step waited forever on a trigger
    /// that doesn't fire again.
    #[test]
    fn a_trainer_battle_next_to_the_trigger_is_the_triggers() {
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        live_lead(&mut state, 60);
        // Last pose one tile short of (14, 11); the battle is a trainer's.
        tick(&mut task, &located(1, "MtMoon_B2F", 14, 12), &state);
        tick(&mut task, &wild_frame(2, None, &[]), &state);
        task.battle_memory.trainer = true;
        let mut f = 3;
        while task.step == 0 && f < 200 {
            tick(&mut task, &located(f, "MtMoon_B2F", 14, 12), &state);
            f += 1;
        }
        assert_eq!(task.step, 1);
        // A wild battle there doesn't count.
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        live_lead(&mut state, 60);
        battle_from(&mut task, &state, ("MtMoon_B2F", 14, 12), 1);
        assert_eq!(task.step, 0);
    }

    #[test]
    fn standing_on_the_trigger_with_no_battle_fails_the_step() {
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        live_lead(&mut state, 60);
        let mut last = String::new();
        for f in 1..=(u64::from(TRIGGER_TIMEOUT_FRAMES) + 5) {
            last = tick(&mut task, &located(f, "MtMoon_B2F", 14, 11), &state);
            if last.starts_with("fail") {
                break;
            }
        }
        assert!(last.starts_with("fail: on the trigger"), "{last}");
    }

    /// Controller ruling: before a planned battle, heal when the lead at its
    /// current HP has P(win) < 0.9 against that trainer (live: 14/60 before
    /// Miguel, then a faint).
    #[test]
    fn a_weakened_lead_heals_before_a_planned_battle() {
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        live_lead(&mut state, 20);
        let label = tick(&mut task, &located(1, "MtMoon_B2F", 20, 20), &state);
        assert_eq!(label, "wait: healing before the battle");
        assert_eq!(task.current(), Some(&StoryStep::Heal { center: None }));
        assert!(matches!(
            task.milestones[0].steps[1],
            StoryStep::Battle { .. }
        ));
        // At full HP it walks on.
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        live_lead(&mut state, 60);
        let label = tick(&mut task, &located(1, "MtMoon_B2F", 20, 20), &state);
        assert_ne!(label, "wait: healing before the battle");
        assert!(matches!(task.current(), Some(StoryStep::Battle { .. })));
    }

    /// Review: with the Lv18 default moves VINE WHIP is the only attack, so
    /// P(win) against Miguel stays below 0.9 even healed. Healing can't help,
    /// and with a wild battle after each heal it would repeat forever: the
    /// lead goes on (the Prepare plan vouched for the battle) with one
    /// warning, also after a wild battle on the way.
    #[test]
    fn a_battle_healing_cant_make_ready_is_fought_without_healing() {
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        if let Some(party) = state.party.value.as_mut() {
            party[0].hp = pokebot_state::Knowledge::observed((20, 54), 1);
        }
        let mut warnings = 0;
        let mut labels = Vec::new();
        let mut run = |task: &mut StoryTask, o: &Observation| {
            let (label, events) = tick_events(task, o, &state);
            warnings += events
                .iter()
                .filter(
                    |e| matches!(e, GameEvent::GoalProgress { phase, .. } if phase == "Warning"),
                )
                .count();
            labels.push(label);
        };
        for f in 1..100 {
            run(&mut task, &located(f, "MtMoon_B2F", 20, 20));
        }
        // A wild battle on the way, then on toward the trigger.
        run(&mut task, &wild_frame(100, None, &[]));
        for f in 101..300 {
            run(&mut task, &located(f, "MtMoon_B2F", 20, 20));
        }
        assert!(
            !labels.iter().any(|l| l.contains("heal")),
            "{:?}",
            labels
                .iter()
                .filter(|l| l.contains("heal"))
                .collect::<Vec<_>>()
        );
        assert!(matches!(task.current(), Some(StoryStep::Battle { .. })));
        assert_eq!(warnings, 1);
    }

    /// Ends the current (Heal) step as the nurse would, then forgets the
    /// heal as a wild battle on the way back would.
    fn heal_done_then_wild_battle(task: &mut StoryTask, state: &pokebot_state::GameState) {
        assert_eq!(task.current(), Some(&StoryStep::Heal { center: None }));
        let mut events = Vec::new();
        task.advance_step(&mut TaskContext {
            observation: &located(1, "MtMoon_B2F", 20, 20),
            state,
            events: &mut events,
        });
        task.just_healed = false;
    }

    /// Review: nothing capped the heal round trips before a battle. After
    /// three in one step, a lead still below 0.9 alone fails the step.
    #[test]
    fn a_fourth_pre_battle_heal_fails_the_step() {
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        live_lead(&mut state, 20);
        for f in 1..=3 {
            let label = tick(&mut task, &located(f, "MtMoon_B2F", 20, 20), &state);
            assert_eq!(label, "wait: healing before the battle", "heal {f}");
            heal_done_then_wild_battle(&mut task, &state);
        }
        let label = tick(&mut task, &located(10, "MtMoon_B2F", 20, 20), &state);
        assert!(
            label.starts_with("fail: not ready for the battle after 3 heals in this step"),
            "{label}"
        );
        assert!(label.contains("lead-only P(win) now"), "{label}");
    }

    /// Past the cap, a lead that is ready alone (here: paralyzed, which the
    /// model doesn't weigh, but at full HP) fights, with one warning.
    #[test]
    fn past_the_heal_cap_a_ready_lead_fights() {
        use pokebot_state::{DefaultReducer, EventRecord, StateReducer};
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        live_lead(&mut state, 60);
        let state = DefaultReducer.reduce(
            &state,
            &[EventRecord {
                frame_id: 2,
                event: GameEvent::PartyObserved {
                    slot: 0,
                    species: None,
                    nickname: None,
                    level: None,
                    hp: None,
                    status: Some(Status::Paralyzed),
                    held_item: None,
                },
            }],
        );
        for f in 1..=3 {
            let label = tick(&mut task, &located(f, "MtMoon_B2F", 20, 20), &state);
            assert!(label.starts_with("wait: healing"), "heal {f}: {label}");
            heal_done_then_wild_battle(&mut task, &state);
        }
        let mut warnings = 0;
        for f in 10..20 {
            let (label, events) = tick_events(&mut task, &located(f, "MtMoon_B2F", 20, 20), &state);
            assert!(!label.contains("heal"), "{label}");
            assert!(!label.starts_with("fail"), "{label}");
            warnings += events
                .iter()
                .filter(
                    |e| matches!(e, GameEvent::GoalProgress { phase, .. } if phase == "Warning"),
                )
                .count();
        }
        assert!(matches!(task.current(), Some(StoryStep::Battle { .. })));
        assert_eq!(task.heals_this_step, 3);
        assert!(warnings >= 1, "no warning past the cap");
    }

    /// A status heal past the cap goes on without healing, with a warning.
    #[test]
    fn past_the_heal_cap_a_status_is_not_healed_again() {
        use pokebot_state::{DefaultReducer, EventRecord, StateReducer};
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let mut state = catch_state(&data);
        live_lead(&mut state, 60);
        let state = DefaultReducer.reduce(
            &state,
            &[EventRecord {
                frame_id: 2,
                event: GameEvent::PartyObserved {
                    slot: 0,
                    species: None,
                    nickname: None,
                    level: None,
                    hp: None,
                    status: Some(Status::Paralyzed),
                    held_item: None,
                },
            }],
        );
        let mut task = one_milestone(
            world,
            data,
            vec![StoryStep::Go(Destination::Tile {
                map: "MtMoon_1F".into(),
                x: 18,
                y: 30,
            })],
        );
        for f in 1..=3 {
            let label = tick(&mut task, &located(f, "MtMoon_1F", 18, 36), &state);
            assert_eq!(label, "wait: healing a status condition", "heal {f}");
            let mut events = Vec::new();
            task.advance_step(&mut TaskContext {
                observation: &located(f, "MtMoon_1F", 18, 36),
                state: &state,
                events: &mut events,
            });
            task.just_healed = false;
        }
        let (label, events) = tick_events(&mut task, &located(10, "MtMoon_1F", 18, 36), &state);
        assert_ne!(label, "wait: healing a status condition");
        assert!(matches!(task.current(), Some(StoryStep::Go(_))));
        assert!(events.iter().any(|e| matches!(
            e,
            GameEvent::GoalProgress { phase, detail, .. }
                if phase == "Warning" && detail.contains("going on without healing")
        )));
        // The warning is given once.
        let (_, events) = tick_events(&mut task, &located(11, "MtMoon_1F", 18, 36), &state);
        assert!(!events
            .iter()
            .any(|e| matches!(e, GameEvent::GoalProgress { phase, .. } if phase == "Warning")));
    }

    /// Review: the trigger timeout counted quiet frames, so after a long
    /// quiet walk the step failed the moment it arrived.
    #[test]
    fn arriving_on_the_trigger_after_a_long_quiet_walk_waits() {
        let Some((mut task, mut state)) = miguel_task() else {
            return;
        };
        live_lead(&mut state, 60);
        task.quiet_frames = 5000;
        let label = tick(&mut task, &located(10_000, "MtMoon_B2F", 14, 11), &state);
        assert_eq!(label, "wait: waiting for the battle");
        let label = tick(&mut task, &located(10_300, "MtMoon_B2F", 14, 11), &state);
        assert_eq!(label, "wait: waiting for the battle");
        // Leaving the tile restarts the wait.
        tick(&mut task, &located(10_301, "MtMoon_B2F", 14, 12), &state);
        let label = tick(&mut task, &located(10_700, "MtMoon_B2F", 14, 11), &state);
        assert_eq!(label, "wait: waiting for the battle");
        let label = tick(
            &mut task,
            &located(
                10_700 + u64::from(TRIGGER_TIMEOUT_FRAMES),
                "MtMoon_B2F",
                14,
                11,
            ),
            &state,
        );
        assert!(label.starts_with("fail: on the trigger"), "{label}");
    }

    #[test]
    fn a_paralyzed_lead_heals_before_walking_on() {
        use pokebot_state::{DefaultReducer, EventRecord, StateReducer};
        let Some((world, data)) = story_fixture() else {
            return;
        };
        let mut state = catch_state(&data);
        live_lead(&mut state, 60);
        let state = DefaultReducer.reduce(
            &state,
            &[EventRecord {
                frame_id: 2,
                event: GameEvent::PartyObserved {
                    slot: 0,
                    species: None,
                    nickname: None,
                    level: None,
                    hp: None,
                    status: Some(Status::Paralyzed),
                    held_item: None,
                },
            }],
        );
        let mut task = one_milestone(
            world,
            data,
            vec![StoryStep::Go(Destination::Tile {
                map: "MtMoon_1F".into(),
                x: 18,
                y: 30,
            })],
        );
        let label = tick(&mut task, &located(3, "MtMoon_1F", 18, 36), &state);
        assert_eq!(label, "wait: healing a status condition");
        assert_eq!(task.current(), Some(&StoryStep::Heal { center: None }));
    }

    /// The lead's status is read from battle text into the state, once:
    /// live, every longer prefix of the printing page ("IVYSAUR is paral",
    /// "IVYSAUR is paralyzed!", …) was a new page and emitted it again.
    #[test]
    fn our_paralysis_in_battle_text_becomes_a_party_status() {
        use pokebot_state::{DefaultReducer, EventRecord, StateReducer};
        let Some((mut task, mut state)) = battle_task() else {
            return;
        };
        let full = ["IVYSAUR is paralyzed!", "It may be unable to move!"];
        let mut pages: Vec<Vec<String>> = Vec::new();
        for n in [16, 18, 21] {
            pages.push(vec![full[0][..n].to_owned()]);
        }
        for n in [5, 12, 25] {
            pages.push(vec![full[0].to_owned(), full[1][..n].to_owned()]);
        }
        let mut statuses = 0;
        let mut f = 1;
        for page in &pages {
            for _ in 0..2 {
                let lines: Vec<&str> = page.iter().map(String::as_str).collect();
                let events = tick_events(&mut task, &wild_frame(f, None, &lines), &state).1;
                statuses += events
                    .iter()
                    .filter(|e| {
                        matches!(
                            e,
                            GameEvent::PartyObserved {
                                slot: 0,
                                status: Some(Status::Paralyzed),
                                ..
                            }
                        )
                    })
                    .count();
                let records: Vec<EventRecord> = events
                    .into_iter()
                    .map(|event| EventRecord { frame_id: f, event })
                    .collect();
                state = DefaultReducer.reduce(&state, &records);
                f += 1;
            }
        }
        assert_eq!(statuses, 1);
        assert_eq!(
            state.party.value.as_ref().unwrap()[0].status.value,
            Some(Status::Paralyzed)
        );
    }

    /// flash-4, Viridian Forest with a full party: after NO to the nickname
    /// the game prints "WEEDLE was transferred to Someone's PC." with the
    /// YES/NO box still on screen (fixture
    /// `captures/fixtures/emu-catch-transferred-to-pc.png`).
    #[test]
    fn the_pc_transfer_text_under_a_lingering_yes_no_box_is_advanced() {
        let Some((mut task, state)) = battle_task() else {
            return;
        };
        let mut page = wild_frame(1, None, &["WEEDLE was transferred to", "Someone’s PC."]);
        page.menu = Some(pokebot_state::MenuObservation {
            window: pokebot_state::Region::new(190, 70, 44, 36),
            rows: 2,
            cursor_row: 0,
            cursor_y: 76,
        });
        let (label, _) = tick_events(&mut task, &page, &state);
        assert!(
            !label.starts_with("fail:"),
            "the transfer text is not a question: {label}"
        );
    }

    /// flash-5, Route 22 with a full party: "It was placed in BOX…" read
    /// as "It" while printing under the lingering YES/NO box (fixture
    /// `captures/fixtures/emu-catch-placed-in-box-printing.png`).
    #[test]
    fn a_page_printing_under_the_lingering_box_is_waited_for() {
        let Some((mut task, state)) = battle_task() else {
            return;
        };
        let mut page = wild_frame(1, None, &["It"]);
        page.menu = Some(pokebot_state::MenuObservation {
            window: pokebot_state::Region::new(190, 70, 44, 36),
            rows: 2,
            cursor_row: 0,
            cursor_y: 76,
        });
        page.dialogue.as_mut().unwrap().stable_frames = 1;
        let (label, _) = tick_events(&mut task, &page, &state);
        assert!(label.starts_with("wait:"), "{label}");
    }

    #[test]
    fn other_questions_in_battle_still_fail() {
        let Some((mut task, state)) = battle_task() else {
            return;
        };
        let mut ask = wild_frame(1, None, &["Will you trade", "your POKéMON?"]);
        ask.menu = Some(pokebot_state::MenuObservation {
            window: pokebot_state::Region::new(190, 70, 44, 36),
            rows: 2,
            cursor_row: 0,
            cursor_y: 76,
        });
        assert!(tick_events(&mut task, &ask, &state)
            .0
            .starts_with("fail: unexpected question in battle"));
    }

    #[test]
    fn the_battle_bag_goes_to_the_throw_not_the_menu_closer() {
        let Some((mut task, state)) = battle_task() else {
            return;
        };
        let command = pokebot_state::BattleMenu::Command { column: 1, row: 0 };
        for f in [1, 2] {
            tick_events(&mut task, &wild_frame(f, Some(command), &[]), &state);
        }
        // Weakened foe and sleep skipped: straight to BAG.
        let attempt = task.battle_memory.catch.attempt.as_mut().unwrap();
        attempt.opened = true;
        let mut low = wild_frame(3, Some(command), &[]);
        low.battle.as_mut().unwrap().opponent_hp = Some(200);
        tick_events(&mut task, &low, &state);
        low.frame_id = 4;
        assert_eq!(tick_events(&mut task, &low, &state).0, "choose BAG");
        // The bag fading in shows a lone ▶ that reads as a 1-row menu.
        let mut flicker = Observation::bare(5, low.screen.clone(), Default::default());
        flicker.screen.value = ScreenState::Menu;
        flicker.menu = Some(pokebot_state::MenuObservation {
            window: pokebot_state::Region::new(88, 12, 100, 16),
            rows: 1,
            cursor_row: 0,
            cursor_y: 12,
        });
        assert_eq!(
            tick_events(&mut task, &flicker, &state).0,
            "wait: waiting for the battle bag"
        );
        let mut bag = Observation::bare(6, low.screen.clone(), Default::default());
        bag.screen.value = ScreenState::Bag;
        bag.bag = Some(pokebot_state::BagObservation {
            pocket: "POKé BALLS".into(),
            rows: vec![("POKé BALL".into(), Some(10)), ("CANCEL".into(), None)],
            cursor: Some(0),
            prompt: None,
        });
        tick_events(&mut task, &bag, &state);
        bag.frame_id = 7;
        let (label, events) = tick_events(&mut task, &bag, &state);
        assert_eq!(label, "throw POKE_BALL: select it");
        assert!(events.contains(&GameEvent::PocketObserved {
            pocket: Pocket::PokeBalls,
            items: vec![("ITEM_POKE_BALL".into(), 10)],
        }));
    }
}
