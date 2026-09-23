//! Story progression as data: milestones made of small declarative steps
//! (walk somewhere, take a door, talk to someone, answer YES/NO, train, heal),
//! executed closed-loop by one task. Battles and dialogue can interrupt any
//! step. If one of our Pokémon faints the task fails, so the caller can
//! reload the last save and retry.

use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_planner::{plan_preparation, Area, PartyMember, PlanStep, Request};
use pokebot_state::{GameEvent, Observation, ScreenState};
use pokebot_world::behavior::TALL_GRASS;
use pokebot_world::World;
use serde::Serialize;

use crate::battle::{self, BattleMemory, BattlePolicy};
use crate::learn::MoveLearning;
use crate::nav::{Destination, NavStatus, Navigator};
use crate::new_game::{advance_or_wait, select};
use crate::party::Party;
use crate::{Action, Decision, Expectation, Outcome, Task, TaskContext};

/// Quiet frames (no dialogue) after a conversation or cutscene before the
/// bot starts walking again; scripts often pause between lines.
const SETTLE_FRAMES: u32 = 90;
/// A battle that starts this soon after dialogue is a trainer battle (the
/// trainer talks first); wild battles start without any.
const TRAINER_DIALOGUE_WINDOW: u64 = 600;
/// Go heal when the lead's HP drops below this share (per mille) while training.
const HEAL_BELOW: u32 = 500;
/// Training heals when wild battles have fewer attack PP than this to spend
/// (a few battles' worth).
const HEAL_BELOW_PP: u32 = 6;

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
    /// back in the overworld.
    Battle { trigger: Destination },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TalkPhase {
    Approach,
    Press,
    Talking,
}

pub struct StoryTask {
    world: Arc<World>,
    data: Option<Arc<GameData>>,
    party: Party,
    milestones: Vec<Milestone>,
    milestone: usize,
    step: usize,
    nav: Option<Navigator>,
    talk: TalkPhase,
    saw_dialogue: bool,
    answers_used: usize,
    quiet_frames: u32,
    announced: bool,
    battle_seen: bool,
    policy: BattlePolicy,
    battle_memory: BattleMemory,
    /// Inside a battle right now (ended once the overworld is seen again).
    in_battle: bool,
    last_dialogue_frame: Option<u64>,
    /// Training: the two grass tiles we pace between, and which is next.
    pacing: Option<(Tile, Tile, bool)>,
    /// New moves and evolution, read from the screen.
    learning: MoveLearning,
    /// Trainers the last plan prepared for (values moves when learning).
    upcoming: Vec<String>,
}

impl StoryTask {
    pub fn new(world: Arc<World>, milestones: Vec<Milestone>) -> Self {
        Self {
            world,
            data: None,
            party: Party::default(),
            milestones,
            milestone: 0,
            step: 0,
            nav: None,
            talk: TalkPhase::Approach,
            saw_dialogue: false,
            answers_used: 0,
            quiet_frames: SETTLE_FRAMES,
            announced: false,
            battle_seen: false,
            policy: BattlePolicy::default(),
            battle_memory: BattleMemory::default(),
            in_battle: false,
            last_dialogue_frame: None,
            pacing: None,
            learning: MoveLearning::default(),
            upcoming: Vec::new(),
        }
    }

