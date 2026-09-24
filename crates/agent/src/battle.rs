//! Battle control: menu navigation verified cursor by cursor, and move
//! choice from the battle evaluator (best move against the identified
//! opponent, keeping PP of strong moves for trainer battles).

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_planner::evaluate::best_move;
use pokebot_planner::Combatant;
use pokebot_state::{BattleMenu, GameEvent, Observation, ScreenState};

use crate::catch::{self, CatchMemory};
use crate::party::{display_name, Party};
use crate::{Action, Decision, Expectation};

#[derive(Debug, Clone, Copy)]
pub struct BattlePolicy {
    /// Try to RUN from wild battles when our HP bar is below this (per mille).
    pub flee_below: u16,
    /// RUN attempts per battle before fighting instead.
    pub max_run_attempts: u32,
    /// In wild battles, moves with at most this many PP left are saved for
    /// trainers (moves with 30+ PP are always usable).
    pub wild_pp_reserve: u8,
}

impl Default for BattlePolicy {
    fn default() -> Self {
        Self {
            flee_below: 350,
            max_run_attempts: 3,
            wild_pp_reserve: 5,
        }
    }
}

/// Per-battle memory.
#[derive(Debug, Default, Clone)]
pub struct BattleMemory {
    pub run_attempts: u32,
    /// A trainer battle (can't flee; spend PP freely).
    pub trainer: bool,
    /// Last move chosen (for PP accounting).
    pub last_move: Option<String>,
    /// (party slot, move slot) of the last move chosen.
    pub last_slot: Option<(u8, u8)>,
    /// Identifying the wild opponent and catching it.
    pub catch: CatchMemory,
    /// Our lead's move under the foe's DISABLE, read from battle text; never
    /// chosen until "… is disabled no more!".
    pub disabled: Option<String>,
}

/// DISABLE in battle text about our lead (`lead`, its printed name):
/// `Some(Some(move))` for "IVYSAUR's VINE WHIP was disabled!" (the foe used
/// DISABLE) and "… is disabled!" (the move was chosen anyway),
/// `Some(None)` for "IVYSAUR is disabled no more!". Pages about the foe
/// ("Foe …", "Wild …") never match. The font's apostrophe reads as `’`.
pub fn disable_text(page: &str, lead: &str, data: &GameData) -> Option<Option<String>> {
    let page = page.replace('’', "'");
    if page == format!("{lead} is disabled no more!") {
        return Some(None);
    }
    let rest = page.strip_prefix(&format!("{lead}'s "))?;
    let name = rest
        .strip_suffix(" was disabled!")
        .or_else(|| rest.strip_suffix(" is disabled!"))?;
    data.move_named(name).map(|m| Some(m.to_owned()))
}

/// Battle text read on two frames ([`crate::catch::observe`]): tracks
/// DISABLE on our lead.
pub fn observe_page(memory: &mut BattleMemory, page: &str, party: &Party, data: &GameData) {
    let Some(lead) = party.lead() else { return };
    if let Some(disabled) = disable_text(page, &lead.display_name(), data) {
        memory.disabled = disabled;
    }
}

/// The opponent as a combatant, if its name and level can be read.
pub fn identify_opponent(data: &GameData, observation: &Observation) -> Option<Combatant> {
    let battle = observation.battle.as_ref()?;
    let read = battle.opponent_name.as_deref()?;
    let level = battle.opponent_level?;
    let names: Vec<(String, &String)> = data.species.keys().map(|k| (display_name(k), k)).collect();
    let species = pokebot_vision::detect::hud::resolve(read, names.iter().map(|(n, _)| n.as_str()))
        .and_then(|n| names.iter().find(|(d, _)| d == n))
        .map(|(_, k)| (*k).clone())?;
    Combatant::new(
        data,
        &species,
        level,
        data.default_moves(&species, level),
        15,
    )
}

