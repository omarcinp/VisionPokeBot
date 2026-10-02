//! `Battle`: play the battle on screen to its end. Wild: the catch policy
//! decides catch, fight or flee; trainer: fight. New moves, evolution, the
//! battle bag during a throw and the Pokédex page after a catch are all
//! handled here, as in `StoryTask` (whose battle branch this copies; to be
//! removed there once the story runs on tools).

use std::collections::BTreeSet;
use std::sync::Arc;

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_state::{BattleMenu, GameEvent, Observation, Status};

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
/// Frames our HUD must read 0 HP before the Pokémon counts as fainted.
const ZERO_HP_FRAMES: u64 = 30;

/// Action windows a SHIFT opens before it is given up.
const MAX_SHIFT_OPENS: u8 = 6;

pub struct BattleStep {
    data: Arc<GameData>,
    plan: BattlePlan,
    party: Party,
    policy: BattlePolicy,
    pub memory: BattleMemory,
    learning: MoveLearning,
    tracker: TextTracker,
    lead_status: Option<Status>,
    /// The frame our HUD first read 0 HP (in a row).
    zero_hp_since: Option<u64>,
    /// Switch-training: the party slot to SHIFT to at the first command
    /// menu of a wild battle (the lead, being trained, only starts it and
    /// shares the experience); done once that member is out.
    shift_to: Option<u8>,
    shifted: bool,
    /// The SHIFT target's row, once read by name on the party list (the
    /// action window hides the names below it).
    shift_row: Option<u8>,
    /// Action windows opened for the SHIFT (bounded: [`MAX_SHIFT_OPENS`]).
    shift_opens: u8,
    /// Members the game refused to send out ("ZUBAT has no energy left to
    /// battle!"): never chosen again this battle.
    no_energy: BTreeSet<String>,
    /// Our battler in this party slot fainted and another can fight: the
    /// next one is sent out ("Use next POKéMON?" YES, SEND OUT). The game
    /// restarts only when all have fainted (the user's rule).
    fainted: Option<u8>,
    /// The HUD last read (the foe a replacement is chosen against).
    last_hud: Option<pokebot_state::BattleObservation>,
    /// The trainer's next Pokémon, as announced ("… is about to use X.").
    next_foe: Option<String>,
    /// The party slot of our Pokémon in battle, from its HUD name (the
    /// lead until another is sent out).
    active: Option<u8>,
    /// A trainer's battle (dialogue just before it).
    trainer: bool,
    started: bool,
    in_battle: bool,
    battle_text_frame: Option<u64>,
    unread_since: Option<u64>,
    /// Trainers the moves are valued against when learning.
    upcoming: Vec<String>,
    /// The script fighting this battle goes on when it is lost (the
    /// early rival battles: `trainerbattle_earlyrival`, whose trainer has a
    /// victory line): a fainted lead is part of the story, not a failure.
    loss_ok: bool,
    /// A species hunted for: the catch policy takes it whenever our lead
    /// is safe ([`crate::catch::plan_catch_wanted`]); when the lead isn't
    /// (the catch declined or given up for its risk), RUN rather than
    /// faint it.
    spare: Option<String>,
    /// What later steps of the plan want caught: caught on the side, in a
    /// training battle too ([`crate::catch::plan_for_hunt`]).
    side: crate::catch::SideCatch,
}

impl BattleStep {
    /// `trainer`: the battle follows dialogue (a trainer's).
    pub fn new(data: Arc<GameData>, plan: BattlePlan, trainer: bool) -> Self {
        Self {
            data,
            plan,
            party: Party::default(),
            policy: BattlePolicy {
                lose: plan == BattlePlan::Lose,
                ..BattlePolicy::default()
            },
            memory: BattleMemory::default(),
            learning: MoveLearning::default(),
            tracker: TextTracker::default(),
            lead_status: None,
            zero_hp_since: None,
            shift_to: None,
            shifted: false,
            shift_row: None,
            shift_opens: 0,
            no_energy: BTreeSet::new(),
            fainted: None,
            last_hud: None,
            next_foe: None,
            active: None,
            trainer,
            started: false,
            in_battle: false,
            battle_text_frame: None,
            unread_since: None,
            upcoming: Vec::new(),
            loss_ok: plan == BattlePlan::Lose,
            spare: None,
            side: crate::catch::SideCatch::default(),
        }
    }

    /// Catch what later steps of the plan want when met: a training
    /// battle identifies the foe for it, and fights all others.
    pub fn catching_on_the_side(mut self, side: crate::catch::SideCatch) -> Self {
        self.side = side;
        self
    }

    /// RUN from `species` whenever it isn't being caught ([`Self::spare`]).
    pub fn sparing(mut self, species: &str) -> Self {
        self.spare = Some(species.to_owned());
        self
    }

    /// Switch-training: at the first command menu of a wild battle, SHIFT
    /// to party slot `slot` (the lead started the battle, and shares the
    /// experience without fighting).
    pub fn shifting_to(mut self, slot: u8) -> Self {
        self.shift_to = Some(slot).filter(|s| *s != 0);
        self
    }

    /// The member to send out after ours fainted: the switch-training
    /// carrier when it can fight, else the one least at risk against the
    /// foe last on the HUD, else the highest level; `None` when no other
    /// can fight (a white-out follows). Never one of `in_battle` (a double
    /// battle's other member out: "already in battle").
    fn replacement(&self, in_battle: &[u8]) -> Option<u8> {
        let out = self.fainted.or(self.active).unwrap_or(0);
        let able: Vec<&party::Member> = self
            .party
            .members
            .iter()
            .filter(|m| {
                m.slot != out && !in_battle.contains(&m.slot) && m.hp.is_some_and(|(hp, _)| hp > 0)
            })
            .collect();
        if let Some(carrier) = self
            .shift_to
            .filter(|slot| able.iter().any(|m| m.slot == *slot))
        {
            return Some(carrier);
        }
        let foe = self.last_hud.as_ref().and_then(|b| {
            Some(catch::Foe {
                species: self
                    .data
                    .species_named(b.opponent_name.as_deref()?)?
                    .to_owned(),
                level: b.opponent_level?,
                hp_per_mille: b.opponent_hp.unwrap_or(1000),
                status: catch::FoeStatus::None,
                shiny: false,
                caught: None,
            })
        });
        let risk = |m: &party::Member| {
            foe.as_ref().map_or(0.0, |foe| {
                let hp = m.hp.unwrap_or_default();
                catch::risk(&self.data, &catch::Lead { member: m, hp }, foe, 3)
            })
        };
        able.into_iter()
            .min_by(|a, b| {
                risk(a)
                    .total_cmp(&risk(b))
                    .then_with(|| b.level.cmp(&a.level))
            })
            .map(|m| m.slot)
    }

    /// The party member in `slot`'s name as the party menu prints it.
    fn member_name(&self, state: &pokebot_state::GameState, slot: u8) -> Option<String> {
        state
            .party
            .value
            .as_ref()
            .and_then(|p| p.get(usize::from(slot)))
            .and_then(|m| m.nickname.value.clone())
            .or_else(|| {
                self.party
                    .members
                    .iter()
                    .find(|m| m.slot == slot)
                    .map(|m| m.display_name())
            })
    }

    /// The member to send against the trainer's announced next Pokémon
    /// (see [`battle::member_against`]), at the level of the one before.
    fn member_for_next(&self, b: &pokebot_state::BattleObservation) -> Option<(u8, f64, f64)> {
        let species = self.next_foe.clone()?;
        let level = b
            .opponent_level
            .or(self.last_hud.as_ref().and_then(|h| h.opponent_level))?;
        let foe = catch::Foe {
            species,
            level,
            hp_per_mille: 1000,
            status: catch::FoeStatus::None,
            shiny: false,
            caught: None,
        };
        battle::member_against(&self.data, &self.party, self.active.unwrap_or(0), &foe)
    }

    /// Our battler `name` fainted: the next is sent out, or, the last
    /// one, it is the white-out (the restart is for a white-out only).
    fn faint_seen(&mut self, name: &str, events: &mut Vec<GameEvent>) -> Option<Decision> {
        self.faint_seen_at(name, None, events)
    }