    /// Game data and party knowledge enable planning, training and move
    /// choice.
    pub fn with_party(mut self, data: Arc<GameData>, party: Party) -> Self {
        self.data = Some(data);
        self.party = party;
        self
    }

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
        self.step += 1;
        self.nav = None;
        self.talk = TalkPhase::Approach;
        self.saw_dialogue = false;
        self.answers_used = 0;
        self.battle_seen = false;
        self.pacing = None;
        if self.step >= self.milestones[self.milestone].steps.len() {
            ctx.events.push(GameEvent::GoalProgress {
                goal: "Story".into(),
                phase: self.milestones[self.milestone].name.clone(),
                detail: "completed".into(),
            });
            self.milestone += 1;
            self.step = 0;
            self.announced = false;
        }
    }

    /// Replaces the current step with `steps` (e.g. a plan, or heal + resume).
    fn splice(&mut self, steps: Vec<StoryStep>, keep_current: bool) {
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
        self.pacing = None;
    }

    fn navigate(&mut self, dest: &Destination, observation: &Observation) -> NavStatusOrDecision {
        if self.quiet_frames < SETTLE_FRAMES {
            return NavStatusOrDecision::Decision(Decision::Wait(
                "letting the scene settle".into(),
            ));
        }
        let world = Arc::clone(&self.world);
        let nav = self
            .nav
            .get_or_insert_with(|| Navigator::new(world, dest.clone()));
        if nav.destination != *dest {
            *nav = Navigator::new(Arc::clone(&self.world), dest.clone());
        }
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

    /// Two neighbouring tall-grass tiles on `map` to pace between, closest to
    /// `near` (deterministic: row-major order breaks ties).
    fn grass_pair(&self, map: &str, near: (i32, i32)) -> Option<((i32, i32), (i32, i32))> {
        let m = self.world.map(map)?;
        let grass = |x: i32, y: i32| {
            m.tile(x, y)
                .is_some_and(|t| t.behavior == TALL_GRASS && t.collision == 0)
        };
        let mut best: Option<(i32, Tile, Tile)> = None;
        for y in 0..m.height {
            for x in 0..m.width {
                if !grass(x, y) {
                    continue;
                }
                for (dx, dy) in [(1, 0), (0, 1)] {
                    if grass(x + dx, y + dy) {
                        let d = (x - near.0).abs() + (y - near.1).abs();
                        if best.as_ref().is_none_or(|(bd, _, _)| d < *bd) {
                            best = Some((d, (x, y), (x + dx, y + dy)));
                        }
                    }
                }
            }
        }
        best.map(|(_, a, b)| (a, b))
    }

    fn plan(
        &self,
        targets: &[String],
        areas: &[String],
        confidence: f64,
    ) -> Result<Vec<StoryStep>, String> {
        let data = self.data.as_ref().ok_or("planning needs game data")?;
        let party: Vec<PartyMember> = self
            .party
            .members
            .iter()
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
        let mut steps = Vec::new();
        for step in &plan.steps {
            match step {
                PlanStep::Train {
                    species, to, map, ..
                } if species == lead => steps.push(StoryStep::Train {
                    map: map.clone(),
                    level: *to,
                }),
                other => return Err(format!("plan step not supported yet: {other:?}")),
            }
        }
        Ok(steps)
    }
}

enum NavStatusOrDecision {
    Nav(NavStatus),
    Decision(Decision),
}

impl Task for StoryTask {
    fn name(&self) -> &str {
        "Story"
    }

