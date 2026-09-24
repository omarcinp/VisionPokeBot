//! Learning a move when four are already known, driven entirely by what is
//! on screen: the text names the offered move, the KNOWN MOVES list shows
//! the current moves, and the confirmation text says what was forgotten and
//! learned. Party changes are emitted as events only when that text is seen.

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, MoveListObservation};

use crate::moves::{self, LearnChoice};
use crate::party::{display_name, names_match, Member, Party};
use crate::{Action, Decision, Expectation};

/// Rows on the KNOWN MOVES list: four moves, then the offered one.
const NEW_MOVE_ROW: u8 = 4;

/// What one page of text says about moves and evolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextFact {
    /// `<name> is trying to learn <move>.`
    Offered { pokemon: String, mv: String },
    /// `<name> forgot <move>.`
    Forgot { pokemon: String, mv: String },
    /// `<name> learned <move>!`
    Learned { pokemon: String, mv: String },
    /// `<name> did not learn <move>.`
    DidNotLearn { pokemon: String, mv: String },
    /// `… your <name> evolved into <species>!`
    Evolved { pokemon: String, into: String },
}

/// Facts on one page (its lines joined with spaces).
pub fn parse(page: &str) -> Vec<TextFact> {
    let mut facts = Vec::new();
    let between = |marker: &str, end: char| -> Option<(String, String)> {
        let at = page.find(marker)?;
        let pokemon = page[..at].rsplit(' ').next()?.to_owned();
        let rest = &page[at + marker.len()..];
        let mv = rest[..rest.find(end)?].trim().to_owned();
        (!pokemon.is_empty() && !mv.is_empty()).then_some((pokemon, mv))
    };
    if let Some((pokemon, mv)) = between(" is trying to learn ", '.') {
        facts.push(TextFact::Offered { pokemon, mv });
    }
    if let Some((pokemon, mv)) = between(" forgot ", '.') {
        facts.push(TextFact::Forgot { pokemon, mv });
    }
    if let Some((pokemon, mv)) = between(" did not learn ", '.') {
        facts.push(TextFact::DidNotLearn { pokemon, mv });
    }
    if let Some((pokemon, mv)) = between(" learned ", '!') {
        facts.push(TextFact::Learned { pokemon, mv });
    }
    if let Some((pokemon, into)) = between(" evolved into ", '!') {
        facts.push(TextFact::Evolved { pokemon, into });
    }
    facts
}

/// The move currently being offered and the decision taken about it.
#[derive(Debug, Clone)]
struct Offer {
    pokemon: String,
    mv: String,
    choice: LearnChoice,
}

#[derive(Debug, Default)]
pub struct MoveLearning {
    offer: Option<Offer>,
    /// Move forgotten, awaiting "… learned X!" (slot it occupied).
    forgot_slot: Option<(String, usize)>,
    /// Last page interpreted: each page is read once.
    last_page: String,
}

