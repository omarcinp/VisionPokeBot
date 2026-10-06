//! Putting a party member first (Start → POKéMON → the member → SWITCH →
//! the first slot), so it leads the next battle. The planner counts on the
//! team meeting each of a trainer's Pokémon with the member that beats it
//! best (`pokebot_planner::team_vs_trainer`): the one for the first leads
//! (fleet workers: CHARMANDER led against Brock and fainted to ONIX while
//! the MANKEY trained for him waited in the party).

use pokebot_core::Button;
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, GameState};

use super::menu::{
    open_start_menu, pick_row, Closer, MenuRow, Retries, CURSOR_FRAMES, SCREEN_FRAMES,
};
use super::{progress, Expects, StepContext, ToolContext, ToolError, ToolStep};
use crate::bag::fits;
use crate::party::Party;
use crate::{Action, Decision, Expectation, Outcome};

/// A better fighter leads only when its chance beats the lead's by this.
pub const LEAD_MARGIN: f64 = 0.05;

/// Moves party slot `slot` to the front.
pub struct LeadWith {
    slot: u8,
    /// Its species, to know it leads once the state says so.
    species: String,
    retries: Retries,
    closer: Closer,
    /// SWITCH was chosen; the next A on the first slot swaps.
    switching: bool,
    swapped: bool,
    reordered: bool,
    /// The action window open is the one the step opened on `slot`: the
    /// window covers the last slots' panels, whose highlight then reads
    /// as another (fleet continue-4: HYPNO in slot 5, its actions opened,
    /// closed as "another member's" and opened again for 18 minutes).
    opened: bool,
    /// Times the member's actions were opened.
    opens: u32,
}

/// Openings of the member's actions before the step gives up.
const MAX_OPENS: u32 = 6;

impl LeadWith {
    pub fn new(slot: u8, species: &str) -> Self {
        Self {
            slot,
            species: species.to_owned(),
            retries: Retries::default(),
            closer: Closer::default(),
            switching: false,
            swapped: false,
            reordered: false,
            opened: false,
            opens: 0,
        }
    }

    fn leads(&self, state: &GameState) -> bool {
        state
            .party
            .value
            .as_ref()
            .and_then(|p| p.first())
            .and_then(|m| m.species.value.as_deref())
            == Some(self.species.as_str())
    }
}

impl ToolStep for LeadWith {
    fn expects(&self) -> Expects {
        Expects::MENUS
    }

    fn on_outcome(&mut self, action: &Action, outcome: Outcome, _: &mut StepContext<'_>) {
        if outcome != Outcome::Confirmed {
            self.retries.failed();
        } else if action.label == "choose SWITCH" {
            self.switching = true;
            self.opened = false;
        } else if action.label == "swap into the first slot" {
            self.swapped = true;
        } else if action.label == "open the member's actions" {
            self.opened = true;
            self.opens += 1;
        } else if action.label == "close another member's actions" {
            self.opened = false;
        }
    }

    fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
        let o = ctx.observation;
        if self.retries.exhausted() {
            return Decision::Fail(format!(
                "leading with slot {}: no progress in {}",
                self.slot,
                self.retries.phase()
            ));
        }
        if self.leads(ctx.state) {
            // The swap is in the state: back to the field.
            return self.closer.next(o, "leads");
        }
        if self.swapped && !self.reordered {
            // The game swapped the two; the sensor reorders the party when
            // the panels name every member uniquely, else it is told here.
            if o.party_menu.as_ref().is_some_and(|m| !m.actions) {
                self.retries.enter("reordered");
                if let Some(n) = ctx.state.party.value.as_ref().map(Vec::len) {
                    let mut order: Vec<u8> = (0..n as u8).collect();
                    order.swap(0, usize::from(self.slot));
                    ctx.events.push(GameEvent::PartyReordered { order });
                    self.reordered = true;
                }
                return Decision::Wait("the new order".into());
            }
            return self.retries.wait(o, "the party list after the swap");
        }
        if let Some(party) = &o.party_menu {
            if !party.actions && self.switching {
                // "Move to where?": the ▶ to the first slot, then A.
                self.retries.enter("switching");
                let Some(at) = party.selected else {
                    return self.retries.wait(o, "reading the switch cursor");
                };
                if at == 0 {
                    return self.retries.act(
                        "swap into the first slot",
                        Button::A,
                        Expectation::InputsDone,
                        SCREEN_FRAMES,
                    );
                }
                return self.retries.act(
                    "switch: Up toward the first slot",
                    Button::Up,
                    Expectation::PartySelected(at - 1),
                    CURSOR_FRAMES,
                );
            }
            if party.actions {
                self.retries.enter("actions");
                if !self.opened && party.selected != Some(self.slot) {
                    return self.retries.act(
                        "close another member's actions",
                        Button::B,
                        Expectation::PartyList,
                        SCREEN_FRAMES,
                    );
                }
                let Some(row) = party.options.iter().position(|l| fits("SWITCH", l)) else {
                    return self.retries.wait(o, "reading the member's actions");
                };
                let Some(at) = party.option_cursor else {
                    return self.retries.wait(o, "reading the action window's ▶");
                };
                let row = row as u8;
                if at == row {
                    return self.retries.act(
                        "choose SWITCH",
                        Button::A,
                        Expectation::PartyList,
                        SCREEN_FRAMES,
                    );
                }
                let (button, next) = if at < row {
                    (Button::Down, at + 1)
                } else {
                    (Button::Up, at - 1)
                };
                return self.retries.act(
                    format!("actions: {button:?} toward SWITCH"),
                    button,
                    Expectation::PartyOptionAt(next),
                    CURSOR_FRAMES,
                );
            }
            self.retries.enter("party");
            self.opened = false;
            if self.slot >= party.count {
                return Decision::Fail(format!(
                    "slot {} is not in the party ({} members)",
                    self.slot, party.count
                ));
            }
            let Some(at) = party.selected else {
                return self.retries.wait(o, "reading the selected member");
            };
            if at == self.slot {
                if self.opens >= MAX_OPENS {
                    return Decision::Fail(format!(
                        "leading with slot {}: its actions opened {MAX_OPENS} times, SWITCH never chosen",
                        self.slot
                    ));
                }
                return self.retries.act(
                    "open the member's actions",
                    Button::A,
                    Expectation::PartyActions,
                    SCREEN_FRAMES,
                );
            }
            let (button, next) = if at < self.slot {
                (Button::Down, at + 1)
            } else {
                (Button::Up, at - 1)
            };
            return self.retries.act(
                format!("party: {button:?} toward slot {}", self.slot),
                button,
                Expectation::PartySelected(next),
                CURSOR_FRAMES,
            );
        }
        if let Some(menu) = &o.menu {
            if o.dialogue.is_none() {
                self.retries.enter("start menu");
                return pick_row(
                    &mut self.retries,
                    o,
                    menu,
                    &MenuRow::Text("POKéMON"),
                    ctx.state,
                    Expectation::PartyList,
                    SCREEN_FRAMES,
                )
                .0;
            }
        }
        if o.dialogue.is_some() {
            return crate::new_game::advance_or_wait(o.dialogue.as_ref(), "reading");
        }
        self.retries.enter("open");
        open_start_menu(&mut self.retries, o, ctx.quiet_frames)
    }
}

/// The party's fighters (not fainted) as combatants, by slot.
fn fighters(data: &GameData, party: &Party) -> Vec<(u8, pokebot_planner::Combatant)> {
    party
        .members
        .iter()
        .filter(|m| m.hp.is_none_or(|(hp, _)| hp > 0))
        .filter_map(|m| {
            let moves: Vec<String> = m.moves.iter().filter(|mv| *mv != "?").cloned().collect();
            let moves = if moves.is_empty() {
                data.default_moves(&m.species, m.level)
            } else {
                moves
            };
            let c = pokebot_planner::Combatant::new(
                data,
                &m.species,
                m.level,
                moves,
                pokebot_planner::prepare::OUR_IV,
            )?;
            Some((m.slot, c))
        })
        .collect()
}

/// IVs a wild Pokémon is taken to have (the middle of the range).
const WILD_IV: u32 = 15;

/// The party slot that should lead on `map`'s land, where wild Pokémon
/// meet whoever leads: the member with the best chance over its encounters
/// (each slot at its highest level, weighed by its chance), when it isn't
/// the lead and beats the lead's by [`LEAD_MARGIN`]; with both chances.
pub fn fighter_for_wilds(data: &GameData, party: &Party, map: &str) -> Option<(u8, f64, f64)> {
    let table = data.wild.get(map)?.get("land")?;
    let foes: Vec<(f64, pokebot_planner::Combatant)> = table
        .slots
        .iter()
        .filter_map(|e| {
            let moves = data.default_moves(&e.species, e.max_level);
            let c = pokebot_planner::Combatant::new(data, &e.species, e.max_level, moves, WILD_IV)?;
            Some((f64::from(e.chance), c))
        })
        .collect();
    let weight: f64 = foes.iter().map(|(w, _)| w).sum();
    if weight <= 0.0 {
        return None;
    }
    let chance = |us: &pokebot_planner::Combatant| {
        foes.iter()
            .map(|(w, them)| w * pokebot_planner::matchup(data, us, them).p_win)
            .sum::<f64>()
            / weight
    };
    let odds: Vec<(u8, f64)> = fighters(data, party)
        .iter()
        .map(|(slot, c)| (*slot, chance(c)))
        .collect();
    let p_lead = odds.iter().find(|(slot, _)| *slot == 0)?.1;
    let &(slot, p_best) = odds
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))?;
    (slot != 0 && p_best > p_lead + LEAD_MARGIN).then_some((slot, p_best, p_lead))
}

