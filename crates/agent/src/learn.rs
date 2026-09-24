//! Learning a move when four are already known, driven entirely by what is
//! on screen: the text names the offered move, the KNOWN MOVES list shows
//! the current moves, and the confirmation text says what was forgotten and
//! learned. Party changes are emitted as events only when that text is seen.

use pokebot_core::{Button, ControllerCommand};
use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, MoveListObservation};

use crate::moves::{self, LearnChoice};
use crate::party::{names_match, Member, Party};
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

/// A move forgotten, awaiting "… learned X!".
#[derive(Debug, Clone)]
struct Forgotten {
    /// Party slot of the Pokémon that forgot it.
    party_slot: u8,
    mv: String,
    /// Move slot it occupied.
    move_slot: usize,
}

#[derive(Debug, Default)]
pub struct MoveLearning {
    offer: Option<Offer>,
    forgot_slot: Option<Forgotten>,
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
                    // A new offer: whatever was forgotten before is over.
                    self.forgot_slot = None;
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
                            self.forgot_slot = Some(Forgotten {
                                party_slot: member.slot,
                                mv: key,
                                move_slot: slot,
                            });
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
                        Some(f)
                            if f.party_slot == member.slot
                                && member.moves.get(f.move_slot) == Some(&f.mv) =>
                        {
                            log.push(format!("{pokemon} forgot {} and learned {mv}", f.mv));
                            events.push(GameEvent::MoveReplaced {
                                slot: member.slot,
                                move_slot: f.move_slot as u8,
                                old: f.mv,
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
                    let species = data.species_named(&into).map(str::to_owned);
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
    fn data() -> Option<GameData> {
        GameData::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
        )
        .ok()
    }

    fn member(slot: u8, species: &str, moves: &[&str]) -> Member {
        Member {
            slot,
            species: species.into(),
            level: 15,
            moves: moves.iter().map(|m| m.to_string()).collect(),
            hp: None,
            pp_used: Default::default(),
        }
    }

    const BULBASAUR_MOVES: [&str; 4] = [
        "MOVE_TACKLE",
        "MOVE_GROWL",
        "MOVE_LEECH_SEED",
        "MOVE_VINE_WHIP",
    ];

    fn bulbasaur() -> Party {
        Party {
            members: vec![member(0, "SPECIES_BULBASAUR", &BULBASAUR_MOVES)],
        }
    }

    fn page(s: &str) -> Vec<String> {
        vec![s.to_owned()]
    }

    /// Reads `pages` in order; the events of the last one.
    fn read(l: &mut MoveLearning, d: &GameData, party: &Party, pages: &[&str]) -> Vec<GameEvent> {
        let mut events = Vec::new();
        for p in pages {
            events = l.observe_page(&page(p), d, party, &[]).0;
        }
        events
    }

    fn list(moves: &[&str], selected: u8) -> MoveListObservation {
        MoveListObservation {
            moves: moves.iter().map(|m| m.to_string()).collect(),
            selected: Some(selected),
        }
    }

    const LIST: [&str; 5] = ["TACKLE", "GROWL", "LEECH SEED", "VINE WHIP", "POISONPOWDER"];

    fn act(decision: Decision) -> Action {
        match decision {
            Decision::Act(a) => a,
            Decision::Wait(w) => panic!("waited: {w}"),
            Decision::Done(w) => panic!("done: {w}"),
            Decision::Fail(w) => panic!("failed: {w}"),
        }
    }

    #[test]
    fn a_swap_becomes_a_replace_event() {
        let Some(d) = data() else { return };
        let mut l = MoveLearning::default();
        let events = read(
            &mut l,
            &d,
            &bulbasaur(),
            &[
                "BULBASAUR is trying to learn POISONPOWDER.",
                "BULBASAUR forgot GROWL.",
                "BULBASAUR learned POISONPOWDER!",
            ],
        );
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

    #[test]
    fn move_list_moves_toward_the_row_to_forget_and_picks_it() {
        let Some(d) = data() else { return };
        let party = bulbasaur();
        let mut l = MoveLearning::default();
        read(
            &mut l,
            &d,
            &party,
            &["BULBASAUR is trying to learn POISONPOWDER."],
        );
        // GROWL (row 2) is the one to forget.
        let (decision, events) = l.on_move_list(&list(&LIST, 3), &d, &party, &[]);
        assert!(events.is_empty(), "the list matches the party view");
        let a = act(decision);
        assert_eq!(a.commands, vec![ControllerCommand::Press(Button::Up)]);
        assert_eq!(a.expect, Expectation::MoveListAt(2));
        let a = act(l.on_move_list(&list(&LIST, 0), &d, &party, &[]).0);
        assert_eq!(a.commands, vec![ControllerCommand::Press(Button::Down)]);
        assert_eq!(a.expect, Expectation::MoveListAt(1));
        let a = act(l.on_move_list(&list(&LIST, 1), &d, &party, &[]).0);
        assert_eq!(a.commands, vec![ControllerCommand::Press(Button::A)]);
        assert_eq!(a.expect, Expectation::MoveListClosed);
    }

    #[test]
    fn listed_moves_that_differ_are_observed_and_rechosen() {
        let Some(d) = data() else { return };
        let party = bulbasaur();
        let mut l = MoveLearning::default();
        read(
            &mut l,
            &d,
            &party,
            &["BULBASAUR is trying to learn POISONPOWDER."],
        );
        assert_eq!(
            l.answer(&page("Delete a move to make room for POISONPOWDER?")),
            Some(true)
        );
        // The game shows SLEEP POWDER where the view has GROWL: nothing is
        // worth forgetting any more, so the new move is skipped (row 5).
        let shown = [
            "TACKLE",
            "SLEEP POWDER",
            "LEECH SEED",
            "VINE WHIP",
            "POISONPOWDER",
        ];
        let (decision, events) = l.on_move_list(&list(&shown, 0), &d, &party, &[]);
        assert_eq!(
            events,
            vec![GameEvent::MovesObserved {
                slot: 0,
                moves: [
                    "MOVE_TACKLE",
                    "MOVE_SLEEP_POWDER",
                    "MOVE_LEECH_SEED",
                    "MOVE_VINE_WHIP"
                ]
                .map(String::from)
                .to_vec(),
            }]
        );
        let a = act(decision);
        assert_eq!(a.commands, vec![ControllerCommand::Press(Button::Down)]);
        assert_eq!(a.expect, Expectation::MoveListAt(1));
        assert_eq!(l.answer(&page("Stop learning POISONPOWDER?")), Some(true));
    }

    #[test]
    fn a_worse_move_is_skipped_on_the_new_move_row() {
        let Some(d) = data() else { return };
        let party = Party {
            members: vec![member(
                0,
                "SPECIES_BULBASAUR",
                &[
                    "MOVE_TACKLE",
                    "MOVE_POISON_POWDER",
                    "MOVE_LEECH_SEED",
                    "MOVE_VINE_WHIP",
                ],
            )],
        };
        let mut l = MoveLearning::default();
        read(&mut l, &d, &party, &["BULBASAUR is trying to learn GROWL."]);
        let shown = ["TACKLE", "POISONPOWDER", "LEECH SEED", "VINE WHIP", "GROWL"];
        let a = act(l.on_move_list(&list(&shown, 4), &d, &party, &[]).0);
        assert_eq!(a.commands, vec![ControllerCommand::Press(Button::A)]);
        assert_eq!(a.expect, Expectation::MoveListClosed);
        assert!(a.label.contains("row 5"), "{}", a.label);
    }

    #[test]
    fn questions_are_answered_from_the_choice() {
        let Some(d) = data() else { return };
        let party = bulbasaur();
        let mut l = MoveLearning::default();
        assert_eq!(
            l.answer(&page("Delete a move to make room for POISONPOWDER?")),
            None
        );
        // Forget: delete a move, don't stop learning.
        read(
            &mut l,
            &d,
            &party,
            &["BULBASAUR is trying to learn POISONPOWDER."],
        );
        assert_eq!(
            l.answer(&page("Delete a move to make room for POISONPOWDER?")),
            Some(true)
        );
        assert_eq!(l.answer(&page("Stop learning POISONPOWDER?")), Some(false));
        assert_eq!(l.answer(&page("BULBASAUR forgot GROWL.")), None);
        // Skip: keep the moves, stop learning.
        let party = Party {
            members: vec![member(
                0,
                "SPECIES_BULBASAUR",
                &[
                    "MOVE_TACKLE",
                    "MOVE_POISON_POWDER",
                    "MOVE_LEECH_SEED",
                    "MOVE_VINE_WHIP",
                ],
            )],
        };
        read(&mut l, &d, &party, &["BULBASAUR is trying to learn GROWL."]);
        assert_eq!(
            l.answer(&page("Delete a move to make room for GROWL?")),
            Some(false)
        );
        assert_eq!(l.answer(&page("Stop learning GROWL?")), Some(true));
    }

    #[test]
    fn a_missed_forgot_page_changes_nothing() {
        let Some(d) = data() else { return };
        let mut l = MoveLearning::default();
        let events = read(
            &mut l,
            &d,
            &bulbasaur(),
            &[
                "BULBASAUR is trying to learn POISONPOWDER.",
                "BULBASAUR learned POISONPOWDER!",
            ],
        );
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_forgotten_move_from_an_earlier_offer_is_not_reused() {
        let Some(d) = data() else { return };
        let mut l = MoveLearning::default();
        let events = read(
            &mut l,
            &d,
            &bulbasaur(),
            &[
                "BULBASAUR is trying to learn POISONPOWDER.",
                "BULBASAUR forgot GROWL.",
                // The "learned" page was missed; a new offer starts.
                "BULBASAUR is trying to learn RAZOR LEAF.",
                "BULBASAUR learned RAZOR LEAF!",
            ],
        );
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_forgotten_move_applies_only_to_its_pokemon() {
        let Some(d) = data() else { return };
        let party = Party {
            members: vec![
                member(0, "SPECIES_BULBASAUR", &BULBASAUR_MOVES),
                member(1, "SPECIES_IVYSAUR", &BULBASAUR_MOVES),
            ],
        };
        let mut l = MoveLearning::default();
        let events = read(
            &mut l,
            &d,
            &party,
            &["BULBASAUR forgot GROWL.", "IVYSAUR learned POISONPOWDER!"],
        );
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_move_learned_into_a_free_slot() {
        let Some(d) = data() else { return };
        let party = Party {
            members: vec![member(
                0,
                "SPECIES_BULBASAUR",
                &["MOVE_TACKLE", "MOVE_GROWL", "MOVE_LEECH_SEED"],
            )],
        };
        let mut l = MoveLearning::default();
        let events = read(&mut l, &d, &party, &["BULBASAUR learned VINE WHIP!"]);
        assert_eq!(
            events,
            vec![GameEvent::MoveLearned {
                slot: 0,
                move_slot: 3,
                mv: "MOVE_VINE_WHIP".into(),
                max_pp: 10,
            }]
        );
    }

    #[test]
    fn evolution_text_becomes_an_event() {
        let Some(d) = data() else { return };
        let mut l = MoveLearning::default();
        let events = read(
            &mut l,
            &d,
            &bulbasaur(),
            &["Congratulations! Your BULBASAUR evolved into IVYSAUR!"],
        );
        assert_eq!(
            events,
            vec![GameEvent::Evolved {
                slot: 0,
                species: "SPECIES_IVYSAUR".into()
            }]
        );
        // Printed names map through the game's spelling.
        let party = Party {
            members: vec![member(0, "SPECIES_NIDORAN_F", &["MOVE_GROWL"])],
        };
        let events = read(
            &mut l,
            &d,
            &party,
            &["Congratulations! Your NIDORAN evolved into NIDORINA!"],
        );
        assert_eq!(
            events,
            vec![GameEvent::Evolved {
                slot: 0,
                species: "SPECIES_NIDORINA".into()
            }]
        );
        // An ambiguous reading names no species.
        let events = read(
            &mut l,
            &d,
            &bulbasaur(),
            &["Congratulations! Your BULBASAUR evolved into ???????!"],
        );
        assert!(events.is_empty(), "{events:?}");
    }
}
