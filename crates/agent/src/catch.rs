//! Whether and how to catch a wild Pokémon (the pure decision).
//!
//! A shiny is always caught, with any ball including the reserve. Otherwise
//! a species not caught yet is caught only when both hold: the probability
//! that our lead faints during the whole attempt (weakening turns plus the
//! expected throws, the foe using its most damaging move every turn) is at
//! most [`RISK_LIMIT`], and the known ball stock exceeds the shiny reserve
//! plus the expected throws. Weakening opens with a status move (sleep before
//! paralysis), then uses only moves whose critical maximum roll cannot faint
//! the foe at the lowest HP its bar allows, until the bar is below
//! [`WEAKENED_PER_MILLE`]. Trainers' Pokémon are never caught, and unknown
//! facts (caught flag, ball stock) decline the catch rather than guess.

use pokebot_gamedata::mechanics::{ball_multiplier, catch_probability_status, damage, DamageRolls};
use pokebot_gamedata::GameData;
use pokebot_planner::evaluate::faint_probability;
use pokebot_planner::Combatant;
use pokebot_state::{GameState, Pocket};

use crate::party::Member;
use crate::stock::{ball_count, SHINY_RESERVE};

/// Largest accepted P(our lead faints during the attempt).
pub const RISK_LIMIT: f64 = 0.02;
/// Weakening stops once the foe's HP bar is below this (‰).
pub const WEAKENED_PER_MILLE: u16 = 250;
/// Caps on the estimates.
const MAX_THROWS: u32 = 20;
const MAX_WEAKENING_TURNS: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoeStatus {
    None,
    Asleep,
    Paralyzed,
}

/// The wild opponent as read from the HUD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foe {
    pub species: String,
    pub level: u8,
    /// HP bar fill, 0–1000.
    pub hp_per_mille: u16,
    pub status: FoeStatus,
    pub shiny: bool,
    /// Pokédex caught flag; `None` when unknown.
    pub caught: Option<bool>,
}

/// Our battling Pokémon and its current/maximum HP.
#[derive(Debug, Clone, Copy)]
pub struct Lead<'a> {
    pub member: &'a Member,
    pub hp: (u16, u16),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatchPlan {
    pub ball: String,
    /// Opening status move (slot, move); `None` to weaken without it or to
    /// throw at once.
    pub status_move: Option<(u8, String)>,
    pub expected_throws: u32,
    /// P(our lead faints during the attempt).
    pub risk: f64,
    pub shiny: bool,
}

/// The Poké Balls pocket, `None` when unknown or stale (needs an audit).
pub fn balls_held(state: &GameState) -> Option<Vec<(String, u16)>> {
    let pocket = state.bag.pockets.get(&Pocket::PokeBalls)?;
    if pocket.needs_audit() {
        return None;
    }
    pocket.value.clone()
}

/// The held ball with the highest multiplier; ties: cheapest, then name.
/// The Master Ball is never chosen.
pub fn best_ball(data: &GameData, balls: &[(String, u16)]) -> Option<String> {
    let price = |item: &str| data.items.get(item).map_or(u32::MAX, |i| i.price);
    balls
        .iter()
        .filter(|(_, n)| *n > 0)
        .filter_map(|(item, _)| Some((ball_multiplier(item)?, item)))
        .min_by(|(ma, a), (mb, b)| mb.cmp(ma).then(price(a).cmp(&price(b))).then(a.cmp(b)))
        .map(|(_, item)| item.clone())
}

/// The status move to open with (sleep before paralysis, then slot order),
/// only with PP left and only on a foe without a status.
pub fn status_move(data: &GameData, lead: &Member, foe: FoeStatus) -> Option<(u8, String)> {
    if foe != FoeStatus::None {
        return None;
    }
    lead.moves
        .iter()
        .enumerate()
        .filter(|(_, m)| lead.pp_left(data, m) > 0)
        .filter_map(|(slot, m)| {
            let rank = match data.move_(m)?.effect.as_deref()? {
                "EFFECT_SLEEP" => 0,
                "EFFECT_PARALYZE" => 1,
                _ => return None,
            };
            Some((rank, slot, m))
        })
        .min()
        .map(|(_, slot, m)| (slot as u8, m.clone()))
}