impl MoveLearning {
    /// Interprets a fully printed page. Returns the party events it implies
    /// and what changed, for the log.
    pub fn observe_page(
        &mut self,
        lines: &[String],
        data: &GameData,
        party: &Party,
        targets: &[String],
    ) -> (Vec<GameEvent>, Vec<String>) {
        let page = lines.join(" ");
        if page.is_empty() || page == self.last_page {
            return (Vec::new(), Vec::new());
        }
        self.last_page = page.clone();
        let mut events = Vec::new();
        let mut log = Vec::new();
        for fact in parse(&page) {
            match fact {
                TextFact::Offered { pokemon, mv } => {
                    let Some(key) = data.move_named(&mv).map(str::to_owned) else {
                        log.push(format!("unknown move offered: {mv}"));
                        continue;
                    };
                    let Some(member) = member_named(party, &pokemon) else {
                        continue;
                    };
                    let choice = moves::choose(data, member, &key, targets);
                    log.push(format!(
                        "{pokemon} is offered {mv}: {}",
                        describe(member, choice)
                    ));
                    self.offer = Some(Offer {
                        pokemon,
                        mv: key,
                        choice,
                    });
                }
                TextFact::Forgot { pokemon, mv } => {
                    let key = data.move_named(&mv).map(str::to_owned);
                    if let (Some(member), Some(key)) = (member_named(party, &pokemon), key) {
                        if let Some(slot) = member.moves.iter().position(|m| *m == key) {
                            self.forgot_slot = Some((key, slot));
                        }
                    }
                }
                TextFact::Learned { pokemon, mv } => {
                    let Some(key) = data.move_named(&mv).map(str::to_owned) else {
                        continue;
                    };
                    let Some(member) = member_named(party, &pokemon) else {
                        continue;
                    };
                    let max_pp = data.move_(&key).map_or(0, |m| m.pp);
                    match self.forgot_slot.take() {
                        Some((old, slot)) if member.moves.get(slot) == Some(&old) => {
                            log.push(format!("{pokemon} forgot {old} and learned {mv}"));
                            events.push(GameEvent::MoveReplaced {
                                slot: member.slot,
                                move_slot: slot as u8,
                                old,
                                new: key,
                                max_pp,
                            });
                        }
                        _ if !member.moves.contains(&key) && member.moves.len() < 4 => {
                            log.push(format!("{pokemon} learned {mv}"));
                            events.push(GameEvent::MoveLearned {
                                slot: member.slot,
                                move_slot: member.moves.len() as u8,
                                mv: key,
                                max_pp,
                            });
                        }
                        _ => {}
                    }
                    self.offer = None;
                }
                TextFact::DidNotLearn { pokemon, mv } => {
                    log.push(format!("{pokemon} did not learn {mv}"));
                    self.offer = None;
                    self.forgot_slot = None;
                }
                TextFact::Evolved { pokemon, into } => {
                    let species = data
                        .species
                        .keys()
                        .filter(|k| names_match(&display_name(k), &into))
                        .min()
                        .cloned();
                    if let (Some(member), Some(species)) = (member_named(party, &pokemon), species)
                    {
                        log.push(format!("{pokemon} evolved into {into}"));
                        events.push(GameEvent::Evolved {
                            slot: member.slot,
                            species,
                        });
                    }
                }
            }
        }
        (events, log)
    }

    /// The answer to a YES/NO question about the offered move, if the page
    /// is one: `Some(true)` = YES.
    pub fn answer(&self, lines: &[String]) -> Option<bool> {
        let page = lines.join(" ");
        let offer = self.offer.as_ref()?;
        if page.contains("Delete a move to make room for") {
            return Some(matches!(offer.choice, LearnChoice::Forget(_)));
        }
        if page.contains("Stop learning") {
            return Some(offer.choice == LearnChoice::Skip);
        }
        None
    }

    /// Input on the KNOWN MOVES list: confirm the moves shown, move the
    /// frame to the chosen row and press A. The moves shown, when they differ
    /// from the party's, come back as a `MovesObserved` event.
    pub fn on_move_list(
        &mut self,
        list: &MoveListObservation,
        data: &GameData,
        party: &Party,
        targets: &[String],
    ) -> (Decision, Vec<GameEvent>) {
        let Some(offer) = self.offer.clone() else {
            return (
                Decision::Fail("move list open but no move is being offered".into()),
                Vec::new(),
            );
        };
        let Some(member) = member_named(party, &offer.pokemon) else {
            return (
                Decision::Fail(format!("{} is not in the party", offer.pokemon)),
                Vec::new(),
            );
        };
        let mut events = Vec::new();
        // The list shows the real moves: adopt them if they read cleanly.
        let shown: Option<Vec<String>> = list
            .moves
            .iter()
            .take(4)
            .map(|n| data.move_named(n).map(str::to_owned))
            .collect();
        if let Some(shown) = shown.filter(|s| s.len() == 4 && *s != member.moves) {
            events.push(GameEvent::MovesObserved {
                slot: member.slot,
                moves: shown.clone(),
            });
            let seen = Member {
                moves: shown,
                ..member.clone()
            };
            let choice = moves::choose(data, &seen, &offer.mv, targets);
            self.offer = Some(Offer {
                choice,
                ..offer.clone()
            });
        }
        let choice = self.offer.as_ref().map_or(offer.choice, |o| o.choice);
        let target = match choice {
            LearnChoice::Forget(slot) => slot as u8,
            LearnChoice::Skip => NEW_MOVE_ROW,
        };
        let Some(at) = list.selected else {
            return (Decision::Wait("move list: no selection yet".into()), events);
        };
        if at == target {
            let decision = Decision::Act(Action::new(
                format!(
                    "pick row {} ({})",
                    target + 1,
                    list.moves.get(target as usize).map_or("?", |s| s)
                ),
                vec![ControllerCommand::Press(Button::A)],
                Expectation::MoveListClosed,
                120,
            ));
            return (decision, events);
        }
        let (button, next) = if at < target {
            (Button::Down, at + 1)
        } else {
            (Button::Up, at - 1)
        };
        let decision = Decision::Act(Action::new(
            format!("move list: {button:?} toward row {}", target + 1),
            vec![ControllerCommand::Press(button)],
            Expectation::MoveListAt(next),
            45,
        ));
        (decision, events)
    }
}

