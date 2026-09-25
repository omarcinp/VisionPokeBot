//! Which party member a printed name refers to. The game prints a
//! member's nickname (its species name unless renamed); readings may hold
//! `?` for glyphs the font didn't know.

use pokebot_gamedata::{printed_name, GameData};
use pokebot_state::GameState;

/// `read` could be `name`: same length, `?` matches any character.
pub fn fits(name: &str, read: &str) -> bool {
    name.chars().count() == read.chars().count()
        && name
            .chars()
            .zip(read.chars())
            .all(|(a, b)| b == '?' || a == b)
}

/// Where a printed name was found in the party.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub slot: u8,
    /// Set when the name is an evolution of the member's known species
    /// (the HUD shows the new name before any text said it evolved).
    pub evolved_into: Option<String>,
}

/// The one party member `read` names: by nickname, else by species name
/// (or an evolution of it). `None` when no member or several fit.
pub fn member(state: &GameState, data: &GameData, read: &str) -> Option<Member> {
    let party = state.party.value.as_deref()?;
    let unique = |hits: Vec<Member>| (hits.len() == 1).then(|| hits.into_iter().next()).flatten();
    let by_nickname: Vec<Member> = party
        .iter()
        .enumerate()
        .filter(|(_, m)| m.nickname.value.as_deref().is_some_and(|n| fits(n, read)))
        .map(|(slot, _)| Member {
            slot: slot as u8,
            evolved_into: None,
        })
        .collect();
    if !by_nickname.is_empty() {
        return unique(by_nickname);
    }
    let by_species: Vec<Member> = party
        .iter()
        .enumerate()
        .filter_map(|(slot, m)| {
            let species = m.species.value.as_deref()?;
            // A renamed member is not printed under its species name.
            if m.nickname
                .value
                .as_deref()
                .is_some_and(|n| n != printed_name(species))
            {
                return None;
            }
            let found = seen_as(data, species, read)?;
            Some(Member {
                slot: slot as u8,
                evolved_into: (found != species).then_some(found),
            })
        })
        .collect();
    unique(by_species)
}

/// `species` itself or the evolution of it whose name fits `read`.
fn seen_as(data: &GameData, species: &str, read: &str) -> Option<String> {
    let mut frontier = vec![species.to_owned()];
    while !frontier.is_empty() {
        let current = frontier.remove(0);
        if fits(&printed_name(&current), read) {
            return Some(current);
        }
        frontier.extend(data.evolutions_of(&current));
    }
    None
}