/// The attack that weakens the foe fastest without any risk of fainting it:
/// its critical maximum roll (our iv 31 vs foe iv 0) stays below the lowest
/// HP the bar allows. Ties: highest normal maximum, then move name.
pub fn weakening_move(data: &GameData, lead: &Lead, foe: &Foe) -> Option<(u8, String)> {
    let us = our_combatant(data, lead, 31)?;
    let them = foe_combatant(data, foe, 0)?;
    let floor = hp_floor(them.max_hp(), foe.hp_per_mille);
    lead.member
        .moves
        .iter()
        .enumerate()
        .filter(|(_, m)| lead.member.pp_left(data, m) > 0)
        .filter_map(|(slot, m)| {
            let rolls = rolls(data, m, &us, &them)?;
            let crit = rolls.critical.iter().max().copied()?;
            let normal = rolls.normal.iter().max().copied()?;
            (crit < floor).then_some((normal, slot, m))
        })
        .max_by(|(na, _, a), (nb, _, b)| na.cmp(nb).then(b.cmp(a)))
        .map(|(_, slot, m)| (slot as u8, m.clone()))
}

/// P(our lead faints within `turns` attacks of the foe's most damaging move),
/// the foe at iv 31 and us at iv 0 from our current HP. 1 when either side
/// is unknown to the game data.
pub fn risk(data: &GameData, lead: &Lead, foe: &Foe, turns: u32) -> f64 {
    let (Some(them), Some(us)) = (foe_combatant(data, foe, 31), our_combatant(data, lead, 0))
    else {
        return 1.0;
    };
    faint_probability(data, &them, &us, turns as usize)
}

/// Decide whether to catch `foe`, and how. `Err` carries the reason not to.
pub fn plan_catch(
    data: &GameData,
    state: &GameState,
    lead: &Lead,
    foe: &Foe,
    trainer: bool,
) -> Result<CatchPlan, String> {
    if trainer {
        return Err("a trainer's Pokémon can't be caught".into());
    }
    if !foe.shiny && foe.caught != Some(false) {
        return Err(format!("{}: caught flag {:?}", foe.species, foe.caught));
    }
    let ball = match balls_held(state) {
        Some(balls) => best_ball(data, &balls).ok_or("no usable ball held")?,
        None if foe.shiny => "ITEM_POKE_BALL".to_owned(),
        None => return Err("ball count unknown (pocket not audited)".into()),
    };
    let multiplier = ball_multiplier(&ball).unwrap_or(10);
    let mut attempt = estimate(data, lead, foe, multiplier, true)?;
    let mut risk_now = risk(data, lead, foe, attempt.turns + attempt.throws);
    if foe.shiny {
        if risk_now > RISK_LIMIT {
            attempt = estimate(data, lead, foe, multiplier, false)?;
            risk_now = risk(data, lead, foe, attempt.throws);
        }
    } else {
        let count = ball_count(state).unwrap_or(0);
        if u32::from(count) <= u32::from(SHINY_RESERVE) + attempt.throws {
            return Err(format!(
                "{count} balls: not above the reserve {SHINY_RESERVE} + {} throws",
                attempt.throws
            ));
        }
        if risk_now > RISK_LIMIT {
            return Err(format!("risk {risk_now:.4} above {RISK_LIMIT}"));
        }
    }
    Ok(CatchPlan {
        ball,
        status_move: attempt.status_move,
        expected_throws: attempt.throws,
        risk: risk_now,
        shiny: foe.shiny,
    })
}

/// The attempt's shape: opener, expected throws and weakening turns.
struct Attempt {
    status_move: Option<(u8, String)>,
    throws: u32,
    turns: u32,
}

/// Estimate an attempt with `ball` (×10); `weaken` false throws at once.
fn estimate(
    data: &GameData,
    lead: &Lead,
    foe: &Foe,
    ball: u32,
    weaken: bool,
) -> Result<Attempt, String> {
    let unknown = || format!("{} unknown to the game data", foe.species);
    let species = data.species(&foe.species).ok_or_else(unknown)?;
    let max = foe_combatant(data, foe, 0).ok_or_else(unknown)?.max_hp();
    let hp_now = (max * u32::from(foe.hp_per_mille) / 1000).max(1);
    let opener = weaken
        .then(|| status_move(data, lead.member, foe.status))
        .flatten();
    let attack = weaken.then(|| weakening_move(data, lead, foe)).flatten();
    let target = if attack.is_some() {
        hp_now
            .min(max * u32::from(WEAKENED_PER_MILLE) / 1000)
            .max(1)
    } else {
        hp_now
    };
    let status_x10 = match opener.as_ref().map(|(_, m)| data.move_(m)) {
        Some(Some(m)) if m.effect.as_deref() == Some("EFFECT_SLEEP") => 20,
        Some(_) => 15,
        None => match foe.status {
            FoeStatus::Asleep => 20,
            FoeStatus::Paralyzed => 15,
            FoeStatus::None => 10,
        },
    };
    let p = catch_probability_status(species.catch_rate, max, target, ball, status_x10);
    let throws = if p > 0.0 {
        ((1.0 / p).ceil() as u32).clamp(1, MAX_THROWS)
    } else {
        MAX_THROWS
    };
    let attack_turns = attack.map_or(0, |(_, m)| {
        weakening_turns(data, lead, foe, &m, hp_now, max)
    });
    Ok(Attempt {
        turns: u32::from(opener.is_some()) + attack_turns,
        status_move: opener,
        throws,
    })
}