    /// [`Self::faint_seen`] with the slot the party screen showed it in,
    /// for when its name wasn't read.
    fn faint_seen_at(
        &mut self,
        name: &str,
        row: Option<u8>,
        events: &mut Vec<GameEvent>,
    ) -> Option<Decision> {
        if self.fainted.is_some() {
            return None;
        }
        let slot = self
            .party
            .members
            .iter()
            .find(|m| !name.is_empty() && m.display_name() == name)
            .map(|m| m.slot)
            .or(row)
            .or(self.active)
            .unwrap_or(0);
        self.fainted = Some(slot);
        if self.replacement(&[]).is_none() {
            return Some(Decision::Fail(format!(
                "whited out: our last Pokémon fainted ({name})"
            )));
        }
        events.push(super::progress(
            "Battle",
            format!("{name} fainted: sending out the next one"),
        ));
        None
    }

    /// "Use next POKéMON?": the one out fainted (its HUD may have hidden
    /// the 0 HP before the faint counted).
    fn faint_asked(&mut self, events: &mut Vec<GameEvent>) {
        let name = self
            .last_hud
            .as_ref()
            .and_then(|b| b.player_name.clone())
            .unwrap_or_default();
        // A question only asked while another can fight: no white-out.
        let _ = self.faint_seen(&name, events);
    }

    /// A SHIFT the game refuses (a trap: ARENA TRAP, MEAN LOOK, WRAP…) is
    /// given up: the one out fights on (asking again only loops).
    fn give_up_a_refused_shift(&mut self, events: &mut Vec<GameEvent>) {
        if self.shift_to.is_some() && !self.shifted && !self.memory.limits.can_switch() {
            self.shifted = true;
            events.push(super::progress(
                "Battle",
                format!(
                    "can't SHIFT ({}): the one out fights on",
                    self.memory.limits.why().unwrap_or_default()
                ),
            ));
        }
    }

    /// The in-battle party screen, choosing the member to SHIFT to.
    fn shift_in_party_menu(
        &mut self,
        party: &pokebot_state::PartyMenuObservation,
        slot: u8,
        name: Option<&str>,
    ) -> Decision {
        // In battle the menu lists the party in battle order (the one out
        // first, then as switches left it; `UpdatePartyToBattleOrder`):
        // the member's row is where its name is, its slot only when no
        // name was read.
        // The row read by name on the list is kept: the action window
        // covers the lower panels' names, and falling back to the slot
        // there opened and closed the windows for two hours (fleet worker
        // 3: WARTORTLE's row, "close another member's actions").
        // Of two members of one name, the one that can still fight (fleet
        // continue-6, Victory Road: two DUGTRIO, the first fainted; its
        // row was opened six times, no SHIFT, then B on a send-out that
        // can't be backed out of, 40 times).
        let by_name = name
            .and_then(|n| {
                let named =
                    |r: &&pokebot_state::PartyRowObservation| r.nickname.as_deref() == Some(n);
                let able = |r: &pokebot_state::PartyRowObservation| {
                    r.status != Some(pokebot_state::Status::Fainted)
                        && r.hp.is_none_or(|(hp, _)| hp > 0)
                };
                party
                    .members
                    .iter()
                    .position(|r| named(&r) && able(r))
                    .or_else(|| party.members.iter().position(|r| named(&r)))
            })
            .map(|row| row as u8);
        if !party.actions {
            if let Some(row) = by_name {
                self.shift_row = Some(row);
            }
        }
        let slot = self.shift_row.or(by_name).unwrap_or(slot);
        let press = |label: String, button: Button, expect: Expectation| {
            Decision::Act(Action::new(
                label,
                vec![ControllerCommand::Press(button)],
                expect,
                60,
            ))
        };
        // The member isn't there (fleet worker 3: a WEEDLE the belief
        // still held, the battle's party showing BULBASAUR alone, and the
        // wait for a ▶ on a missing row, B, POKéMON, for 110k frames):
        // fight on as is.
        if party.count > 0 && slot >= party.count && !party.actions {
            self.shifted = true;
            return press(
                format!("shift: no slot {slot} in a party of {}, back", party.count),
                Button::B,
                Expectation::ScreenIsNot(pokebot_state::ScreenState::PartyMenu),
            );
        }
        if party.actions {
            if party.selected != Some(slot) {
                return press(
                    "shift: close another member's actions".into(),
                    Button::B,
                    Expectation::PartyList,
                );
            }
            let Some(row) = party
                .options
                .iter()
                .position(|l| crate::bag::fits("SHIFT", l) || crate::bag::fits("SEND OUT", l))
            else {
                // No SHIFT (the member can't come out): fight on as is.
                self.shifted = true;
                return press(
                    "shift: no SHIFT, back".into(),
                    Button::B,
                    Expectation::PartyList,
                );
            };
            let Some(at) = party.option_cursor else {
                return Decision::Wait("reading the action window's ▶".into());
            };
            let row = row as u8;
            if at == row {
                return press("choose SHIFT".into(), Button::A, Expectation::InputsDone);
            }
            let (button, next) = if at < row {
                (Button::Down, at + 1)
            } else {
                (Button::Up, at - 1)
            };
            return press(
                format!("shift: {button:?} toward SHIFT"),
                button,
                Expectation::PartyOptionAt(next),
            );
        }
        let Some(at) = party.selected else {
            return Decision::Wait("reading the selected member".into());
        };
        if at == slot {
            self.shift_opens += 1;
            if self.shift_opens > MAX_SHIFT_OPENS {
                // Opened again and again without a SHIFT: fight on as is.
                self.shifted = true;
                return press(
                    format!("shift: gave up after {MAX_SHIFT_OPENS} tries, back"),
                    Button::B,
                    Expectation::ScreenIsNot(pokebot_state::ScreenState::PartyMenu),
                );
            }
            return press(
                "shift: open the member's actions".into(),
                Button::A,
                Expectation::PartyActions,
            );
        }
        let (button, next) = if at < slot {
            (Button::Down, at + 1)
        } else {
            (Button::Up, at - 1)
        };
        press(
            format!("shift: {button:?} toward slot {slot}"),
            button,
            Expectation::PartySelected(next),
        )
    }

    /// Losing doesn't end the story ([`loss_allowed`]).
    pub fn loss_ok(mut self, loss_ok: bool) -> Self {
        self.loss_ok = loss_ok || self.policy.lose;
        self
    }