/// The move slot to use: the evaluator's best damaging move among those
/// with PP to spare; in wild battles, the trainer reserve is spent only when
/// nothing else is left. A disabled move is never chosen (the game refuses
/// it and returns to the move menu). `None` when no damaging move has PP.
pub fn choose_move(
    data: &GameData,
    party: &Party,
    opponent: Option<&Combatant>,
    memory: &BattleMemory,
    policy: &BattlePolicy,
) -> Option<(u8, String)> {
    let lead = party.lead()?;
    let damaging = |m: &String| {
        data.move_(m).is_some_and(|mv| mv.power > 0) && memory.disabled.as_ref() != Some(m)
    };
    let spare = |m: &String| {
        let left = lead.pp_left(data, m);
        let max = data.move_(m).map_or(0, |mv| mv.pp);
        left > 0 && (memory.trainer || max >= 30 || left > policy.wild_pp_reserve)
    };
    let with_pp = |m: &String| lead.pp_left(data, m) > 0;
    let pick = |usable: Vec<String>| -> Option<String> {
        let best = opponent.and_then(|foe| {
            let us = Combatant::new(data, &lead.species, lead.level, usable.clone(), 10)?;
            best_move(data, &us, foe).map(|(m, _)| m)
        });
        // Unknown opponent: strongest usable move by power.
        best.or_else(|| {
            usable
                .iter()
                .max_by_key(|m| data.move_(m).map_or(0, |mv| mv.power))
                .cloned()
        })
    };
    let tier = |keep: &dyn Fn(&String) -> bool| -> Vec<String> {
        lead.moves
            .iter()
            .filter(|m| damaging(m) && keep(m))
            .cloned()
            .collect()
    };
    let chosen = pick(tier(&spare)).or_else(|| pick(tier(&with_pp)))?;
    let slot = lead.moves.iter().position(|x| *x == chosen)? as u8;
    Some((slot, chosen))
}

/// Any move the game will accept when no damaging move can be chosen (the
/// only one is disabled or out of PP): the first non-disabled move with PP,
/// status moves included, in slot order. `None` when no move has PP (the
/// game then uses STRUGGLE by itself).
pub fn fallback_move(
    data: &GameData,
    party: &Party,
    memory: &BattleMemory,
) -> Option<(u8, String)> {
    let lead = party.lead()?;
    lead.moves
        .iter()
        .enumerate()
        .find(|(_, m)| lead.pp_left(data, m) > 0 && memory.disabled.as_ref() != Some(*m))
        .map(|(slot, m)| (slot as u8, m.clone()))
}

/// The next battle input, if a battle menu is open.
pub fn decide(
    observation: &Observation,
    policy: &BattlePolicy,
    memory: &mut BattleMemory,
    party: &Party,
    data: &GameData,
    events: &mut Vec<GameEvent>,
) -> Option<Decision> {
    let battle = observation.battle.as_ref()?;
    let menu = battle.menu?;
    // A catch attempt drives the menus (its risk check replaces fleeing);
    // `None` means it was abandoned and the battle goes on as usual.
    if memory.catch.attempt.is_some() {
        if let Some(decision) =
            catch::attempt_decision(observation, policy, memory, party, data, events)
        {
            return Some(decision);
        }
    }
    // Wild battles: the opponent is identified (and the catch decided) on
    // the command menu before the first choice.
    if !memory.trainer && !memory.catch.decided && matches!(menu, BattleMenu::Command { .. }) {
        return Some(Decision::Wait("identifying the wild opponent".into()));
    }
    let low = battle.player_hp.is_some_and(|hp| hp < policy.flee_below);
    let no_attacks = choose_move(data, party, None, memory, policy).is_none();
    let flee = (low || no_attacks || memory.catch.flee)
        && !memory.trainer
        && memory.run_attempts < policy.max_run_attempts;
    Some(match menu {
        BattleMenu::Command { column, row } if flee => step_toward(
            (column, row),
            (1, 1),
            |c, r| BattleMenu::Command { column: c, row: r },
            "RUN",
            Expectation::ScreenIsNot(ScreenState::BattleCommand),
        ),
        BattleMenu::Command { column, row } => step_toward(
            (column, row),
            (0, 0),
            |c, r| BattleMenu::Command { column: c, row: r },
            "FIGHT",
            Expectation::ScreenIs(ScreenState::BattleMoveSelection),
        ),
        BattleMenu::Moves { .. } if flee => Decision::Act(Action::new(
            "back to the command menu to RUN",
            vec![ControllerCommand::Press(Button::B)],
            Expectation::ScreenIs(ScreenState::BattleCommand),
            45,
        )),
        BattleMenu::Moves { column, row } => {
            let opponent = identify_opponent(data, observation);
            let Some((slot, name)) = choose_move(data, party, opponent.as_ref(), memory, policy)
                .or_else(|| fallback_move(data, party, memory))
            else {
                return Some(Decision::Fail("no move has PP left".into()));
            };
            memory.last_move = Some(name.clone());
            memory.last_slot = party.lead().map(|lead| (lead.slot, slot));
            let label = format!("move {} ({})", slot + 1, name.trim_start_matches("MOVE_"));
            step_toward(
                (column, row),
                (slot % 2, slot / 2),
                |c, r| BattleMenu::Moves { column: c, row: r },
                &label,
                Expectation::ScreenIsNot(ScreenState::BattleMoveSelection),
            )
        }
    })
}

