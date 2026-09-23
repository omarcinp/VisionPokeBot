//! Battle control: menu navigation verified cursor by cursor, and move
//! choice from the battle evaluator (best move against the identified
//! opponent, keeping PP of strong moves for trainer battles).

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_planner::evaluate::best_move;
use pokebot_planner::Combatant;
use pokebot_state::{BattleMenu, Observation, ScreenState};

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

/// The move slot to use: the evaluator's best move among those with PP to
/// spare; slot 0 if nothing is known.
pub fn choose_move(
    data: &GameData,
    party: &Party,
    opponent: Option<&Combatant>,
    memory: &BattleMemory,
    policy: &BattlePolicy,
) -> (u8, Option<String>) {
    let Some(lead) = party.lead() else {
        return (0, None);
    };
    let usable: Vec<String> = lead
        .moves
        .iter()
        .filter(|m| {
            let left = lead.pp_left(data, m);
            let max = data.move_(m).map_or(0, |mv| mv.pp);
            left > 0 && (memory.trainer || max >= 30 || left > policy.wild_pp_reserve)
        })
        .cloned()
        .collect();
    let chosen = opponent.and_then(|foe| {
        let us = Combatant::new(data, &lead.species, lead.level, usable.clone(), 10)?;
        best_move(data, &us, foe).map(|(m, _)| m)
    });
    let chosen = chosen.or_else(|| {
        // Unknown opponent: strongest usable damaging move by power.
        usable
            .iter()
            .filter(|m| data.move_(m).is_some_and(|mv| mv.power > 0))
            .max_by_key(|m| data.move_(m).map_or(0, |mv| mv.power))
            .cloned()
    });
    match chosen {
        Some(m) => (
            lead.moves.iter().position(|x| *x == m).unwrap_or(0) as u8,
            Some(m),
        ),
        None => (0, lead.moves.first().cloned()),
    }
}

/// The next battle input, if a battle menu is open.
pub fn decide(
    observation: &Observation,
    policy: &BattlePolicy,
    memory: &mut BattleMemory,
    party: &Party,
    data: &GameData,
) -> Option<Decision> {
    let battle = observation.battle.as_ref()?;
    let menu = battle.menu?;
    let low = battle.player_hp.is_some_and(|hp| hp < policy.flee_below);
    let flee = low && !memory.trainer && memory.run_attempts < policy.max_run_attempts;
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
        BattleMenu::Moves { column, row } => {
            let opponent = identify_opponent(data, observation);
            let (slot, name) = choose_move(data, party, opponent.as_ref(), memory, policy);
            memory.last_move = name.clone();
            let label = match &name {
                Some(n) => format!("move {} ({})", slot + 1, n.trim_start_matches("MOVE_")),
                None => format!("move {}", slot + 1),
            };
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

fn step_toward(
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