    /// Whether RUN was chosen in this battle (an escape, or its attempt).
    pub fn ran(&self) -> bool {
        self.memory.run_attempts > 0
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
        self.memory.catch.wanted = self.spare.clone();
        self.memory.catch.side = self.side.clone();
        match self.plan {
            BattlePlan::Auto => {}
            // Training with species wanted later: identified, and only
            // those caught.
            BattlePlan::Fight if !self.side.is_empty() => self.memory.catch.only_wanted = true,
            // No identification, no catch: fight (or flee) on the HUD.
            BattlePlan::Fight => self.memory.catch.decided = true,
            BattlePlan::Flee => {
                self.memory.catch.decided = true;
                self.memory.catch.flee = true;
            }
            BattlePlan::Lose => self.memory.catch.decided = true,
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
                self.memory.lead_status = Some(status);
                events.push(GameEvent::PartyObserved {
                    slot: self.active.unwrap_or(0),
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
        let o = ctx.observation;
        // The Pokémon out is the policy's lead: after a faint it is
        // another member (fleet worker 2: BUBBLE, the fainted SQUIRTLE's
        // move, chosen for RATTATA, whose menu has two moves).
        let party = Party::from_state(ctx.state);
        // Two members of one species (two WEEDLE) make the species name
        // ambiguous: the nickname tells them apart, as the sensor reads
        // them (fleet worker 2 at BROCK: the WEEDLE sent out for a fainted
        // CHARMANDER went unresolved, and CHARMANDER's METAL CLAW, move 4,
        // was aimed at on WEEDLE's two-move menu until "no progress").
        if let Some(slot) = o
            .battle
            .as_ref()
            .and_then(|b| b.player_name.as_deref())
            .filter(|n| !n.is_empty())
            .and_then(|n| {
                party.battler_named(&self.data, n).or_else(|| {
                    pokebot_sense::names::member(ctx.state, &self.data, n).map(|m| m.slot)
                })
            })
        {
            self.active = Some(slot);
        }
        // The replacement is out: a fresh battler.
        if self
            .fainted
            .is_some_and(|f| self.active.is_some_and(|a| a != f))
        {
            self.fainted = None;
            self.zero_hp_since = None;
            self.memory.our_stages = [0; 6];
            self.memory.our_accuracy = 0;
        }
        self.party = match self.active {
            Some(slot) => party.with_first(slot),
            None => party,
        };
        let active = usize::from(self.active.unwrap_or(0));
        self.lead_status = ctx
            .state
            .party
            .value
            .as_ref()
            .and_then(|p| p.get(active))
            .and_then(|m| m.status.value);
        self.memory.lead_status = self.lead_status;
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
            ctx.events
                .extend(tracked.into_iter().filter(crate::track::tool_emits));
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
            if self.plan == BattlePlan::Auto || self.memory.catch.only_wanted {
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
            if let Some(species) = o
                .dialogue
                .as_ref()
                .and_then(|d| battle::next_foe(&data, &d.lines.join(" ")))
            {
                self.next_foe = Some(species);
            }
            if let Some(species) = spares(self.spare.as_deref(), &self.memory.catch) {
                self.memory.catch.flee = true;
                ctx.events.push(super::progress(
                    "Catch",
                    format!("not catching {species} now: running rather than fainting it"),
                ));
            }
            // 0 HP read on one frame may be the drain's digits misread
            // (fleet worker 4: a full-HP MANKEY read 0/16 as a Lv3
            // RATTATA's TACKLE landed, and the cycle ended); a faint shows
            // 0 until the fainting page.
            let zero = b.player_hp_numbers.is_some_and(|(hp, _)| hp == 0);
            let since = match (zero, self.zero_hp_since) {
                (false, _) => None,
                (true, None) => Some(o.frame_id),
                (true, s) => s,
            };
            self.zero_hp_since = since;
            if since.is_some_and(|s| o.frame_id.saturating_sub(s) >= ZERO_HP_FRAMES)
                && !self.loss_ok
                && self.fainted.is_none()
            {
                let name = b.player_name.clone().unwrap_or_default();
                if let Some(white_out) = self.faint_seen(&name, ctx.events) {
                    return white_out;
                }
            }
            self.last_hud = Some(b.clone());
            // A trainer's battle can't be run from: a member at risk
            // against the foe out makes way for a safer one (once a battle:
            // the battle's party menu is reordered after a switch).
            if self.memory.trainer
                && !self.policy.lose
                && self.shift_to.is_none()
                && !self.shifted
                && self.memory.limits.can_switch()
            {
                if let Some(BattleMenu::Command { .. }) = b.menu {
                    if let Some(slot) =
                        battle::defensive_switch(&data, &self.party, b, self.memory.our_stages)
                    {
                        ctx.events.push(super::progress(
                            "Battle",
                            format!("switching to slot {slot}: the one out is at risk"),
                        ));
                        self.shift_to = Some(slot);
                    }
                }
            }
            self.give_up_a_refused_shift(ctx.events);
            // A SHIFT to a member that fainted since it was chosen: the
            // game refuses it ("There's no will to fight!"); the one out
            // fights on.
            if let Some(slot) = self.shift_to.filter(|_| !self.shifted).filter(|s| {
                self.party
                    .members
                    .iter()
                    .any(|m| m.slot == *s && m.fainted())
            }) {
                self.shifted = true;
                ctx.events.push(super::progress(
                    "Battle",
                    format!("not switching to slot {slot}: it fainted"),
                ));
            }
            // Switch-training: the member being carried started the
            // battle; the carrier comes out at the first command menu.
            // The trainee fights alone the wild ones it now beats (fleet
            // worker 2: a Lv10 MANKEY, switch-trained since Lv3, handed
            // every Lv2–5 RATTATA to CHARMANDER and took half the
            // experience): the carrier comes out only when the trainee is
            // at risk against this foe, a faint costing no more than a
            // SEND OUT (the carrier is there).
            if let (Some(carrier), false, Some(BattleMenu::Command { .. })) = (
                self.shift_to.filter(|_| !self.shifted),
                self.memory.trainer,
                b.menu,
            ) {
                if let Some(risk) = battle::lead_risk(&data, &self.party, b, self.memory.our_stages)
                    .filter(|r| *r <= catch::risk_limit(true))
                    .filter(|_| battle::alone_pays(&data, &self.party, b, carrier))
                {
                    self.shifted = true;
                    ctx.events.push(super::progress(
                        "Battle",
                        format!(
                            "{} takes {} Lv{} alone (faint risk {:.1}%)",
                            b.player_name.clone().unwrap_or_default(),
                            b.opponent_name.clone().unwrap_or_default(),
                            b.opponent_level.unwrap_or_default(),
                            risk * 100.0
                        ),
                    ));
                }
            }
            if let Some(slot) = self.shift_to.filter(|_| !self.shifted) {
                if self.active == Some(slot) {
                    self.shifted = true;
                    self.memory.our_stages = [0; 6];
                    self.memory.our_accuracy = 0;
                } else if let Some(BattleMenu::Command { column, row }) = b.menu {
                    return battle::step_toward(
                        (column, row),
                        (0, 1),
                        |c, r| BattleMenu::Command { column: c, row: r },
                        "POKéMON",
                        Expectation::ScreenIsNot(pokebot_state::ScreenState::BattleCommand),
                    );
                }
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
            if let (Some(d), Some(menu)) = (&o.dialogue, &o.menu) {
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
                // Our battler fainted in a wild battle: "Use next POKéMON?"
                // → YES (NO would try to run, and a trap refuses it).
                if battle::is_use_next_question(&page) {
                    self.faint_asked(ctx.events);
                    return select(menu, 0, "use next Pokémon: YES");
                }
                // "Will RED change POKéMON?": YES when another member
                // beats the announced one clearly better (the team plan),
                // else NO (the one out fights on).
                if battle::is_switch_question(&page) {
                    if let Some((slot, p_best, p_out)) =
                        self.member_for_next(b).filter(|_| !self.policy.lose)
                    {
                        ctx.events.push(super::progress(
                            "Battle",
                            format!(
                                "{} comes out against {} ({:.0} %, the one out {:.0} %)",
                                self.member_name(ctx.state, slot).unwrap_or_default(),
                                self.next_foe
                                    .as_deref()
                                    .map(party::display_name)
                                    .unwrap_or_default(),
                                p_best * 100.0,
                                p_out * 100.0
                            ),
                        ));
                        self.shift_to = Some(slot);
                        self.shifted = false;
                        return select(menu, 0, "change Pokémon: YES");
                    }
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
            // Where a catch went (party or PC) is the runtime's sensor's to
            // tell, from the same pages.
            ctx.events.push(GameEvent::BattleEnded);
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
            if let (Some(d), Some(menu)) = (&o.dialogue, &o.menu) {
                if battle::is_use_next_question(&d.lines.join(" ")) {
                    self.faint_asked(ctx.events);
                    return select(menu, 0, "use next Pokémon: YES");
                }
            }
            // The party menu's refusals ("Wild DIGLETT's ARENA TRAP
            // prevents switching!", "X can't be switched out!") show there,
            // without the battle HUD.
            if let Some(d) = &o.dialogue {
                let lead = self
                    .party
                    .lead()
                    .map(|l| l.display_name())
                    .unwrap_or_default();
                let text = d.lines.join(" ");
                self.memory.limits.observe(&text, &lead, &self.data);
                self.give_up_a_refused_shift(ctx.events);
                // The member chosen has fainted (its panel unread, the
                // belief not knowing): another is chosen, afresh (fleet
                // worker 6 chose ZUBAT again and again, then pressed B on
                // a screen that can't be left).
                if let Some(name) = no_energy(&text) {
                    if self.no_energy.insert(name.clone()) {
                        ctx.events.push(super::progress(
                            "Battle",
                            format!("{name} can't battle: choosing another"),
                        ));
                    }
                    self.shift_row = None;
                    self.shift_opens = 0;
                }
            }
            // The one out fainted: a trainer's battle opens this screen
            // itself, a wild one after YES. The HUD's 0 HP is hidden soon
            // after it reads, so its faint may not have counted (fleet
            // workers 2, 4 and 5 pressed B on this screen, which can't be
            // left, for hours): the FNT on the first panel (the one out)
            // tells.
            // In a double battle either of the two out (left) may be the one
            // (Switch, Route 16: PIDGEY fainted beside VENUSAUR, the first
            // panel read healthy, and B was pressed 40 times).
            if let Some(party) = &o.party_menu {
                let out = if party.double { 2 } else { 1 };
                let fainted = party
                    .members
                    .iter()
                    .enumerate()
                    .take(out)
                    .find(|(_, r)| r.status == Some(Status::Fainted));
                if let (None, Some((row, r))) = (self.fainted, fainted) {
                    let name = r.nickname.clone().unwrap_or_default();
                    if let Some(white_out) = self.faint_seen_at(&name, Some(row as u8), ctx.events)
                    {
                        return white_out;
                    }
                }
            }
            // Sending out the next one after a faint (the party screen
            // can't be left): never a member the screen shows fainted.
            if let (Some(party), Some(_)) = (&o.party_menu, self.fainted) {
                let fainted_row = |name: Option<&str>| {
                    party.members.iter().any(|r| {
                        name.is_some()
                            && r.nickname.as_deref() == name
                            && r.status == Some(Status::Fainted)
                    })
                };
                // A double battle's other member out, left, can't be sent
                // (emulator worker 2, Route 16: PRIMEAPE, the highest level,
                // is "already in battle").
                let mut in_battle: Vec<u8> = if party.double { vec![0, 1] } else { Vec::new() };
                in_battle.extend(
                    self.party
                        .members
                        .iter()
                        .filter(|m| {
                            self.member_name(ctx.state, m.slot)
                                .is_some_and(|n| self.no_energy.contains(&n))
                        })
                        .map(|m| m.slot),
                );
                let in_battle = in_battle.as_slice();
                let choice = self
                    .replacement(in_battle)
                    .map(|slot| (slot, self.member_name(ctx.state, slot)))
                    .filter(|(_, name)| !fainted_row(name.as_deref()))
                    .or_else(|| {
                        party
                            .members
                            .iter()
                            .enumerate()
                            .find(|(row, r)| {
                                (!party.double || *row > 1)
                                    && r.nickname
                                        .as_ref()
                                        .is_none_or(|n| !self.no_energy.contains(n))
                                    && r.status != Some(Status::Fainted)
                                    && r.hp.is_none_or(|(hp, _)| hp > 0)
                            })
                            .map(|(row, r)| (row as u8, r.nickname.clone()))
                    });
                if let Some((slot, name)) = choice {
                    return self.shift_in_party_menu(party, slot, name.as_deref());
                }
            }
            if let (Some(party), Some(slot)) =
                (&o.party_menu, self.shift_to.filter(|_| !self.shifted))
            {
                let name = ctx
                    .state
                    .party
                    .value
                    .as_ref()
                    .and_then(|p| p.get(usize::from(slot)))
                    .and_then(|m| m.nickname.value.clone())
                    .or_else(|| {
                        self.party
                            .members
                            .iter()
                            .find(|m| m.slot == slot)
                            .map(|m| m.display_name())
                    });
                return self.shift_in_party_menu(party, slot, name.as_deref());
            }
            if o.bag.is_some() || self.memory.catch.thrower.is_some() {
                return catch::in_bag(o, &data, &mut self.memory.catch, ctx.events);
            }
            if o.dialogue.is_some() {
                return advance_or_wait(o.dialogue.as_ref(), "battle text");
            }
            // The party menu with no SHIFT to make (given up): back to the
            // command menu.
            if o.party_menu.is_some() && self.fainted.is_none() {
                return Decision::Act(Action::new(
                    "party menu: nothing to SHIFT, back",
                    vec![ControllerCommand::Press(Button::B)],
                    Expectation::ScreenIsNot(pokebot_state::ScreenState::PartyMenu),
                    60,
                ));
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
        let loss_ok = loss_allowed(ctx);
        if loss_ok {
            ctx.info("battle: the script goes on if it is lost");
        }
        let mut step =
            BattleStep::new(Arc::clone(&ctx.data), *policy, ctx.after_dialogue()).loss_ok(loss_ok);
        ctx.drive(&mut step).map(|_| ()).into()
    }
}

/// The hunted species to RUN from: the foe is `spare`, and the catch was
/// decided against (declined, or the attempt given up). No species hunted
/// spares nothing, and neither does a foe never identified (Switch goal
/// run, Route 22: a `Train` hunt fights without identifying, so an unknown
/// foe equalled "no species hunted" and all 1666 battles were run from).
fn spares<'a>(spare: Option<&'a str>, c: &crate::catch::CatchMemory) -> Option<&'a str> {
    let spare = spare?;
    let foe = c.foe.as_ref().map(|(s, _)| s.as_str())?;
    (c.decided && c.attempt.is_none() && c.caught.is_none() && !c.flee && foe == spare)
        .then_some(spare)
}

/// Whether the battle starting now belongs to a script that goes on when
/// it is lost: the script being run, or an armed trigger under the player,
/// with a battle whose trainer has a victory line (the decomp's
/// `trainerbattle_earlyrival`: the rival wins, gloats, and the story goes
/// on without a white-out).
pub fn loss_allowed(ctx: &ToolContext<'_>) -> bool {
    use pokebot_world::predicate::{BeliefView, Truth};
    let Some(events) = ctx.world.events() else {
        return false;
    };
    let belief = crate::belief_view::StateBelief(ctx.state());
    let mut scripts: Vec<&str> = ctx.running_script.iter().map(String::as_str).collect();
    if let Some(pose) = ctx.pose() {
        for t in events
            .triggers
            .iter()
            .filter(|t| t.map == pose.map && (t.x, t.y) == (pose.x, pose.y))
        {
            let armed = pokebot_world::route::requirement_of(&t.when)
                .is_none_or(|req| !req.iter().any(|q| belief.eval(q) == Truth::False));
            if let (true, Some(s)) = (armed, t.script.as_deref()) {
                scripts.push(s);
            }
        }
    }
    scripts.iter().filter_map(|s| events.script(s)).any(|s| {
        s.paths.iter().any(|p| {
            p.does.iter().any(|e| {
                matches!(
                    e,
                    pokebot_world::events::Effect::Battle {
                        victory: Some(_),
                        ..
                    }
                )
            })
        })
    })
}

/// The member the party screen refused to send out: "ZUBAT has no energy
/// left to battle!" (`gText_PkmnHasNoEnergy`).
fn no_energy(text: &str) -> Option<String> {
    let (name, _) = text.split_once(" has no energy")?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catch::CatchMemory;

    fn decided(foe: Option<&str>) -> CatchMemory {
        let mut c = CatchMemory::default();
        c.decided = true;
        c.foe = foe.map(|s| (s.to_owned(), 4));
        c
    }

    #[test]
    fn only_the_hunted_species_is_run_from() {
        let pidgey = Some("SPECIES_PIDGEY");
        assert_eq!(spares(pidgey, &decided(pidgey)), pidgey);
        assert_eq!(spares(pidgey, &decided(Some("SPECIES_RATTATA"))), None);
        // Not identified (yet): nothing to spare.
        assert_eq!(spares(pidgey, &decided(None)), None);
        // Being caught, or already fleeing: nothing more to do.
        let mut fleeing = decided(pidgey);
        fleeing.flee = true;
        assert_eq!(spares(pidgey, &fleeing), None);
    }

    /// Fleet continue-6, Victory Road: two DUGTRIO, the first fainted. The
    /// send-out went to the first row with the name, never offered SHIFT,
    /// and gave up with a B the send-out can't be left by. The one able
    /// to fight is the row.
    #[test]
    fn of_two_members_of_one_name_the_able_one_is_sent_out() {
        let Ok(data) = pokebot_gamedata::GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let row = |name: &str, hp: u16| pokebot_state::PartyRowObservation {
            nickname: Some(name.into()),
            level: None,
            hp: Some((hp, 65)),
            status: Some(if hp == 0 {
                pokebot_state::Status::Fainted
            } else {
                pokebot_state::Status::Healthy
            }),
        };
        let menu = pokebot_state::PartyMenuObservation {
            count: 6,
            selected: Some(1),
            members: vec![
                row("SPEAROW", 0),
                row("DUGTRIO", 0),
                row("RATTATA", 0),
                row("PIDGEY", 0),
                row("DUGTRIO", 65),
                row("BLASTOISE", 0),
            ],
            ..Default::default()
        };
        let mut step = BattleStep::new(Arc::new(data), BattlePlan::Fight, false).shifting_to(4);
        match step.shift_in_party_menu(&menu, 4, Some("DUGTRIO")) {
            Decision::Act(a) => assert_eq!(a.label, "shift: Down toward slot 4"),
            _ => panic!("expected a move toward the able DUGTRIO"),
        }
    }

    /// In battle the party menu is in battle order: the member to SHIFT to
    /// is the row with its name (fleet worker 6's switches went to the row
    /// numbered like its party slot).
    #[test]
    fn the_shift_goes_to_the_row_with_the_members_name() {
        let Ok(data) = pokebot_gamedata::GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let row = |name: &str| pokebot_state::PartyRowObservation {
            nickname: Some(name.into()),
            level: None,
            hp: None,
            status: None,
        };
        let mut menu = pokebot_state::PartyMenuObservation {
            count: 3,
            selected: Some(0),
            members: vec![row("RATTATA"), row("BEEDRILL"), row("PIDGEY")],
            ..Default::default()
        };
        let mut step = BattleStep::new(Arc::new(data), BattlePlan::Fight, false).shifting_to(2);
        let label = |d: Decision| match d {
            Decision::Act(a) => a.label,
            _ => String::new(),
        };
        // BEEDRILL is party slot 2 but battle row 1.
        assert_eq!(
            label(step.shift_in_party_menu(&menu, 2, Some("BEEDRILL"))),
            "shift: Down toward slot 1"
        );
        menu.selected = Some(1);
        assert_eq!(
            label(step.shift_in_party_menu(&menu, 2, Some("BEEDRILL"))),
            "shift: open the member's actions"
        );
        // The action window opens on BEEDRILL and hides the names below
        // it: the row read on the list stands (fleet worker 3 closed and
        // reopened it for two hours), and SHIFT is chosen.
        let hidden = pokebot_state::PartyRowObservation {
            nickname: None,
            level: None,
            hp: None,
            status: None,
        };
        let mut actions = pokebot_state::PartyMenuObservation {
            count: 3,
            selected: Some(1),
            actions: true,
            options: vec!["SHIFT".into(), "SUMMARY".into(), "CANCEL".into()],
            option_cursor: Some(0),
            members: vec![row("RATTATA"), hidden.clone(), hidden],
            ..Default::default()
        };
        assert_eq!(
            label(step.shift_in_party_menu(&actions, 2, Some("BEEDRILL"))),
            "choose SHIFT"
        );
        // Opened again and again without a SHIFT: given up, fight on.
        actions.actions = false;
        for _ in 0..MAX_SHIFT_OPENS - 1 {
            step.shift_in_party_menu(&menu, 2, Some("BEEDRILL"));
        }
        assert!(label(step.shift_in_party_menu(&menu, 2, Some("BEEDRILL"))).contains("gave up"));
        // No name read: the slot.
        let mut fresh = BattleStep::new(
            Arc::new(
                pokebot_gamedata::GameData::load(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../data/world/gamedata.json"),
                )
                .unwrap(),
            ),
            BattlePlan::Fight,
            false,
        )
        .shifting_to(2);
        menu.selected = Some(0);
        assert_eq!(
            label(fresh.shift_in_party_menu(&menu, 2, None)),
            "shift: Down toward slot 2"
        );
    }

    /// A training battle identifies its foe only when later steps of the
    /// plan want a species caught (it then catches nothing else).
    #[test]
    fn a_training_battle_identifies_only_for_a_side_catch() {
        let Ok(data) = GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        ) else {
            return;
        };
        let data = Arc::new(data);
        let mut events = Vec::new();
        let mut plain = BattleStep::new(Arc::clone(&data), BattlePlan::Fight, false);
        plain.begin(&mut events);
        assert!(plain.memory.catch.decided && !plain.memory.catch.only_wanted);
        let mut side = crate::catch::SideCatch::default();
        side.species.insert("SPECIES_PIDGEY".into());
        let mut step =
            BattleStep::new(data, BattlePlan::Fight, false).catching_on_the_side(side.clone());
        step.begin(&mut events);
        assert!(!step.memory.catch.decided && step.memory.catch.only_wanted);
        assert_eq!(step.memory.catch.side, side);
    }

    /// Switch goal run, Route 22: `Train(BULBASAUR to Lv10)` fights with
    /// `BattlePlan::Fight`, which never identifies the foe and hunts no
    /// species; `None == None` made every wild battle a RUN ("not catching
    /// now: running rather than fainting it", 1666 times, no experience).
    #[test]
    fn a_training_battle_spares_nothing() {
        assert_eq!(spares(None, &decided(None)), None);
        assert_eq!(spares(None, &decided(Some("SPECIES_RATTATA"))), None);
    }

    fn member(species: &str, level: u8, hp: u16, mv: &[&str]) -> pokebot_state::PartyMon {
        use pokebot_state::{Knowledge, MoveSlot, PartyMon};
        let mut m = PartyMon {
            species: Knowledge::observed(species.to_owned(), 1),
            level: Knowledge::observed(level, 1),
            hp: Knowledge::observed((hp, hp), 1),
            ..PartyMon::default()
        };
        for (i, x) in mv.iter().enumerate() {
            m.moves[i] = Some(MoveSlot {
                mv: Knowledge::observed((*x).to_owned(), 1),
                pp: Knowledge::observed((20, 20), 1),
            });
        }
        m
    }

    /// Switch goal run, Route 4: `Train(PIDGEY to Lv9)` led with a Lv3
    /// PIDGEY for IVYSAUR (slot 1) to carry, but the first command menu
    /// went Down, Right, A: RUN ("Can't escape!" three times) until
    /// SPEAROW Lv12 fainted PIDGEY. The carrier comes out: POKéMON.
    #[test]
    fn a_switch_trained_lead_shifts_to_the_carrier_at_the_first_command_menu() {
        use pokebot_state::{Knowledge, Observation};
        use pokebot_vision::{FireRedPerception, PerceptionSystem};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(data), Ok(font)) = (
            GameData::load(root.join("data/world/gamedata.json")),
            pokebot_vision::text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let data = Arc::new(data);
        let mut vision = FireRedPerception::default().with_font(Arc::new(font));
        let mut observe = |name: &str, id: u64| -> Observation {
            let image =
                pokebot_video::png::load(root.join(format!("captures/fixtures/{name}.png")))
                    .unwrap();
            let frame =
                pokebot_core::NormalizedFrame::new(id, std::time::Instant::now(), image).unwrap();
            vision.observe(&frame)
        };
        let state = pokebot_state::GameState {
            party: Knowledge::observed(
                vec![
                    member("SPECIES_PIDGEY", 3, 16, &["MOVE_TACKLE"]),
                    member(
                        "SPECIES_IVYSAUR",
                        25,
                        68,
                        &[
                            "MOVE_TACKLE",
                            "MOVE_SLEEP_POWDER",
                            "MOVE_RAZOR_LEAF",
                            "MOVE_VINE_WHIP",
                        ],
                    ),
                ],
                1,
            ),
            ..pokebot_state::GameState::default()
        };
        let mut step = BattleStep::new(Arc::clone(&data), BattlePlan::Fight, false).shifting_to(1);
        let mut events = Vec::new();
        let mut run = |o: &Observation| {
            step.next(&mut StepContext {
                observation: o,
                state: &state,
                events: &mut events,
                quiet_frames: 0,
                frame: None,
                learned: &[],
            })
        };
        let label = |d: &Decision| match d {
            Decision::Act(a) => a.label.clone(),
            Decision::Wait(w) => format!("wait: {w}"),
            Decision::Done(w) => format!("done: {w}"),
            Decision::Fail(w) => format!("fail: {w}"),
        };
        let mut id = 0;
        for name in ["switch-shift-go", "switch-shift-go"] {
            id += 1;
            run(&observe(name, id));
        }
        let mut last = String::new();
        for name in ["switch-shift-command-fight", "switch-shift-command-fight"] {
            id += 1;
            last = label(&run(&observe(name, id)));
        }
        assert_eq!(last, "cursor to POKéMON: Down");
        for name in [
            "switch-shift-command-pokemon",
            "switch-shift-command-pokemon",
        ] {
            id += 1;
            last = label(&run(&observe(name, id)));
        }
        assert_eq!(last, "choose POKéMON");
    }

    /// Fleet worker 3, a trainer battle on Route 3: the belief still held a
    /// WEEDLE in slot 1, so the defensive switch went for it, but the
    /// battle's party showed BULBASAUR alone and no ▶ to read. It waited,
    /// pressed B and chose POKéMON again for 110k frames. A slot the menu
    /// doesn't show is given up: back to the fight.
    #[test]
    fn a_shift_to_a_member_the_party_menu_lacks_goes_back() {
        use pokebot_vision::{FireRedPerception, PerceptionSystem};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(data), Ok(font)) = (
            GameData::load(root.join("data/world/gamedata.json")),
            pokebot_vision::text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let mut vision = FireRedPerception::default().with_font(Arc::new(font));
        let image = pokebot_video::png::load(
            root.join("captures/fixtures/emu-battle-party-one-member.png"),
        )
        .unwrap();
        let frame =
            pokebot_core::NormalizedFrame::new(1, std::time::Instant::now(), image).unwrap();
        let o = vision.observe(&frame);
        let party = o.party_menu.expect("the battle's party menu");
        assert_eq!(party.count, 1);
        let mut step = BattleStep::new(Arc::new(data), BattlePlan::Fight, false).shifting_to(1);
        match step.shift_in_party_menu(&party, 1, None) {
            Decision::Act(a) => assert!(a.label.starts_with("shift: no slot 1"), "{}", a.label),
            _ => panic!("no press"),
        }
        assert!(step.shifted);
    }

    /// Switch, Diglett's Cave: switch-training CATERPIE (Lv5) for VENUSAUR
    /// (Lv33), the SHIFT met "Wild DIGLETT's ARENA TRAP prevents
    /// switching!", and the member was chosen again 45 times until the run
    /// was stopped. The refusal is read over the party menu: the SHIFT is
    /// given up and the menu left.
    #[test]
    fn a_shift_refused_by_a_trap_is_given_up() {
        use pokebot_state::{Knowledge, Observation};
        use pokebot_vision::{FireRedPerception, PerceptionSystem};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(data), Ok(font)) = (
            GameData::load(root.join("data/world/gamedata.json")),
            pokebot_vision::text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let mut vision = FireRedPerception::default().with_font(Arc::new(font));
        let mut observe = |name: &str, id: u64| -> Observation {
            let image =
                pokebot_video::png::load(root.join(format!("captures/fixtures/{name}.png")))
                    .unwrap();
            let frame =
                pokebot_core::NormalizedFrame::new(id, std::time::Instant::now(), image).unwrap();
            vision.observe(&frame)
        };
        let state = pokebot_state::GameState {
            party: Knowledge::observed(
                vec![
                    member(
                        "SPECIES_CATERPIE",
                        5,
                        20,
                        &["MOVE_TACKLE", "MOVE_STRING_SHOT"],
                    ),
                    member("SPECIES_VENUSAUR", 33, 100, &["MOVE_RAZOR_LEAF"]),
                ],
                1,
            ),
            ..pokebot_state::GameState::default()
        };
        let mut step = BattleStep::new(Arc::new(data), BattlePlan::Fight, false).shifting_to(1);
        step.started = true;
        step.in_battle = true;
        let mut events = Vec::new();
        let mut run = |o: &Observation, events: &mut Vec<GameEvent>| {
            step_label(&step_next(&mut step, o, &state, events))
        };
        // The page, read on two frames.
        for id in 1..=2 {
            run(
                &observe("switch-arena-trap-prevents-switching", id),
                &mut events,
            );
        }
        assert!(
            events.iter().any(|e| matches!(e,
                GameEvent::GoalProgress { detail, .. } if detail.starts_with("can't SHIFT (Wild DIGLETT's ARENA TRAP"))),
            "{events:?}"
        );
        let last = run(&observe("switch-arena-trap-party-actions", 3), &mut events);
        assert_eq!(last, "party menu: nothing to SHIFT, back");
    }

    /// The user's rule: the game restarts only when all the Pokémon have
    /// fainted. A trapped CATERPIE fainting against DIGLETT is not the end:
    /// "Use next POKéMON?" YES, and VENUSAUR is sent out. The last one
    /// fainting is a white-out.
    #[test]
    fn a_faint_sends_out_the_next_one_and_only_the_last_is_a_white_out() {
        use pokebot_state::{
            BattleObservation, DialogueKind, DialogueObservation, Knowledge, MenuObservation,
            Observation, Observed, PartyMenuObservation, PartyRowObservation, Region, ScreenState,
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(data) = GameData::load(root.join("data/world/gamedata.json")) else {
            return;
        };
        let data = Arc::new(data);
        let bare = |id: u64, screen: ScreenState| {
            Observation::bare(
                id,
                Observed {
                    value: screen,
                    detector: "test".into(),
                },
                Default::default(),
            )
        };
        let fainting = |id: u64| {
            let mut o = bare(id, ScreenState::BattleText);
            o.battle = Some(BattleObservation {
                menu: None,
                player_name: Some("CATERPIE".into()),
                player_level: Some(5),
                player_hp_numbers: Some((0, 20)),
                opponent_name: Some("DIGLETT".into()),
                opponent_level: Some(17),
                player_hp: None,
                opponent_hp: Some(1000),
                move_pp: None,
                move_names: Vec::new(),
                opponent_caught: None,
                opponent_shiny: None,
                level_up_stats: None,
            });
            o
        };
        let question = |id: u64| {
            let mut o = bare(id, ScreenState::Dialogue);
            o.dialogue = Some(DialogueObservation {
                kind: DialogueKind::BattleText,
                region: Region::new(8, 119, 224, 34),
                waiting_for_input: false,
                arrow: None,
                stable_frames: 10,
                text_cells: vec![1; 4],
                lines: vec!["Use next POKéMON?".into()],
                help: false,
            });
            o.menu = Some(MenuObservation {
                window: Region::new(180, 60, 50, 40),
                rows: 2,
                cursor_row: 0,
                cursor_y: 70,
            });
            o
        };
        let row = |name: &str, hp: u16| PartyRowObservation {
            nickname: Some(name.into()),
            level: None,
            hp: Some((hp, hp.max(20))),
            status: None,
        };
        let party_menu = |id: u64, actions: bool| {
            let mut o = bare(id, ScreenState::PartyMenu);
            o.party_menu = Some(PartyMenuObservation {
                count: 2,
                selected: Some(if actions { 1 } else { 0 }),
                actions,
                options: if actions {
                    vec!["SEND OUT".into(), "SUMMARY".into(), "CANCEL".into()]
                } else {
                    Vec::new()
                },
                option_cursor: actions.then_some(0),
                members: vec![row("CATERPIE", 0), row("VENUSAUR", 100)],
                ..PartyMenuObservation::default()
            });
            o
        };
        let state = |members: Vec<pokebot_state::PartyMon>| pokebot_state::GameState {
            party: Knowledge::observed(members, 1),
            ..pokebot_state::GameState::default()
        };
        let two = state(vec![
            member("SPECIES_CATERPIE", 5, 20, &["MOVE_TACKLE"]),
            member("SPECIES_VENUSAUR", 33, 100, &["MOVE_RAZOR_LEAF"]),
        ]);
        let mut step = BattleStep::new(Arc::clone(&data), BattlePlan::Fight, false);
        step.started = true;
        step.in_battle = true;
        let mut events = Vec::new();
        let mut last = String::new();
        for id in [1, 20, 40] {
            last = step_label(&step_next(&mut step, &fainting(id), &two, &mut events));
        }
        assert!(!last.starts_with("fail"), "{last}");
        assert!(
            events.iter().any(|e| matches!(e,
                GameEvent::GoalProgress { detail, .. } if detail == "CATERPIE fainted: sending out the next one")),
            "{events:?}"
        );
        let label = step_label(&step_next(&mut step, &question(50), &two, &mut events));
        assert_eq!(label, "use next Pokémon: YES");
        let label = step_label(&step_next(
            &mut step,
            &party_menu(60, false),
            &two,
            &mut events,
        ));
        assert!(label.contains("toward slot 1"), "{label}");
        let label = step_label(&step_next(
            &mut step,
            &party_menu(70, true),
            &two,
            &mut events,
        ));
        assert_eq!(label, "choose SHIFT");

        // A double battle (emulator worker 2, Route 16): CATERPIE fainted
        // beside VENUSAUR, the highest level, which is already in battle
        // (left, slot 0): the next one is PIDGEY, from the right.
        let three = state(vec![
            member("SPECIES_VENUSAUR", 33, 100, &["MOVE_RAZOR_LEAF"]),
            member("SPECIES_CATERPIE", 5, 20, &["MOVE_TACKLE"]),
            member("SPECIES_PIDGEY", 9, 30, &["MOVE_TACKLE"]),
        ]);
        let mut step = BattleStep::new(Arc::clone(&data), BattlePlan::Fight, false);
        step.started = true;
        step.in_battle = true;
        for id in [1, 20, 40] {
            step_next(&mut step, &fainting(id), &three, &mut events);
        }
        let mut double = party_menu(60, false);
        if let Some(m) = double.party_menu.as_mut() {
            m.count = 3;
            m.double = true;
            m.members = vec![row("VENUSAUR", 100), row("CATERPIE", 0), row("PIDGEY", 30)];
        }
        let label = step_label(&step_next(&mut step, &double, &three, &mut events));
        assert!(label.contains("toward slot 2"), "{label}");

        // A double battle's second one out fainted, and only its panel
        // tells (Switch, Route 16: PIDGEY beside VENUSAUR; the first panel
        // read healthy and B was pressed 40 times): the next one is sent.
        let mut step = BattleStep::new(Arc::clone(&data), BattlePlan::Fight, false);
        step.started = true;
        step.in_battle = true;
        let mut double = party_menu(200, false);
        if let Some(m) = double.party_menu.as_mut() {
            m.count = 3;
            m.double = true;
            m.members = vec![
                row("VENUSAUR", 100),
                PartyRowObservation {
                    nickname: None,
                    level: None,
                    hp: Some((0, 20)),
                    status: Some(Status::Fainted),
                },
                row("PIDGEY", 30),
            ];
        }
        let label = step_label(&step_next(&mut step, &double, &three, &mut events));
        assert!(label.contains("toward slot 2"), "{label}");

        // The one chosen has fainted, its panel unread and the belief not
        // knowing (fleet worker 6, Cerulean's rival: ZUBAT chosen again
        // and again, then B on a screen that can't be left): once the game
        // says so, another is chosen.
        let others = state(vec![
            member("SPECIES_CATERPIE", 5, 20, &["MOVE_TACKLE"]),
            member("SPECIES_VENUSAUR", 33, 100, &["MOVE_RAZOR_LEAF"]),
            member("SPECIES_PIDGEY", 9, 30, &["MOVE_TACKLE"]),
        ]);
        let mut step = BattleStep::new(Arc::clone(&data), BattlePlan::Fight, false);
        step.started = true;
        step.in_battle = true;
        for id in [1, 20, 40] {
            step_next(&mut step, &fainting(id), &others, &mut events);
        }
        let menu = |id: u64| {
            let mut o = party_menu(id, false);
            if let Some(m) = o.party_menu.as_mut() {
                m.count = 3;
                m.members = vec![
                    row("CATERPIE", 0),
                    PartyRowObservation {
                        nickname: None,
                        level: None,
                        hp: None,
                        status: None,
                    },
                    row("PIDGEY", 30),
                ];
            }
            o
        };
        let label = step_label(&step_next(&mut step, &menu(60), &others, &mut events));
        assert!(label.contains("toward slot 1"), "{label}");
        let refused = |id: u64| {
            let mut o = bare(id, ScreenState::Dialogue);
            o.dialogue = Some(DialogueObservation {
                kind: DialogueKind::MessageBox,
                region: Region::new(8, 119, 224, 34),
                waiting_for_input: true,
                arrow: None,
                stable_frames: 10,
                text_cells: vec![1; 4],
                lines: vec!["VENUSAUR has no energy".into(), "left to battle!".into()],
                help: false,
            });
            o
        };
        for id in [70, 72] {
            step_next(&mut step, &refused(id), &others, &mut events);
        }
        let label = step_label(&step_next(&mut step, &menu(80), &others, &mut events));
        assert!(label.contains("toward slot 2"), "{label}");

        // CATERPIE alone: its faint is the white-out.
        let one = state(vec![member("SPECIES_CATERPIE", 5, 20, &["MOVE_TACKLE"])]);
        let mut step = BattleStep::new(data, BattlePlan::Fight, false);
        step.started = true;
        step.in_battle = true;
        let mut last = String::new();
        for id in [1, 20, 40] {
            last = step_label(&step_next(&mut step, &fainting(id), &one, &mut events));
        }
        assert_eq!(
            last,
            "fail: whited out: our last Pokémon fainted (CATERPIE)"
        );
    }

    /// Fleet worker 5, a Team Rocket Grunt in Mt. Moon: RATTATA fainted
    /// and the game opened the party screen (a trainer's battle asks
    /// nothing). The HUD's 0 HP had hidden before the faint counted, so
    /// the step pressed B there, which the screen ignores, for hours. The
    /// FNT on the first panel is the faint: the next one is sent out.
    #[test]
    fn a_faint_is_seen_on_the_party_screen() {
        use pokebot_state::{Knowledge, Observation};
        use pokebot_vision::{FireRedPerception, PerceptionSystem};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (Ok(data), Ok(font)) = (
            GameData::load(root.join("data/world/gamedata.json")),
            pokebot_vision::text::Font::load(root.join("data/world/font_normal.json")),
        ) else {
            return;
        };
        let Ok(image) = pokebot_video::png::load(
            root.join("captures/fixtures/emu-battle-party-lead-fainted.png"),
        ) else {
            return;
        };
        let Ok(small) = pokebot_vision::text::Font::load(root.join("data/world/font_small.json"))
        else {
            return;
        };
        let mut vision = FireRedPerception::default()
            .with_font(Arc::new(font))
            .with_small_font(Arc::new(small));
        let frame =
            pokebot_core::NormalizedFrame::new(1, std::time::Instant::now(), image).unwrap();
        let o: Observation = vision.observe(&frame);
        let state = pokebot_state::GameState {
            party: Knowledge::observed(
                vec![
                    member("SPECIES_RATTATA", 9, 26, &["MOVE_TACKLE"]),
                    member("SPECIES_CHARMELEON", 24, 62, &["MOVE_EMBER"]),
                    member("SPECIES_GEODUDE", 11, 32, &["MOVE_TACKLE"]),
                    member("SPECIES_ZUBAT", 8, 25, &["MOVE_LEECH_LIFE"]),
                    member("SPECIES_MANKEY", 8, 26, &["MOVE_SCRATCH"]),
                    member("SPECIES_PARAS", 5, 19, &["MOVE_SCRATCH"]),
                ],
                1,
            ),
            ..pokebot_state::GameState::default()
        };
        // A step begun on this screen (the faint happened in the one before).
        let mut step = BattleStep::new(Arc::new(data), BattlePlan::Fight, false);
        step.started = true;
        step.in_battle = true;
        let mut events = Vec::new();
        let label = step_label(&step_next(&mut step, &o, &state, &mut events));
        assert!(label.contains("toward slot"), "{label}");
        assert!(
            events.iter().any(|e| matches!(e,
                GameEvent::GoalProgress { detail, .. } if detail == "RATTATA fainted: sending out the next one")),
            "{events:?}"
        );
    }

    /// Fleet worker 4: PARAS Lv10, switch-trained with VENUSAUR, took
    /// Lv13–16 ODDISH alone (little risk, many turns of SCRATCH) while
    /// VENUSAUR, which wins in a turn or two, waited. A slow win is handed
    /// to the carrier: half the experience in a fraction of the time.
    #[test]
    fn a_trainee_hands_a_slow_win_to_its_carrier() {
        use pokebot_state::{BattleObservation, Knowledge, Observation, Observed, ScreenState};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(data) = GameData::load(root.join("data/world/gamedata.json")) else {
            return;
        };
        let data = Arc::new(data);
        let mut o = Observation::bare(
            1,
            Observed {
                value: ScreenState::BattleCommand,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.battle = Some(BattleObservation {
            menu: Some(BattleMenu::Command { column: 0, row: 0 }),
            player_name: Some("PARAS".into()),
            player_level: Some(10),
            player_hp_numbers: Some((31, 31)),
            opponent_name: Some("ODDISH".into()),
            opponent_level: Some(16),
            player_hp: None,
            opponent_hp: Some(1000),
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: Some(true),
            opponent_shiny: None,
            level_up_stats: None,
        });
        let state = pokebot_state::GameState {
            party: Knowledge::observed(
                vec![
                    member(
                        "SPECIES_PARAS",
                        10,
                        31,
                        &["MOVE_SCRATCH", "MOVE_STUN_SPORE"],
                    ),
                    member(
                        "SPECIES_VENUSAUR",
                        36,
                        110,
                        &[
                            "MOVE_CUT",
                            "MOVE_SLEEP_POWDER",
                            "MOVE_RAZOR_LEAF",
                            "MOVE_VINE_WHIP",
                        ],
                    ),
                ],
                1,
            ),
            ..pokebot_state::GameState::default()
        };
        let mut events = Vec::new();
        let mut step = BattleStep::new(data, BattlePlan::Fight, false).shifting_to(1);
        step.started = true;
        step.in_battle = true;
        let label = step_label(&step_next(&mut step, &o, &state, &mut events));
        assert!(label.contains("POKéMON"), "{label} {events:?}");
    }

    /// The team plan meets each of a trainer's Pokémon with the member that
    /// beats it best. ERIKA announces VICTREEBEL: VENUSAUR's grass moves
    /// are resisted, PIDGEOTTO's flying ones are not, so "Will RED change
    /// POKéMON?" is YES and PIDGEOTTO comes out. Against a foe VENUSAUR
    /// beats best (ONIX, weak to grass), NO.
    #[test]
    fn a_trainers_next_pokemon_is_met_by_the_member_that_beats_it() {
        use pokebot_state::{
            BattleObservation, DialogueKind, DialogueObservation, Knowledge, MenuObservation,
            Observation, Observed, Region, ScreenState,
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(data) = GameData::load(root.join("data/world/gamedata.json")) else {
            return;
        };
        let data = Arc::new(data);
        let page = |id: u64, text: &str, question: bool| {
            let mut o = Observation::bare(
                id,
                Observed {
                    value: ScreenState::BattleText,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.battle = Some(BattleObservation {
                menu: None,
                player_name: Some("VENUSAUR".into()),
                player_level: Some(36),
                player_hp_numbers: Some((110, 110)),
                opponent_name: Some("TANGELA".into()),
                opponent_level: Some(24),
                player_hp: None,
                opponent_hp: Some(0),
                move_pp: None,
                move_names: Vec::new(),
                opponent_caught: Some(true),
                opponent_shiny: None,
                level_up_stats: None,
            });
            o.dialogue = Some(DialogueObservation {
                kind: DialogueKind::BattleText,
                region: Region::new(8, 119, 224, 34),
                waiting_for_input: !question,
                arrow: None,
                stable_frames: 30,
                text_cells: vec![1; 4],
                lines: vec![text.into()],
                help: false,
            });
            if question {
                o.menu = Some(MenuObservation {
                    window: Region::new(180, 60, 50, 40),
                    rows: 2,
                    cursor_row: 0,
                    cursor_y: 70,
                });
            }
            o
        };
        let state = pokebot_state::GameState {
            party: Knowledge::observed(
                vec![
                    member(
                        "SPECIES_VENUSAUR",
                        36,
                        110,
                        &["MOVE_RAZOR_LEAF", "MOVE_VINE_WHIP", "MOVE_SLEEP_POWDER"],
                    ),
                    member(
                        "SPECIES_PIDGEOTTO",
                        30,
                        90,
                        &["MOVE_WING_ATTACK", "MOVE_GUST"],
                    ),
                ],
                1,
            ),
            ..pokebot_state::GameState::default()
        };
        let answer = |foe: &str| {
            let mut step = BattleStep::new(Arc::clone(&data), BattlePlan::Fight, true);
            step.started = true;
            step.in_battle = true;
            let mut events = Vec::new();
            let text = format!("LEADER ERIKA is about to use {foe}.");
            for id in 1..=2 {
                step_next(&mut step, &page(id, &text, false), &state, &mut events);
            }
            step_label(&step_next(
                &mut step,
                &page(3, "Will RED change POKéMON?", true),
                &state,
                &mut events,
            ))
        };
        assert_eq!(answer("VICTREEBEL"), "change Pokémon: YES");
        assert_eq!(answer("ONIX"), "change Pokémon: NO");
    }

    /// Fleet worker 2: MANKEY, switch-trained since Lv3 with CHARMANDER
    /// carrying it, was Lv10 and still called back at every Route 22
    /// battle against Lv2–5 foes, taking half the experience. The trainee
    /// now fights alone what it beats; the carrier comes out against a foe
    /// it doesn't.
    #[test]
    fn a_trainee_fights_alone_the_wild_ones_it_beats() {
        use pokebot_state::{BattleObservation, Knowledge, Observation, Observed, ScreenState};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(data) = GameData::load(root.join("data/world/gamedata.json")) else {
            return;
        };
        let data = Arc::new(data);
        let command = |foe: &str, level: u8| {
            let mut o = Observation::bare(
                1,
                Observed {
                    value: ScreenState::BattleCommand,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.battle = Some(BattleObservation {
                menu: Some(BattleMenu::Command { column: 0, row: 0 }),
                player_name: Some("MANKEY".into()),
                player_level: Some(10),
                player_hp_numbers: Some((31, 31)),
                opponent_name: Some(foe.into()),
                opponent_level: Some(level),
                player_hp: None,
                opponent_hp: Some(1000),
                move_pp: None,
                move_names: Vec::new(),
                opponent_caught: Some(true),
                opponent_shiny: None,
                level_up_stats: None,
            });
            o
        };
        let state = pokebot_state::GameState {
            party: Knowledge::observed(
                vec![
                    member(
                        "SPECIES_MANKEY",
                        10,
                        31,
                        &[
                            "MOVE_SCRATCH",
                            "MOVE_LEER",
                            "MOVE_LOW_KICK",
                            "MOVE_KARATE_CHOP",
                        ],
                    ),
                    member(
                        "SPECIES_CHARMANDER",
                        14,
                        38,
                        &["MOVE_SCRATCH", "MOVE_EMBER"],
                    ),
                ],
                1,
            ),
            ..pokebot_state::GameState::default()
        };
        let mut events = Vec::new();
        let mut step = BattleStep::new(Arc::clone(&data), BattlePlan::Fight, false).shifting_to(1);
        step.started = true;
        step.in_battle = true;
        let label = step_label(&step_next(
            &mut step,
            &command("RATTATA", 4),
            &state,
            &mut events,
        ));
        assert!(!label.contains("POKéMON"), "{label}");
        assert!(
            events.iter().any(|e| matches!(e,
                GameEvent::GoalProgress { detail, .. } if detail.starts_with("MANKEY takes RATTATA Lv4 alone"))),
            "{events:?}"
        );
        // A foe it doesn't beat: the carrier comes out.
        let mut step = BattleStep::new(data, BattlePlan::Fight, false).shifting_to(1);
        step.started = true;
        step.in_battle = true;
        let label = step_label(&step_next(
            &mut step,
            &command("PRIMEAPE", 30),
            &state,
            &mut events,
        ));
        assert!(label.contains("POKéMON"), "{label}");
    }

    fn step_next(
        step: &mut BattleStep,
        o: &pokebot_state::Observation,
        state: &pokebot_state::GameState,
        events: &mut Vec<GameEvent>,
    ) -> Decision {
        step.next(&mut StepContext {
            observation: o,
            state,
            events,
            quiet_frames: 0,
            frame: None,
            learned: &[],
        })
    }

    fn step_label(d: &Decision) -> String {
        match d {
            Decision::Act(a) => a.label.clone(),
            Decision::Wait(w) => format!("wait: {w}"),
            Decision::Done(w) => format!("done: {w}"),
            Decision::Fail(w) => format!("fail: {w}"),
        }
    }
}