pub(crate) fn step_toward(
    at: (u8, u8),
    target: (u8, u8),
    cell: impl Fn(u8, u8) -> BattleMenu,
    name: &str,
    confirmed: Expectation,
) -> Decision {
    if at == target {
        return Decision::Act(Action::new(
            format!("choose {name}"),
            vec![ControllerCommand::Press(Button::A)],
            confirmed,
            90,
        ));
    }
    let (button, next) = if at.1 != target.1 {
        (
            if target.1 > at.1 {
                Button::Down
            } else {
                Button::Up
            },
            (at.0, target.1),
        )
    } else {
        (
            if target.0 > at.0 {
                Button::Right
            } else {
                Button::Left
            },
            (target.0, at.1),
        )
    };
    Decision::Act(Action::new(
        format!("cursor to {name}: {button:?}"),
        vec![ControllerCommand::Press(button)],
        Expectation::BattleMenuAt(cell(next.0, next.1)),
        45,
    ))
}

/// "Will RED change POKéMON?": asked before a trainer sends the next
/// Pokémon, when the party has more than one.
pub fn is_switch_question(page: &str) -> bool {
    page.starts_with("Will ") && page.contains(" change") && page.contains("POK")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::party::Member;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    fn ivysaur(data: &GameData, used: &[(&str, u8)]) -> Party {
        let mut member = Member::new(data, "SPECIES_IVYSAUR", 16);
        member.moves = [
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_LEECH_SEED",
            "MOVE_VINE_WHIP",
        ]
        .map(String::from)
        .to_vec();
        for (m, n) in used {
            member.pp_used.insert((*m).to_owned(), *n);
        }
        Party {
            members: vec![member],
        }
    }

    #[test]
    fn wild_battles_spend_the_reserve_before_running_dry() {
        let Some(data) = data() else { return };
        let policy = BattlePolicy::default();
        let wild = BattleMemory::default();
        // Tackle empty, Vine Whip at the reserve: Vine Whip rather than nothing.
        let party = ivysaur(&data, &[("MOVE_TACKLE", 35), ("MOVE_VINE_WHIP", 5)]);
        assert_eq!(
            choose_move(&data, &party, None, &wild, &policy),
            Some((3, "MOVE_VINE_WHIP".into()))
        );
        // No damaging PP at all: nothing to choose (the battle policy runs).
        let party = ivysaur(&data, &[("MOVE_TACKLE", 35), ("MOVE_VINE_WHIP", 10)]);
        assert_eq!(choose_move(&data, &party, None, &wild, &policy), None);
    }

    /// Review: in a trainer battle with the only damaging move disabled,
    /// the move menu failed the story. Any accepted move is chosen instead;
    /// only a lead with no PP at all fails.
    #[test]
    fn a_trainer_battle_with_the_only_attack_disabled_uses_another_move() {
        let Some(data) = data() else { return };
        let policy = BattlePolicy::default();
        // Tackle out of PP, Vine Whip disabled: Sleep Powder (slot 2).
        let party = ivysaur(&data, &[("MOVE_TACKLE", 35)]);
        let mut memory = BattleMemory {
            trainer: true,
            disabled: Some("MOVE_VINE_WHIP".into()),
            ..BattleMemory::default()
        };
        assert_eq!(choose_move(&data, &party, None, &memory, &policy), None);
        assert_eq!(
            fallback_move(&data, &party, &memory),
            Some((1, "MOVE_SLEEP_POWDER".into()))
        );
        let mut o = pokebot_state::Observation::bare(
            1,
            pokebot_state::Observed {
                value: ScreenState::BattleMoveSelection,
                detector: "test".into(),
            },
            Default::default(),
        );
        o.battle = Some(pokebot_state::BattleObservation {
            menu: Some(BattleMenu::Moves { column: 1, row: 0 }),
            player_name: Some("IVYSAUR".into()),
            player_level: Some(16),
            player_hp_numbers: Some((50, 50)),
            opponent_name: None,
            opponent_level: None,
            player_hp: Some(1000),
            opponent_hp: Some(1000),
            move_pp: None,
            move_names: Vec::new(),
            opponent_caught: None,
            opponent_shiny: None,
        });
        let mut events = Vec::new();
        match decide(&o, &policy, &mut memory, &party, &data, &mut events) {
            Some(Decision::Act(a)) => assert_eq!(a.label, "choose move 2 (SLEEP_POWDER)"),
            _ => panic!("unexpected decision"),
        }
        // No PP anywhere (the disabled move aside): fail.
        let party = ivysaur(
            &data,
            &[
                ("MOVE_TACKLE", 35),
                ("MOVE_SLEEP_POWDER", 15),
                ("MOVE_LEECH_SEED", 10),
            ],
        );
        assert_eq!(fallback_move(&data, &party, &memory), None);
        match decide(&o, &policy, &mut memory, &party, &data, &mut events) {
            Some(Decision::Fail(r)) => assert_eq!(r, "no move has PP left"),
            _ => panic!("unexpected decision"),
        }
    }

    /// Live (Route 3, Lass Robin's JIGGLYPUFF): DISABLE on VINE WHIP, and the
    /// bot chose VINE WHIP again every turn: "IVYSAUR's VINE WHIP is
    /// disabled!" sent it back to the move menu forever.
    #[test]
    fn a_disabled_move_is_not_chosen_until_disable_ends() {
        let Some(data) = data() else { return };
        let policy = BattlePolicy::default();
        let party = ivysaur(&data, &[("MOVE_VINE_WHIP", 3)]);
        let mut memory = BattleMemory {
            trainer: true,
            ..BattleMemory::default()
        };
        assert_eq!(
            choose_move(&data, &party, None, &memory, &policy),
            Some((3, "MOVE_VINE_WHIP".into()))
        );
        // The foe's own moves and unrelated pages change nothing.
        for page in [
            "Foe JIGGLYPUFF's POUND was disabled!",
            "Foe JIGGLYPUFF used DISABLE!",
        ] {
            observe_page(&mut memory, page, &party, &data);
            assert_eq!(memory.disabled, None, "{page}");
        }
        observe_page(
            &mut memory,
            "IVYSAUR's VINE WHIP was disabled!",
            &party,
            &data,
        );
        assert_eq!(memory.disabled.as_deref(), Some("MOVE_VINE_WHIP"));
        assert_eq!(
            choose_move(&data, &party, None, &memory, &policy),
            Some((0, "MOVE_TACKLE".into()))
        );
        // Chosen anyway (e.g. the first page was missed): the refusal page
        // also marks it.
        memory.disabled = None;
        observe_page(
            &mut memory,
            // As read live: the font's apostrophe is `’`.
            "IVYSAUR’s VINE WHIP is disabled!",
            &party,
            &data,
        );
        assert_eq!(memory.disabled.as_deref(), Some("MOVE_VINE_WHIP"));
        observe_page(&mut memory, "IVYSAUR is disabled no more!", &party, &data);
        assert_eq!(memory.disabled, None);
        assert_eq!(
            choose_move(&data, &party, None, &memory, &policy),
            Some((3, "MOVE_VINE_WHIP".into()))
        );
    }
}