fn member_named<'a>(party: &'a Party, read: &str) -> Option<&'a Member> {
    party
        .members
        .iter()
        .find(|m| names_match(&m.display_name(), read))
}

fn describe(member: &Member, choice: LearnChoice) -> String {
    match choice {
        LearnChoice::Forget(slot) => format!(
            "forget {}",
            member
                .moves
                .get(slot)
                .map_or("?", |m| m.trim_start_matches("MOVE_"))
        ),
        LearnChoice::Skip => "don't learn it".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_pages_of_a_move_swap() {
        assert_eq!(
            parse("BULBASAUR is trying to learn POISONPOWDER."),
            vec![TextFact::Offered {
                pokemon: "BULBASAUR".into(),
                mv: "POISONPOWDER".into()
            }]
        );
        assert_eq!(
            parse("BULBASAUR forgot GROWL."),
            vec![TextFact::Forgot {
                pokemon: "BULBASAUR".into(),
                mv: "GROWL".into()
            }]
        );
        assert_eq!(
            parse("BULBASAUR learned POISONPOWDER!"),
            vec![TextFact::Learned {
                pokemon: "BULBASAUR".into(),
                mv: "POISONPOWDER".into()
            }]
        );
        assert_eq!(
            parse("Congratulations! Your BULBASAUR evolved into IVYSAUR!"),
            vec![TextFact::Evolved {
                pokemon: "BULBASAUR".into(),
                into: "IVYSAUR".into()
            }]
        );
        assert!(parse("Delete a move to make room for POISONPOWDER?").is_empty());
    }
    #[test]
    fn a_swap_becomes_a_replace_event() {
        let Some(d) = pokebot_gamedata::GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok() else {
            return;
        };
        let party = Party {
            members: vec![Member {
                slot: 0,
                species: "SPECIES_BULBASAUR".into(),
                level: 15,
                moves: [
                    "MOVE_TACKLE",
                    "MOVE_GROWL",
                    "MOVE_LEECH_SEED",
                    "MOVE_VINE_WHIP",
                ]
                .map(String::from)
                .to_vec(),
                hp: None,
                pp_used: Default::default(),
            }],
        };
        let mut l = MoveLearning::default();
        let page = |s: &str| vec![s.to_owned()];
        l.observe_page(
            &page("BULBASAUR is trying to learn POISONPOWDER."),
            &d,
            &party,
            &[],
        );
        l.observe_page(&page("BULBASAUR forgot GROWL."), &d, &party, &[]);
        let (events, _) = l.observe_page(&page("BULBASAUR learned POISONPOWDER!"), &d, &party, &[]);
        assert_eq!(
            events,
            vec![GameEvent::MoveReplaced {
                slot: 0,
                move_slot: 1,
                old: "MOVE_GROWL".into(),
                new: "MOVE_POISON_POWDER".into(),
                max_pp: 35
            }]
        );
    }
}