    fn next(&mut self, ctx: &mut TaskContext<'_>) -> Decision {
        let o = ctx.observation;
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
            if let Some(d) = o
                .dialogue
                .as_ref()
                .filter(|d| d.ready_for_a() || o.menu.is_some())
            {
                for detail in
                    self.learning
                        .observe_page(&d.lines, &data, &mut self.party, &self.upcoming)
                {
                    ctx.events.push(GameEvent::GoalProgress {
                        goal: "Story".into(),
                        phase: "Party".into(),
                        detail,
                    });
                }
            }
            if let Some(list) = &o.move_list {
                self.quiet_frames = 0;
                return self
                    .learning
                    .on_move_list(list, &data, &mut self.party, &self.upcoming);
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
            if !self.in_battle {
                self.in_battle = true;
                self.battle_seen = true;
                let trainer = self
                    .last_dialogue_frame
                    .is_some_and(|f| o.frame_id.saturating_sub(f) < TRAINER_DIALOGUE_WINDOW);
                self.battle_memory = BattleMemory {
                    trainer,
                    ..BattleMemory::default()
                };
                ctx.events.push(GameEvent::BattleStarted);
            }
            if let Some(data) = &self.data {
                let learned = self.party.observe_battle(data, battle);
                for mv in learned {
                    ctx.events.push(GameEvent::GoalProgress {
                        goal: "Story".into(),
                        phase: "Party".into(),
                        detail: format!(
                            "{} knows {} now",
                            self.party
                                .lead()
                                .map_or(String::new(), |m| m.display_name()),
                            mv.trim_start_matches("MOVE_")
                        ),
                    });
                }
            }
            if battle.player_hp_numbers.is_some_and(|(hp, _)| hp == 0) {
                return Decision::Fail(format!(
                    "our Pokémon fainted ({})",
                    battle.player_name.clone().unwrap_or_default()
                ));
            }
            self.quiet_frames = 0;
            if let Some(data) = self.data.clone() {
                if let Some(decision) =
                    battle::decide(o, &self.policy, &mut self.battle_memory, &self.party, &data)
                {
                    return decision;
                }
            }
            if let (Some(d), Some(_)) = (&o.dialogue, &o.menu) {
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
        }
        // Dialogue and menus interrupt whatever the step is doing.
        if o.dialogue.is_some() || (o.menu.is_some() && o.screen.value == ScreenState::Dialogue) {
            self.quiet_frames = 0;
            self.saw_dialogue = true;
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
                Ok(steps) => {
                    self.upcoming = targets.clone();
                    ctx.events.push(GameEvent::GoalProgress {
                        goal: "Story".into(),
                        phase: "Plan".into(),
                        detail: if steps.is_empty() {
                            "already ready".into()
                        } else {
                            format!("{steps:?}")
                        },
                    });
                    self.splice(steps, false);
                    Decision::Wait("planned".into())
                }
                Err(e) => Decision::Fail(format!("planning failed: {e}")),
            },
            StoryStep::Battle { trigger } => {
                if self.battle_seen {
                    if !self.in_battle && self.quiet_frames >= SETTLE_FRAMES {
                        self.advance_step(ctx);
                    }
                    return Decision::Wait("battle over; settling".into());
                }
                match self.navigate(&trigger, o) {
                    NavStatusOrDecision::Decision(d) => d,
                    NavStatusOrDecision::Nav(NavStatus::Arrived) => {
                        Decision::Wait("waiting for the battle".into())
                    }
                    NavStatusOrDecision::Nav(status) => nav_decision(status),
                }
            }
            StoryStep::Settle { frames } => {
                if self.quiet_frames >= frames {
                    self.advance_step(ctx);
                }
                Decision::Wait("letting the story play out".into())
            }
        }
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, _ctx: &mut TaskContext<'_>) {
        if action.label == "choose RUN" && outcome == Outcome::Confirmed {
            self.battle_memory.run_attempts += 1;
        }
        if action.label.starts_with("choose move") && outcome == Outcome::Confirmed {
            if let (Some(mv), Some(lead)) = (
                self.battle_memory.last_move.clone(),
                self.party.members.first_mut(),
            ) {
                *lead.pp_used.entry(mv).or_insert(0) += 1;
            }
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
                        self.party.heal_all();
                    }
                    self.advance_step(ctx);
                }
                Decision::Wait("conversation ending".into())
            }
        }
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
        let out_of_pp = self.data.as_ref().is_some_and(|data| {
            lead.wild_attack_pp(data, self.policy.wild_pp_reserve) < HEAL_BELOW_PP
        });
        if out_of_pp
            || lead
                .hp
                .is_some_and(|(hp, max)| u32::from(hp) * 1000 < u32::from(max) * HEAL_BELOW)
        {
            self.splice(vec![StoryStep::Heal { center: None }], true);
            return Decision::Wait("going to heal".into());
        }
        let Some(pose) = o.player.as_ref().map(|p| p.pose.clone()) else {
            return Decision::Wait("locating".into());
        };
        if pose.map != map {
            let dest = self.world.map(map).and_then(|m| {
                let entry = (m.width / 2, m.height / 2);
                self.grass_pair(map, entry)
            });
            let Some((a, _)) = dest else {
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
        if self.pacing.is_none() {
            let Some((a, b)) = self.grass_pair(map, (pose.x, pose.y)) else {
                return Decision::Fail(format!("no tall grass on {map}"));
            };
            self.pacing = Some((a, b, false));
        }
        let (a, b, toward_b) = self.pacing.expect("set above");
        let target = if toward_b { b } else { a };
        if (pose.x, pose.y) == target {
            self.pacing = Some((a, b, !toward_b));
            self.nav = None;
            return Decision::Wait("turning around in the grass".into());
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
            NavStatusOrDecision::Nav(NavStatus::Arrived) => Decision::Wait("pacing".into()),
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
                StoryStep::Battle { trigger: Destination::Tile { map: LAB.into(), x: 6, y: 8 } },
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

/// Every milestone implemented so far, in order.
pub fn all_milestones(starter: Starter) -> Vec<Milestone> {
    let mut list = opening(starter);
    list.extend(to_brock());
    list.extend(to_mt_moon());
    list
}
