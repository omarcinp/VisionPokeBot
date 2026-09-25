//! `Beat`: fight a trainer, healing first when the lead isn't ready
//! (flash-7 and flash-9: IVYSAUR met Miguel at 29/62 and then 19/62 HP
//! with no attacking PP after twelve battles since Pewter, and fainted;
//! the plan had no Heal because the lead was full when it was made).
//! The readiness rule is the story's: HP under [`LEAD_HP_MIN`], no
//! damaging move with PP, or P(win) alone below the confidence while a
//! full heal would reach it.

use pokebot_gamedata::GameData;
use pokebot_planner::intents::LEAD_HP_MIN;

use super::{progress, Intent, Tool, ToolContext, ToolOutcome};
use crate::battle::{choose_move, BattleMemory, BattlePolicy};
use crate::party::Party;
use crate::story::lead_p_win;

/// The story runner's confidence for a planned battle.
const BATTLE_CONFIDENCE: f64 = 0.9;

pub struct BeatTool;

/// Why the lead is too worn to walk on (through encounters and trainers'
/// sight), if it is: HP under [`LEAD_HP_MIN`] or no attacking move with
/// PP left (flash-11: IVYSAUR walked into Lass Iris's sight at 25/60 with
/// only Sleep Powder and fainted).
pub fn worn(data: &GameData, party: &Party) -> Option<String> {
    let lead = party.lead()?;
    let name = lead.display_name();
    if let Some((hp, max)) = lead.hp {
        if u32::from(hp) * 100 < u32::from(max) * u32::from(LEAD_HP_MIN) {
            return Some(format!("{name} at {hp}/{max} HP"));
        }
    }
    let memory = BattleMemory {
        trainer: true,
        ..BattleMemory::default()
    };
    if choose_move(data, party, None, &memory, &BattlePolicy::default()).is_none() {
        return Some(format!("{name} has no attacking move with PP left"));
    }
    None
}

/// Why the lead should heal before fighting `trainer`, if it should.
pub fn unready(data: &GameData, party: &Party, trainer: &str) -> Option<String> {
    if let Some(why) = worn(data, party) {
        return Some(why);
    }
    let lead = party.lead()?;
    let p_now = lead_p_win(data, lead, trainer, false);
    if p_now < BATTLE_CONFIDENCE {
        let p_full = lead_p_win(data, lead, trainer, true);
        if p_full >= BATTLE_CONFIDENCE {
            return Some(format!(
                "P(win) {p_now:.3} vs {trainer} now, {p_full:.3} healed"
            ));
        }
    }
    None
}

impl Tool for BeatTool {
    fn name(&self) -> &str {
        "Beat"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Beat { .. })
    }

    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
        let Intent::Beat {
            trainer,
            map,
            object,
        } = intent
        else {
            return ToolOutcome::failed("not a Beat");
        };
        let party = Party::from_state(ctx.state());
        if let Some(why) = unready(&ctx.data, &party, trainer) {
            if let Err(e) = ctx.emit(progress("Beat", format!("healing before {trainer}: {why}"))) {
                return e.into();
            }
            let healed = ctx.invoke(&Intent::Heal { center: None });
            if let Err(e) = healed.result {
                return e.into();
            }
        }
        ctx.invoke(&Intent::Talk {
            map: map.clone(),
            object: *object,
            answers: Vec::new(),
        })
        .result
        .into()
    }
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

    #[test]
    fn a_worn_lead_heals_before_a_trainer() {
        let Some(data) = data() else { return };
        let mut m = Member::new(&data, "SPECIES_IVYSAUR", 21);
        m.moves = vec![
            "MOVE_TACKLE".into(),
            "MOVE_SLEEP_POWDER".into(),
            "MOVE_LEECH_SEED".into(),
            "MOVE_VINE_WHIP".into(),
        ];
        m.hp = Some((62, 62));
        let party = |m: &Member| Party {
            members: vec![m.clone()],
        };
        // Flash-9: 19/62 HP.
        m.hp = Some((19, 62));
        let why = unready(&data, &party(&m), "TRAINER_NOBODY").expect("unready");
        assert!(why.contains("19/62"), "{why}");
        // Full HP but Tackle and Vine Whip used up.
        m.hp = Some((62, 62));
        m.pp_used.insert("MOVE_TACKLE".into(), 35);
        m.pp_used.insert("MOVE_VINE_WHIP".into(), 10);
        let why = unready(&data, &party(&m), "TRAINER_NOBODY").expect("unready");
        assert!(why.contains("no attacking move"), "{why}");
        // Rested: ready (an unknown trainer has P(win) 0 either way).
        m.pp_used.clear();
        assert_eq!(unready(&data, &party(&m), "TRAINER_NOBODY"), None);
    }
}