/// Attacks of `mv` to bring the foe from `hp_now` to a quarter of `max`,
/// with pessimistic mean damage (our iv 0 vs foe iv 31), capped.
fn weakening_turns(
    data: &GameData,
    lead: &Lead,
    foe: &Foe,
    mv: &str,
    hp_now: u32,
    max: u32,
) -> u32 {
    let excess = hp_now.saturating_sub(max / 4);
    if excess == 0 {
        return 0;
    }
    let mean = our_combatant(data, lead, 0)
        .zip(foe_combatant(data, foe, 31))
        .and_then(|(us, them)| rolls(data, mv, &us, &them))
        .map_or(0.0, |r| {
            r.hit_chance * r.normal.iter().map(|d| f64::from(*d)).sum::<f64>() / 16.0
        });
    if mean <= 0.0 {
        return MAX_WEAKENING_TURNS;
    }
    ((f64::from(excess) / mean).ceil() as u32).min(MAX_WEAKENING_TURNS)
}

/// Lowest HP the bar allows: its fill minus one pixel (1/48), at least 1.
fn hp_floor(max: u32, per_mille: u16) -> u32 {
    (max * u32::from(per_mille) / 1000)
        .saturating_sub(max / 48)
        .max(1)
}

fn our_combatant(data: &GameData, lead: &Lead, iv: u32) -> Option<Combatant> {
    let m = lead.member;
    let mut us = Combatant::new(data, &m.species, m.level, m.moves.clone(), iv)?;
    us.hp = u32::from(lead.hp.0);
    Some(us)
}

fn foe_combatant(data: &GameData, foe: &Foe, iv: u32) -> Option<Combatant> {
    let moves = data.default_moves(&foe.species, foe.level);
    Combatant::new(data, &foe.species, foe.level, moves, iv)
}

