//! IVs and natures worked out from each member's stat readings (the
//! state keeps the readings and EVs; the species' base stats are here).

use pokebot_gamedata::training::{nature_named, solve, Sample, ALL_NATURES, MAX_IV};
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, GameState, IvEstimate, PartyMon};

/// A new estimate for every member whose readings, EVs or read nature
/// changed since the last one.
pub fn estimates(data: &GameData, state: &GameState) -> Vec<GameEvent> {
    let Some(party) = &state.party.value else {
        return Vec::new();
    };
    party
        .iter()
        .enumerate()
        .filter_map(|(slot, m)| {
            let estimate = estimate(data, m)?;
            let current = m.training.estimate.as_ref();
            (current != Some(&estimate)).then_some(GameEvent::IvsEstimated {
                slot: slot as u8,
                estimate,
            })
        })
        .collect()
}

/// The member's estimate, or `None` while it has no readings or it is
/// already solved for them.
pub fn estimate(data: &GameData, m: &PartyMon) -> Option<IvEstimate> {
    let t = &m.training;
    let nature = m
        .details
        .nature
        .value
        .clone()
        .filter(|n| nature_named(n).is_some());
    if t.samples.is_empty()
        || t.estimate
            .as_ref()
            .is_some_and(|e| e.revision == t.revision && e.nature_read == nature)
    {
        return None;
    }
    let samples: Vec<Sample> = t
        .samples
        .iter()
        .filter_map(|s| {
            Some(Sample {
                base: data.species(&s.species)?.base,
                fixed_hp: s.species == "SPECIES_SHEDINJA",
                level: s.level,
                stats: s.stats,
                ev_low: s.evs.low,
                ev_high: s.evs.high,
            })
        })
        .collect();
    let natures = nature
        .as_deref()
        .and_then(nature_named)
        .map_or(ALL_NATURES, |n| 1 << n);
    let solved = solve(&samples, natures);
    Some(IvEstimate {
        revision: t.revision,
        nature_read: nature,
        natures: solved
            .map(|e| e.nature_names().into_iter().map(str::to_owned).collect())
            .unwrap_or_default(),
        ivs: std::array::from_fn(|i| solved.and_then(|e| e.iv_range(i)).unwrap_or((0, MAX_IV))),
        consistent: solved.is_some_and(|e| e.consistent),
        exact_evs: t.exact(),
    })
}