/// Before a long stay on `map` (a boulder puzzle), the member fittest
/// against its wild Pokémon leads (fleet continue-5, Victory Road 1F: a
/// GRIMER Lv30 led, CHARIZARD Lv68 last; every wild ONIX fainted it, the
/// party was flown to Viridian to heal, and the boulders reset each time).
pub fn lead_for_wilds(ctx: &mut ToolContext<'_>, map: &str) -> Result<(), ToolError> {
    let party = Party::from_state(ctx.state());
    let Some((slot, p_best, p_lead)) = fighter_for_wilds(&ctx.data, &party, map) else {
        return Ok(());
    };
    let species = party
        .members
        .iter()
        .find(|m| m.slot == slot)
        .map(|m| m.species.clone())
        .unwrap_or_default();
    ctx.emit(progress(
        "Party",
        format!(
            "{} leads on {map} ({:.0} % against its wild Pokémon, the lead {:.0} %)",
            crate::party::display_name(&species),
            p_best * 100.0,
            p_lead * 100.0
        ),
    ))?;
    ctx.drive(&mut LeadWith::new(slot, &species))?;
    Ok(())
}

/// The party slot that should lead against `trainer`: the member the team
/// sends against its first Pokémon, when it isn't the lead and its chance
/// against that one beats the lead's by [`LEAD_MARGIN`]; with that chance
/// and the lead's.
pub fn fighter_for(data: &GameData, party: &Party, trainer: &str) -> Option<(u8, f64, f64)> {
    let fighters = fighters(data, party);
    let lead = fighters.iter().find(|(slot, _)| *slot == 0)?;
    let first = data.trainers.get(trainer)?.party.first()?;
    let moves = first
        .moves
        .clone()
        .unwrap_or_else(|| data.default_moves(&first.species, first.level));
    let iv = u32::from(first.iv) * 31 / 255;
    let foe = pokebot_planner::Combatant::new(data, &first.species, first.level, moves, iv)?;
    let combatants: Vec<_> = fighters.iter().map(|(_, c)| c.clone()).collect();
    let best = pokebot_planner::team_vs_trainer(data, &combatants, trainer)?.lead;
    let (slot, fighter) = &fighters[best];
    let p_best = pokebot_planner::matchup(data, fighter, &foe).p_win;
    let p_lead = pokebot_planner::matchup(data, &lead.1, &foe).p_win;
    (*slot != 0 && p_best > p_lead + LEAD_MARGIN).then_some((*slot, p_best, p_lead))
}

