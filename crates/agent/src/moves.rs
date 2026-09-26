//! Which move to forget when a Pokémon with four moves learns a new one.
//!
//! Candidate movesets (keep the current four, or replace one slot with the
//! new move) are ranked by, in order:
//! 1. the evaluator's total win probability against the upcoming trainers;
//! 2. raw damage value (power × accuracy, STAB counted): the evaluator
//!    ignores PP, so a damaging move is never traded for a status move
//!    unless that wins battles;
//! 3. status utility (the evaluator only models damage): sleep > paralysis >
//!    poison / Leech Seed > stat drops;
//! 4. keeping the current moves, then forgetting the earliest slot.

use pokebot_gamedata::GameData;
use pokebot_planner::evaluate::battle_vs_trainer;
use pokebot_planner::Combatant;

use crate::party::Member;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnChoice {
    /// Forget the move in this slot (menu order) for the new one.
    Forget(usize),
    /// Don't learn the new move.
    Skip,
}

/// Decides what `member` (four moves known) does about `new_move`.
pub fn choose(data: &GameData, member: &Member, new_move: &str, targets: &[String]) -> LearnChoice {
    let mut best = (
        LearnChoice::Skip,
        value(data, member, &member.moves, targets),
    );
    for slot in 0..member.moves.len() {
        let mut moves = member.moves.clone();
        moves[slot] = new_move.to_owned();
        let v = value(data, member, &moves, targets);
        if v > best.1 {
            best = (LearnChoice::Forget(slot), v);
        }
    }
    best.0
}

/// (win probability total in millionths, damage value, status utility).
fn value(
    data: &GameData,
    member: &Member,
    moves: &[String],
    targets: &[String],
) -> (i64, u32, u32) {
    let p_win: f64 = Combatant::new(data, &member.species, member.level, moves.to_vec(), 10)
        .map_or(0.0, |us| {
            targets
                .iter()
                .filter_map(|t| battle_vs_trainer(data, std::slice::from_ref(&us), t))
                .map(|e| e.p_win)
                .sum()
        });
    let types = data
        .species(&member.species)
        .map(|s| s.types.clone())
        .unwrap_or_default();
    let (mut utility, mut damage) = (0, 0);
    for mv in moves.iter().filter_map(|m| data.move_(m)) {
        utility += match mv.effect.as_deref().unwrap_or("") {
            "EFFECT_SLEEP" => 4,
            "EFFECT_PARALYZE" => 3,
            "EFFECT_POISON" | "EFFECT_TOXIC" | "EFFECT_LEECH_SEED" => 2,
            e if e.ends_with("_DOWN") || e.ends_with("_DOWN_2") => 1,
            _ => 0,
        };
        let stab = mv.kind.as_ref().is_some_and(|k| types.contains(k));
        let d = u32::from(mv.power) * u32::from(mv.accuracy);
        damage += if stab { d * 3 / 2 } else { d };
    }
    ((p_win * 1e6).round() as i64, damage, utility)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    fn bulbasaur(moves: &[&str]) -> Member {
        Member {
            slot: 0,
            species: "SPECIES_BULBASAUR".into(),
            level: 15,
            moves: moves.iter().map(|m| (*m).to_owned()).collect(),
            hp: None,
            pp_used: Default::default(),
            read_stats: None,
            ability: None,
        }
    }

    #[test]
    fn status_moves_replace_stat_drops_and_damage_is_kept() {
        let Some(data) = data() else { return };
        let targets = vec![
            "TRAINER_BUG_CATCHER_GREG".to_owned(),
            "TRAINER_LASS_JANICE".to_owned(),
        ];
        let member = bulbasaur(&[
            "MOVE_TACKLE",
            "MOVE_GROWL",
            "MOVE_LEECH_SEED",
            "MOVE_VINE_WHIP",
        ]);
        assert_eq!(
            choose(&data, &member, "MOVE_POISON_POWDER", &targets),
            LearnChoice::Forget(1)
        );
        let member = bulbasaur(&[
            "MOVE_TACKLE",
            "MOVE_POISON_POWDER",
            "MOVE_LEECH_SEED",
            "MOVE_VINE_WHIP",
        ]);
        assert_eq!(
            choose(&data, &member, "MOVE_SLEEP_POWDER", &targets),
            LearnChoice::Forget(1)
        );
    }

    #[test]
    fn a_worse_move_is_not_learned() {
        let Some(data) = data() else { return };
        let member = bulbasaur(&[
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_LEECH_SEED",
            "MOVE_VINE_WHIP",
        ]);
        assert_eq!(choose(&data, &member, "MOVE_GROWL", &[]), LearnChoice::Skip);
    }
}