fn rolls(data: &GameData, mv: &str, from: &Combatant, to: &Combatant) -> Option<DamageRolls> {
    damage(
        data,
        data.move_(mv)?,
        &from.types,
        from.level,
        &from.stats,
        &to.types,
        &to.stats,
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pokebot_gamedata::mechanics::damage;
    use pokebot_gamedata::GameData;
    use pokebot_state::{DefaultReducer, EventRecord, GameEvent, Pocket, StateReducer};

    use super::*;
    use crate::party::Member;

    fn data() -> Option<GameData> {
        GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
            .ok()
    }

    fn ivysaur(data: &GameData) -> Member {
        let mut member = Member::new(data, "SPECIES_IVYSAUR", 18);
        member.moves = [
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_LEECH_SEED",
            "MOVE_VINE_WHIP",
        ]
        .map(String::from)
        .to_vec();
        member
    }

    fn with_balls(items: &[(&str, u16)]) -> GameState {
        let event = GameEvent::PocketObserved {
            pocket: Pocket::PokeBalls,
            items: items.iter().map(|(i, n)| ((*i).to_owned(), *n)).collect(),
        };
        DefaultReducer.reduce(&GameState::default(), &[EventRecord { frame_id: 1, event }])
    }

    fn foe(species: &str, level: u8) -> Foe {
        Foe {
            species: species.into(),
            level,
            hp_per_mille: 1000,
            status: FoeStatus::None,
            shiny: false,
            caught: Some(false),
        }
    }

    #[test]
    fn uncaught_weak_foe_is_caught() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let plan = plan_catch(&data, &state, &lead, &foe("SPECIES_PIDGEY", 6), false).unwrap();
        assert_eq!(plan.ball, "ITEM_POKE_BALL");
        assert_eq!(plan.status_move, Some((1, "MOVE_SLEEP_POWDER".into())));
        assert!(plan.risk <= RISK_LIMIT, "{plan:?}");
        assert!(plan.expected_throws >= 1);
        assert!(!plan.shiny);
        // Zubat has a safe weakening move: its turns count in the risk too.
        let plan = plan_catch(&data, &state, &lead, &foe("SPECIES_ZUBAT", 9), false).unwrap();
        assert_eq!(plan.status_move, Some((1, "MOVE_SLEEP_POWDER".into())));
        assert!(plan.risk <= RISK_LIMIT, "{plan:?}");
    }

    #[test]
    fn caught_species_is_not_caught_again() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut pidgey = foe("SPECIES_PIDGEY", 6);
        pidgey.caught = Some(true);
        assert!(plan_catch(&data, &state, &lead, &pidgey, false).is_err());
    }

    #[test]
    fn unknown_caught_flag_declines() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut pidgey = foe("SPECIES_PIDGEY", 6);
        pidgey.caught = None;
        assert!(plan_catch(&data, &state, &lead, &pidgey, false).is_err());
    }

    #[test]
    fn unknown_ball_count_declines_non_shiny() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let pidgey = foe("SPECIES_PIDGEY", 6);
        assert_eq!(balls_held(&GameState::default()), None);
        assert!(plan_catch(&data, &GameState::default(), &lead, &pidgey, false).is_err());
        // A stale (tracked) pocket counts as unknown too.
        let state = DefaultReducer.reduce(
            &with_balls(&[("ITEM_POKE_BALL", 10)]),
            &[EventRecord {
                frame_id: 2,
                event: GameEvent::ItemsChanged {
                    pocket: Pocket::PokeBalls,
                    item: "ITEM_POKE_BALL".into(),
                    delta: -1,
                    reason: "thrown".into(),
                },
            }],
        );
        assert_eq!(balls_held(&state), None);
        assert!(plan_catch(&data, &state, &lead, &pidgey, false).is_err());
    }

    #[test]
    fn no_catch_at_or_below_the_reserve() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let pidgey = foe("SPECIES_PIDGEY", 6);
        // 6 balls ≤ 5 reserve + at least one throw.
        let state = with_balls(&[("ITEM_POKE_BALL", 6)]);
        let err = plan_catch(&data, &state, &lead, &pidgey, false).unwrap_err();
        assert!(err.contains("reserve"), "{err}");
    }

    #[test]
    fn shiny_uses_the_reserve() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let mut pidgey = foe("SPECIES_PIDGEY", 6);
        pidgey.shiny = true;
        pidgey.caught = Some(true);
        let state = with_balls(&[("ITEM_POKE_BALL", 3)]);
        let plan = plan_catch(&data, &state, &lead, &pidgey, false).unwrap();
        assert_eq!(plan.ball, "ITEM_POKE_BALL");
        assert!(plan.shiny);
        // Unknown pocket: still Ok, planned with a Poké Ball.
        let plan = plan_catch(&data, &GameState::default(), &lead, &pidgey, false).unwrap();
        assert_eq!(plan.ball, "ITEM_POKE_BALL");
        assert!(plan.expected_throws >= 1);
        // No ball at all (only a Master Ball): Err even for a shiny.
        let state = with_balls(&[("ITEM_MASTER_BALL", 1)]);
        assert!(plan_catch(&data, &state, &lead, &pidgey, false).is_err());
    }

    #[test]
    fn risky_foe_is_not_caught() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (3, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 20)]);
        let geodude = foe("SPECIES_GEODUDE", 9);
        let err = plan_catch(&data, &state, &lead, &geodude, false).unwrap_err();
        assert!(err.contains("risk"), "{err}");
        // A shiny is still attempted, throwing at once.
        let shiny = Foe {
            shiny: true,
            ..geodude
        };
        let plan = plan_catch(&data, &state, &lead, &shiny, false).unwrap();
        assert_eq!(plan.status_move, None);
        assert!(plan.risk > RISK_LIMIT);
    }

    #[test]
    fn trainer_mons_are_never_caught() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let mut pidgey = foe("SPECIES_PIDGEY", 6);
        assert!(plan_catch(&data, &state, &lead, &pidgey, true).is_err());
        pidgey.shiny = true;
        assert!(plan_catch(&data, &state, &lead, &pidgey, true).is_err());
    }

    /// Crit maximum (our iv 31 vs foe iv 0) of `mv` and the foe's HP floor.
    fn crit_max_and_floor(data: &GameData, mv: &str, foe: &Foe) -> (u32, u32) {
        let us = Combatant::new(data, "SPECIES_IVYSAUR", 18, vec![], 31).unwrap();
        let them = Combatant::new(data, &foe.species, foe.level, vec![], 0).unwrap();
        let rolls = damage(
            data,
            data.move_(mv).unwrap(),
            &us.types,
            us.level,
            &us.stats,
            &them.types,
            &them.stats,
        )
        .unwrap();
        let max = them.max_hp();
        let floor = (max * u32::from(foe.hp_per_mille) / 1000)
            .saturating_sub(max / 48)
            .max(1);
        (rolls.critical[15], floor)
    }

    #[test]
    fn weakening_move_is_safe_even_on_a_crit() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        // Zubat Lv9 at full HP: Vine Whip (resisted) can't KO even on a crit;
        // Tackle could.
        let zubat = foe("SPECIES_ZUBAT", 9);
        let (slot, mv) = weakening_move(&data, &lead, &zubat).unwrap();
        assert_eq!((slot, mv.as_str()), (3, "MOVE_VINE_WHIP"));
        let (crit, floor) = crit_max_and_floor(&data, &mv, &zubat);
        assert!(crit < floor, "{crit} vs {floor}");
        let (tackle, _) = crit_max_and_floor(&data, "MOVE_TACKLE", &zubat);
        assert!(tackle >= floor);
        // Mankey Lv7 at 400‰: every move's crit could KO it — none is safe.
        let mankey = Foe {
            hp_per_mille: 400,
            ..foe("SPECIES_MANKEY", 7)
        };
        for mv in ["MOVE_TACKLE", "MOVE_VINE_WHIP"] {
            let (crit, floor) = crit_max_and_floor(&data, mv, &mankey);
            assert!(crit >= floor, "{mv}: {crit} vs {floor}");
        }
        assert_eq!(weakening_move(&data, &lead, &mankey), None);
        // Low HP: nothing is safe.
        for f in [&zubat, &mankey] {
            let low = Foe {
                hp_per_mille: 60,
                ..f.clone()
            };
            assert_eq!(weakening_move(&data, &lead, &low), None);
        }
    }

    #[test]
    fn asleep_foe_gets_no_status_move() {
        let Some(data) = data() else { return };
        let member = ivysaur(&data);
        assert_eq!(
            status_move(&data, &member, FoeStatus::None),
            Some((1, "MOVE_SLEEP_POWDER".into()))
        );
        assert_eq!(status_move(&data, &member, FoeStatus::Asleep), None);
        assert_eq!(status_move(&data, &member, FoeStatus::Paralyzed), None);
        // Without PP left, no status move.
        let mut tired = member.clone();
        tired.pp_used.insert("MOVE_SLEEP_POWDER".into(), 15);
        assert_eq!(status_move(&data, &tired, FoeStatus::None), None);
        let lead = Lead {
            member: &member,
            hp: (54, 54),
        };
        let state = with_balls(&[("ITEM_POKE_BALL", 10)]);
        let asleep = Foe {
            status: FoeStatus::Asleep,
            ..foe("SPECIES_PIDGEY", 6)
        };
        let plan = plan_catch(&data, &state, &lead, &asleep, false).unwrap();
        assert_eq!(plan.status_move, None);
    }

    #[test]
    fn best_ball_prefers_multiplier_then_price() {
        let Some(data) = data() else { return };
        let held = |v: &[(&str, u16)]| -> Vec<(String, u16)> {
            v.iter().map(|(i, n)| ((*i).to_owned(), *n)).collect()
        };
        assert_eq!(
            best_ball(
                &data,
                &held(&[("ITEM_POKE_BALL", 5), ("ITEM_GREAT_BALL", 1)])
            ),
            Some("ITEM_GREAT_BALL".into())
        );
        // Premier and Poké Ball both cost ¥200: the cheaper, then the name.
        let price = |i: &str| data.items[i].price;
        let expected = match price("ITEM_PREMIER_BALL").cmp(&price("ITEM_POKE_BALL")) {
            std::cmp::Ordering::Less => "ITEM_PREMIER_BALL",
            _ => "ITEM_POKE_BALL",
        };
        assert_eq!(
            best_ball(
                &data,
                &held(&[("ITEM_PREMIER_BALL", 1), ("ITEM_POKE_BALL", 1)])
            ),
            Some(expected.into())
        );
        assert_eq!(best_ball(&data, &held(&[("ITEM_MASTER_BALL", 1)])), None);
        assert_eq!(best_ball(&data, &held(&[("ITEM_POKE_BALL", 0)])), None);
    }
}