/// Before a battle with `trainer`: the member best able to beat it leads.
pub fn lead_for(ctx: &mut ToolContext<'_>, trainer: &str) -> Result<(), ToolError> {
    let party = Party::from_state(ctx.state());
    let Some((slot, p_best, p_lead)) = fighter_for(&ctx.data, &party, trainer) else {
        return Ok(());
    };
    let species = party
        .members
        .iter()
        .find(|m| m.slot == slot)
        .map(|m| m.species.clone())
        .unwrap_or_default();
    ctx.emit(progress(
        "Party",
        format!(
            "{} leads against {trainer} ({:.0} % against its first, the lead {:.0} %)",
            crate::party::display_name(&species),
            p_best * 100.0,
            p_lead * 100.0
        ),
    ))?;
    ctx.drive(&mut LeadWith::new(slot, &species))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::party::Member;

    fn data() -> Option<GameData> {
        GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok()
    }

    /// Fleet workers: CHARMANDER led against Brock and fainted to ONIX
    /// while the MANKEY trained for him waited. The team meets each of
    /// Brock's Pokémon with its best match: MANKEY is sent against ONIX,
    /// and leads when the lead can't face GEODUDE (a PIDGEY).
    #[test]
    fn the_member_for_the_trainers_first_pokemon_leads() {
        let Some(d) = data() else { return };
        let combatant = |m: &Member| {
            pokebot_planner::Combatant::new(
                &d,
                &m.species,
                m.level,
                d.default_moves(&m.species, m.level),
                pokebot_planner::prepare::OUR_IV,
            )
            .unwrap()
        };
        let pidgey = Member::new(&d, "SPECIES_PIDGEY", 10);
        let charmander = Member::new(&d, "SPECIES_CHARMANDER", 14);
        let mankey = Member {
            slot: 1,
            ..Member::new(&d, "SPECIES_MANKEY", 14)
        };
        let team = pokebot_planner::team_vs_trainer(
            &d,
            &[combatant(&charmander), combatant(&mankey)],
            "TRAINER_LEADER_BROCK",
        )
        .unwrap();
        let onix = team
            .opponents
            .iter()
            .find(|o| o.0 == "SPECIES_ONIX")
            .unwrap();
        assert_eq!(onix.3, 1, "{team:?}");
        let party = Party {
            members: vec![pidgey.clone(), mankey],
        };
        let (slot, p_best, p_lead) = fighter_for(&d, &party, "TRAINER_LEADER_BROCK").unwrap();
        assert_eq!(slot, 1);
        assert!(p_best > p_lead);
        // A lone lead stays.
        let alone = Party {
            members: vec![pidgey],
        };
        assert_eq!(fighter_for(&d, &alone, "TRAINER_LEADER_BROCK"), None);
    }

    /// Fleet continue-5, Victory Road 1F: GRIMER Lv30 led, CHARIZARD Lv68
    /// last; wild ONIX fainted the lead again and again mid-puzzle. The
    /// member fittest against the floor's wild Pokémon leads; a lead fit
    /// already stays, and a map without wild Pokémon changes nothing.
    #[test]
    fn the_member_fittest_against_the_wilds_leads() {
        let Some(d) = data() else { return };
        let member = |slot: u8, species: &str, level: u8| Member {
            slot,
            ..Member::new(&d, species, level)
        };
        let party = Party {
            members: vec![
                member(0, "SPECIES_GRIMER", 30),
                member(1, "SPECIES_PIDGEY", 14),
                member(5, "SPECIES_CHARIZARD", 68),
            ],
        };
        let (slot, p_best, p_lead) = fighter_for_wilds(&d, &party, "VictoryRoad_1F").unwrap();
        assert_eq!(slot, 5);
        assert!(p_best > p_lead + LEAD_MARGIN, "{p_best} {p_lead}");
        let led = Party {
            members: vec![
                member(0, "SPECIES_CHARIZARD", 68),
                member(1, "SPECIES_GRIMER", 30),
            ],
        };
        assert_eq!(fighter_for_wilds(&d, &led, "VictoryRoad_1F"), None);
        assert_eq!(
            fighter_for_wilds(&d, &party, "PalletTown_PlayersHouse_1F"),
            None
        );
    }

    /// Fleet continue-4: HYPNO in slot 5, its actions opened; the window
    /// covers the last panels and the highlight read as another member's,
    /// so the actions were closed and opened again, for 18 minutes. The
    /// window the step opened is taken for the member's: Down to SWITCH.
    #[test]
    fn the_actions_opened_on_the_last_slot_are_its_own() {
        use pokebot_state::{Observation, Observed, PartyMenuObservation, ScreenState};
        let state = GameState::default();
        let mut events = Vec::new();
        let menu = |actions: bool, selected: u8| {
            let mut o = Observation::bare(
                1,
                Observed {
                    value: ScreenState::PartyMenu,
                    detector: "test".into(),
                },
                Default::default(),
            );
            o.party_menu = Some(PartyMenuObservation {
                count: 6,
                selected: Some(selected),
                actions,
                options: if actions {
                    ["SUMMARY", "SWITCH", "ITEM", "CANCEL"]
                        .map(String::from)
                        .to_vec()
                } else {
                    Vec::new()
                },
                option_cursor: actions.then_some(0),
                ..Default::default()
            });
            o
        };
        let mut step = LeadWith::new(5, "SPECIES_HYPNO");
        let mut ctx = |o: &Observation, events: &mut Vec<GameEvent>| -> Option<String> {
            let mut c = StepContext {
                observation: o,
                state: &state,
                events,
                quiet_frames: 100,
                frame: None,
                learned: &[],
            };
            let d = step.next(&mut c);
            let Decision::Act(a) = d else { return None };
            step.on_outcome(&a, Outcome::Confirmed, &mut c);
            Some(a.label)
        };
        let list = menu(false, 5);
        assert_eq!(
            ctx(&list, &mut events).as_deref(),
            Some("open the member's actions")
        );
        // The window over slot 5: its highlight reads as slot 3.
        let actions = menu(true, 3);
        assert_eq!(
            ctx(&actions, &mut events).as_deref(),
            Some("actions: Down toward SWITCH")
        );
    }
}
