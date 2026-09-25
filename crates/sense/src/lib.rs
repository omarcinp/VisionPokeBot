//! The sensor: every observation, whatever the bot is doing, becomes state
//! facts. Perception says what one frame shows; the sensor waits until a
//! reading holds (so animations, fades and half-printed text don't count),
//! uses the state as context (which party slot a printed name is, which
//! member's summary is open), and emits each confirmed reading once as
//! events. Whether the value is new is the state's business: the reducer
//! applies it and `diff` reports what actually changed.
//!
//! Readers, by screen:
//!
//! | Screen | Facts |
//! |---|---|
//! | message / battle text | money won, items found/received/obtained, balls thrown, badges, heal, catches (and where the catch went: party or PC box), level-ups, evolutions, fainting, status |
//! | battle HUD | our active member's level and HP (and evolution), the opponent species seen / already caught |
//! | battle move menu | the active member's moves and the PP of the move under the ▶ |
//! | KNOWN MOVES list | the moves of the member the page before named ("X is trying to learn Y.") |
//! | party menu | party size; each panel's nickname, level, HP and status |
//! | summary pages | species, nickname, level, HP, status, held item, moves and PP |
//! | bag (field and battle) | a pocket's items, whole or the rows on screen |
//! | mart | money |
//! | Trainer Card | badges, Pokédex caught count, money |
//! | Pokédex list | species seen and caught |
//! | Fly map | fly spots visited |
//! | field (player located) | NPCs seen on a tile, NPCs whose whole reach is empty |
//! | overworld matching several maps | the one the state points at (the committed pose's map, a map its warps lead to, the respawn point) as an inferred pose, else `LocationAmbiguous` |
//! | anything | the view: text, menu, opponent, sprites on the field |

mod field;
pub mod names;
pub mod text;

use std::sync::Arc;

use pokebot_gamedata::GameData;
use pokebot_state::{
    BagObservation, BattleMenu, BoxMon, FlyMapObservation, GameEvent, GameState, Knowledge,
    MenuView, MoveSlot, Observation, OpponentView, PartyMenuObservation, PartyMon,
    PartyRowObservation, Pocket, PokedexListObservation, ScreenState, Status, SummaryObservation,
    SummaryPage, TrainerCardObservation, ViewState,
};

/// Frames a reading must hold before it counts, per screen: long enough
/// for the screen's own animations (HP bars drain, summary panels slide,
/// Pokédex marks are drawn after the names).
const TEXT_FRAMES: u64 = 1;
const HUD_FRAMES: u64 = 10;
const MOVE_MENU_FRAMES: u64 = 4;
const SUMMARY_FRAMES: u64 = 15;
const PARTY_MENU_FRAMES: u64 = 15;
const BAG_FRAMES: u64 = 4;
const SHOP_FRAMES: u64 = 4;
const CARD_FRAMES: u64 = 8;
const DEX_FRAMES: u64 = 8;
const FLY_FRAMES: u64 = 8;
const MOVE_LIST_FRAMES: u64 = 8;
const VIEW_FRAMES: u64 = 2;
/// A page counts as printed after this many unchanged frames without its
/// arrow (the last page of a conversation has none).
const PAGE_PRINTED_FRAMES: u32 = 8;
/// Rows the bag list shows; fewer rows ending in CANCEL are the whole pocket.
const BAG_ROWS: usize = 6;

/// A reading that counts once it has read the same for a span of frames,
/// on at least two observations; each reading is given out once until the
/// screen stops showing it.
#[derive(Debug, Clone)]
struct Confirm<T> {
    first: Option<(u64, T)>,
    given: Option<T>,
}

impl<T> Default for Confirm<T> {
    fn default() -> Self {
        Self {
            first: None,
            given: None,
        }
    }
}

impl<T: PartialEq + Clone> Confirm<T> {
    fn update(&mut self, frame: u64, reading: Option<T>, span: u64) -> Option<T> {
        let Some(reading) = reading else {
            self.first = None;
            self.given = None;
            return None;
        };
        match &self.first {
            Some((since, first)) if *first == reading => {
                let held = frame.saturating_sub(*since) >= span.max(1);
                if held && self.given.as_ref() != Some(&reading) {
                    self.given = Some(reading.clone());
                    return Some(reading);
                }
                None
            }
            _ => {
                self.first = Some((frame, reading));
                None
            }
        }
    }
}

/// A move's (current, maximum) PP, when read.
type Pp = Option<(u8, u8)>;

/// Our side of the battle HUD.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hud {
    name: String,
    level: Option<u8>,
    hp: Option<(u16, u16)>,
}

/// The move menu: names in menu order, the ▶'s slot and its PP.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MoveMenu {
    names: Vec<String>,
    cursor: u8,
    pp: Option<(u8, u8)>,
}

pub struct Sensor {
    data: Arc<GameData>,
    /// Where warps lead, to tell lookalike maps apart (off without it).
    world: Option<Arc<pokebot_world::World>>,
    /// The lookalike maps last resolved or announced, so each ambiguous
    /// stretch is decided once.
    ambiguous: Option<Vec<String>>,
    page: Confirm<String>,
    hud: Confirm<Hud>,
    opponent: Confirm<(String, Option<bool>, Option<u8>)>,
    move_menu: Confirm<MoveMenu>,
    summary: Confirm<SummaryObservation>,
    party_menu: Confirm<PartyMenuObservation>,
    /// The party menu's panels, confirmed on their own: a cursor move
    /// re-confirms the menu, not the panels.
    party_rows: Confirm<Vec<PartyRowObservation>>,
    bag: Confirm<BagObservation>,
    shop: Confirm<u32>,
    card: Confirm<TrainerCardObservation>,
    dex: Confirm<PokedexListObservation>,
    fly: Confirm<FlyMapObservation>,
    move_list: Confirm<Vec<String>>,
    /// The party slot the last "X is trying to learn Y." page named.
    learning: Option<u8>,
    view: Confirm<ViewState>,
    /// Sprites on the field, confirmed across frames.
    field: field::Field,
    /// The party slot the battle HUD shows (the move menu is its moves).
    active_slot: Option<u8>,
    /// The party menu's ▶ when it was last seen: whose summary opens.
    party_selected: Option<u8>,
    /// The party menu was seen since the last overworld or battle frame (a
    /// summary opened from the PC is not a party member's).
    party_screens: bool,
    /// The opponent's level as last confirmed on its HUD.
    opponent_level: Option<u8>,
    /// A catch whose destination is settled when the battle is over.
    catch: Option<Catch>,
}

/// A Pokémon caught in the battle on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Catch {
    species: String,
    level: Option<u8>,
    /// The PC pages said it went to the PC (and which box, if read).
    to_pc: bool,
    box_index: Option<u8>,
}

impl Sensor {
    pub fn new(data: Arc<GameData>) -> Self {
        Self {
            data,
            world: None,
            ambiguous: None,
            page: Confirm::default(),
            hud: Confirm::default(),
            opponent: Confirm::default(),
            move_menu: Confirm::default(),
            summary: Confirm::default(),
            party_menu: Confirm::default(),
            party_rows: Confirm::default(),
            bag: Confirm::default(),
            shop: Confirm::default(),
            card: Confirm::default(),
            dex: Confirm::default(),
            fly: Confirm::default(),
            move_list: Confirm::default(),
            learning: None,
            view: Confirm::default(),
            field: field::Field::default(),
            active_slot: None,
            party_selected: None,
            party_screens: false,
            opponent_level: None,
            catch: None,
        }
    }

    /// Tells lookalike maps apart by where the committed pose's warps lead.
    pub fn with_world(mut self, world: Arc<pokebot_world::World>) -> Self {
        self.world = Some(world);
        self
    }

    /// The facts `o` confirms, given what the state already holds.
    pub fn observe(&mut self, o: &Observation, state: &GameState) -> Vec<GameEvent> {
        let mut events = Vec::new();
        // Fades and cut-ins show nothing; readings in progress survive them
        // only if they read the same afterwards.
        if o.screen.value == ScreenState::Transition {
            return events;
        }
        let f = o.frame_id;
        if o.player.is_some() || o.battle.is_some() {
            self.party_screens = false;
        }
        self.location(o, state, &mut events);
        self.text(o, state, &mut events);
        self.battle(o, state, &mut events);
        // Back on the field: a catch goes to the party or the PC.
        if o.player.is_some() {
            if let Some(catch) = self.catch.take() {
                events.extend(caught_events(&self.data, state, &catch));
            }
        }
        self.party(o, state, &mut events);
        self.known_moves(o, &mut events);
        self.bag(o, &mut events);
        if let Some(amount) =
            self.shop
                .update(f, o.shop.as_ref().and_then(|s| s.money), SHOP_FRAMES)
        {
            events.push(GameEvent::MoneyObserved { amount });
        }
        if let Some(card) = self.card.update(f, o.trainer_card.clone(), CARD_FRAMES) {
            events.extend(trainer_card_events(&card));
        }
        if let Some(list) = self.dex.update(f, o.pokedex_list.clone(), DEX_FRAMES) {
            events.extend(self.pokedex_rows(&list));
        }
        let fly = o.fly_map.clone().filter(|m| !m.lit.is_empty());
        if let Some(map) = self.fly.update(f, fly, FLY_FRAMES) {
            events.extend(
                map.lit
                    .iter()
                    .map(|map| GameEvent::MapVisited { map: map.clone() }),
            );
        }
        events.extend(self.field.observe(o, state));
        let view = self.view_of(o, state);
        if let Some(view) = self.view.update(f, Some(view), VIEW_FRAMES) {
            if view != state.view {
                events.push(GameEvent::ViewObserved {
                    view: Box::new(view),
                });
            }
        }
        events
    }

    /// A frame that matches several maps equally (they share a layout):
    /// the one the state points at becomes an inferred pose, in this
    /// order: the committed pose's map (still in the same Center), a map
    /// the committed map's warps or edges lead to (walked in from the
    /// town), the respawn point (a white-out or a reset left no pose).
    /// With none or several, `LocationAmbiguous` for the scheduler.
    fn location(&mut self, o: &Observation, state: &GameState, events: &mut Vec<GameEvent>) {
        if o.player.is_some() {
            self.ambiguous = None;
            return;
        }
        if o.pose_candidates.len() < 2 {
            return;
        }
        let mut maps: Vec<String> = o
            .pose_candidates
            .iter()
            .map(|c| c.pose.map.clone())
            .collect();
        maps.sort();
        if self.ambiguous.as_ref() == Some(&maps) {
            return;
        }
        self.ambiguous = Some(maps);
        let committed = state.player.pose.value.as_ref();
        let only = |keep: &dyn Fn(&str) -> bool| {
            let mut hits = o.pose_candidates.iter().filter(|c| keep(&c.pose.map));
            let first = hits.next()?;
            hits.next().is_none().then(|| first.pose.clone())
        };
        let same = committed.and_then(|p| only(&|m| m == p.map));
        // Maps `map`'s warps and edges lead to.
        let leads = |map: &str| -> Vec<String> {
            let Some(world) = self.world.as_ref() else {
                return Vec::new();
            };
            let Some(from) = world.map(map) else {
                return Vec::new();
            };
            from.warps
                .iter()
                .filter_map(|w| world.name_of(&w.dest_map))
                .chain(
                    from.connections
                        .iter()
                        .filter_map(|c| world.name_of(&c.map)),
                )
                .map(str::to_owned)
                .collect()
        };
        let next_door = || {
            let leads = leads(&committed?.map);
            only(&|m| leads.iter().any(|l| l == m))
        };
        // The respawn point is the spot outside the Center (`HealSpot`):
        // the player is on it or in the Center its door leads to.
        let respawn = || {
            let spot = state.world.respawn.value.as_ref()?;
            if committed.is_some() {
                return None;
            }
            let leads = leads(&spot.map);
            only(&|m| m == spot.map || leads.iter().any(|l| l == m))
        };
        match same.or_else(next_door).or_else(respawn) {
            Some(pose) => events.push(GameEvent::PlayerInferred {
                pose,
                candidates: Vec::new(),
            }),
            None => events.push(GameEvent::LocationAmbiguous {
                candidates: o.pose_candidates.iter().map(|c| c.pose.clone()).collect(),
            }),
        }
    }

    /// A fully printed page, applied once.
    fn text(&mut self, o: &Observation, state: &GameState, events: &mut Vec<GameEvent>) {
        let page = o
            .dialogue
            .as_ref()
            .filter(|d| d.ready_for_a() || d.stable_frames >= PAGE_PRINTED_FRAMES)
            .map(|d| d.lines.join(" "))
            .filter(|p| !p.trim().is_empty());
        // Keep the page while its box is up (A pressed on it clears the
        // arrow for a frame or two before the next page prints).
        let reading = page.or_else(|| o.dialogue.as_ref().and_then(|_| self.page.given.clone()));
        let Some(page) = self.page.update(o.frame_id, reading, TEXT_FRAMES) else {
            return;
        };
        events.extend(text::page_events(&page, &self.data));
        if let Some(species) = text::caught_name(&page).and_then(|n| self.data.species_named(&n)) {
            if self.catch.as_ref().is_none_or(|c| c.species != species) {
                self.catch = Some(Catch {
                    species: species.to_owned(),
                    level: self.opponent_level,
                    to_pc: false,
                    box_index: None,
                });
            }
        }
        if let Some(catch) = self
            .catch
            .as_mut()
            .filter(|_| text::is_pc_transfer_text(&page))
        {
            catch.to_pc = true;
            catch.box_index = text::box_from_text(&page).or(catch.box_index);
        }
        if let Some((name, _)) = text::trying_to_learn(&page) {
            self.learning = names::member(state, &self.data, &name).map(|m| m.slot);
        }
        for fact in text::mon_facts(&page) {
            match fact {
                text::MonFact::GrewTo { name, level } => {
                    if let Some(m) = names::member(state, &self.data, &name) {
                        events.push(party_observed(m.slot, |e| e.level = Some(level)));
                    }
                }
                text::MonFact::Evolved { name, into } => {
                    let member = names::member(state, &self.data, &name);
                    if let (Some(m), Some(species)) = (member, self.data.species_named(&into)) {
                        events.push(GameEvent::Evolved {
                            slot: m.slot,
                            species: species.to_owned(),
                        });
                    }
                }
                text::MonFact::Fainted { name } => {
                    if let Some(m) = names::member(state, &self.data, &name) {
                        let max = member_hp(state, m.slot).map(|(_, max)| max);
                        events.push(party_observed(m.slot, |e| {
                            e.hp = max.map(|max| (0, max));
                            e.status = Some(Status::Fainted);
                        }));
                    }
                }
            }
        }
        if let Some((name, status)) = status_text(&page) {
            if let Some(m) = names::member(state, &self.data, name) {
                events.push(party_observed(m.slot, |e| e.status = Some(status)));
            }
        }
    }

    /// The HUDs and the move menu.
    fn battle(&mut self, o: &Observation, state: &GameState, events: &mut Vec<GameEvent>) {
        let f = o.frame_id;
        let b = o.battle.as_ref();
        let hud = b.and_then(|b| {
            let level = b.player_level;
            Some(Hud {
                name: b.player_name.clone().filter(|n| !n.is_empty())?,
                level,
                // Every Pokémon but Shedinja has at least level + 10 HP; the
                // Switch OCR has read `22` as `2`.
                hp: b.player_hp_numbers.filter(|&(cur, max)| {
                    cur <= max && (max >= u16::from(level.unwrap_or(1)) + 10 || max == 1)
                }),
            })
        });
        if let Some(hud) = self.hud.update(f, hud, HUD_FRAMES) {
            let member = names::member(state, &self.data, &hud.name).or_else(|| {
                // A species the knowledge can't explain (restored from
                // another save): a lone member can only be that one.
                let party = state.party.value.as_ref().filter(|p| p.len() == 1)?;
                let species = self.data.species_named(&hud.name)?;
                (party[0].species.value.as_deref() != Some(species)).then(|| names::Member {
                    slot: 0,
                    evolved_into: Some(species.to_owned()),
                })
            });
            self.active_slot = member.as_ref().map(|m| m.slot);
            if let Some(m) = member {
                if let Some(species) = m.evolved_into {
                    events.push(GameEvent::Evolved {
                        slot: m.slot,
                        species,
                    });
                }
                if hud.level.is_some() || hud.hp.is_some() {
                    events.push(party_observed(m.slot, |e| {
                        e.level = hud.level;
                        e.hp = hud.hp;
                        if hud.hp.is_some_and(|(cur, _)| cur == 0) {
                            e.status = Some(Status::Fainted);
                        }
                    }));
                }
            }
        }
        let opponent = b.and_then(|b| {
            let name = b.opponent_name.clone().filter(|n| !n.contains('?'))?;
            b.opponent_hp?;
            Some((name, b.opponent_caught, b.opponent_level))
        });
        if let Some((name, caught, level)) = self.opponent.update(f, opponent, HUD_FRAMES) {
            self.opponent_level = level.or(self.opponent_level);
            if let Some(species) = self.data.species_named(&name) {
                let species = species.to_owned();
                events.push(match caught {
                    Some(true) => GameEvent::SpeciesCaught { species },
                    _ => GameEvent::SpeciesSeen { species },
                });
            }
        }
        let menu = b.and_then(|b| match b.menu {
            Some(BattleMenu::Moves { column, row }) if !b.move_names.is_empty() => Some(MoveMenu {
                names: b.move_names.clone(),
                cursor: row * 2 + column,
                pp: b.move_pp,
            }),
            _ => None,
        });
        if let (Some(menu), Some(slot)) = (
            self.move_menu.update(f, menu, MOVE_MENU_FRAMES),
            self.active_slot,
        ) {
            let moves: Option<Vec<String>> = menu
                .names
                .iter()
                .filter(|n| !n.trim().is_empty() && n.trim() != "-")
                .map(|n| self.data.move_named(n).map(str::to_owned))
                .collect();
            if let Some(moves) = moves.filter(|m| !m.is_empty()) {
                let at = usize::from(menu.cursor);
                events.push(GameEvent::MovesObserved {
                    slot,
                    moves: moves.clone(),
                });
                if let (Some((cur, max)), true) = (menu.pp, at < moves.len()) {
                    if cur <= max && max > 0 {
                        events.push(GameEvent::MovePpObserved {
                            slot,
                            move_slot: menu.cursor,
                            cur,
                            max,
                        });
                    }
                }
            }
        }
    }

    /// The party menu and the summary pages.
    fn party(&mut self, o: &Observation, state: &GameState, events: &mut Vec<GameEvent>) {
        let f = o.frame_id;
        if let Some(menu) = &o.party_menu {
            self.party_screens = true;
            if !menu.actions {
                self.party_selected = menu.selected.or(self.party_selected);
            }
        }
        let list = o.party_menu.clone().filter(|m| !m.actions && m.count > 0);
        let rows = list.as_ref().map(|m| m.members.clone());
        if let Some(menu) = self.party_menu.update(f, list, PARTY_MENU_FRAMES) {
            events.push(GameEvent::PartySizeObserved { size: menu.count });
        }
        if let Some(rows) = self.party_rows.update(f, rows, PARTY_MENU_FRAMES) {
            events.extend(
                rows.iter()
                    .enumerate()
                    .filter_map(|(slot, row)| party_row_event(slot as u8, row, state)),
            );
        }
        let summary = o
            .summary
            .clone()
            .filter(|s| self.party_screens && !s.nickname.is_empty() && !s.nickname.contains('?'));
        let Some(s) = self.summary.update(f, summary, SUMMARY_FRAMES) else {
            return;
        };
        let slot = names::member(state, &self.data, &s.nickname)
            .map(|m| m.slot)
            .or(self.party_selected);
        let Some(slot) = slot else { return };
        events.extend(self.summary_events(slot, &s, state));
    }

    /// What one summary page shows, checked the way the party audit does.
    fn summary_events(
        &self,
        slot: u8,
        s: &SummaryObservation,
        state: &GameState,
    ) -> Vec<GameEvent> {
        let mut events = Vec::new();
        match s.page {
            SummaryPage::Info => {
                let species = s
                    .species
                    .as_deref()
                    .and_then(|n| self.data.species_named(n))
                    .map(str::to_owned);
                let held = match s.held_item.as_deref() {
                    Some("NONE") => Some(None),
                    Some(name) => self.data.item_named(name).map(|i| Some(i.to_owned())),
                    None => None,
                };
                events.push(party_observed(slot, |e| {
                    e.species = species;
                    e.nickname = Some(s.nickname.clone());
                    e.level = s.level;
                    e.status = s.status;
                    e.held_item = held;
                }));
            }
            SummaryPage::Skills => {
                let level = s.level.or_else(|| {
                    state
                        .party
                        .value
                        .as_ref()?
                        .get(usize::from(slot))?
                        .level
                        .value
                });
                let hp = s.hp.filter(|&(cur, max)| {
                    cur <= max
                        && max > 0
                        && level.is_none_or(|l| max >= u16::from(l) + 10 || max == 1)
                });
                if hp.is_some() || s.level.is_some() {
                    events.push(party_observed(slot, |e| {
                        e.nickname = Some(s.nickname.clone());
                        e.level = s.level;
                        e.hp = hp;
                        if hp.is_some_and(|(cur, _)| cur == 0) {
                            e.status = Some(Status::Fainted);
                        }
                    }));
                }
            }
            SummaryPage::Moves => {
                let moves: Option<Vec<(String, Pp)>> = s
                    .moves
                    .iter()
                    .map(|(name, pp)| Some((self.data.move_named(name)?.to_owned(), *pp)))
                    .collect();
                let Some(moves) = moves.filter(|m| !m.is_empty() && m.len() <= 4) else {
                    return events;
                };
                events.push(GameEvent::MovesObserved {
                    slot,
                    moves: moves.iter().map(|(m, _)| m.clone()).collect(),
                });
                for (i, (_, pp)) in moves.iter().enumerate() {
                    if let Some((cur, max)) = pp.filter(|(cur, max)| cur <= max && *max > 0) {
                        events.push(GameEvent::MovePpObserved {
                            slot,
                            move_slot: i as u8,
                            cur,
                            max,
                        });
                    }
                }
            }
        }
        events
    }

    /// The KNOWN MOVES list: its first four rows are the learning member's
    /// moves (the fifth is the move offered).
    fn known_moves(&mut self, o: &Observation, events: &mut Vec<GameEvent>) {
        let rows = o
            .move_list
            .as_ref()
            .map(|l| l.moves.clone())
            .filter(|m| m.len() >= 2 && m.iter().all(|n| !n.contains('?')));
        let Some(rows) = self.move_list.update(o.frame_id, rows, MOVE_LIST_FRAMES) else {
            return;
        };
        let Some(slot) = self.learning else { return };
        let known = &rows[..rows.len() - 1];
        let moves: Option<Vec<String>> = known
            .iter()
            .map(|n| self.data.move_named(n).map(str::to_owned))
            .collect();
        if let Some(moves) = moves.filter(|m| m.len() == 4) {
            events.push(GameEvent::MovesObserved { slot, moves });
        }
    }

    /// A pocket: whole when the list ends in CANCEL before it fills.
    fn bag(&mut self, o: &Observation, events: &mut Vec<GameEvent>) {
        let Some(bag) = self.bag.update(o.frame_id, o.bag.clone(), BAG_FRAMES) else {
            return;
        };
        let Some(pocket) = pocket_from_title(&bag.pocket) else {
            return;
        };
        let cancel = bag
            .rows
            .iter()
            .position(|(n, _)| names::fits("CANCEL", n) || names::fits("CLOSE BAG", n));
        let rows = &bag.rows[..cancel.unwrap_or(bag.rows.len())];
        let items: Option<Vec<(String, u16)>> = rows
            .iter()
            .map(|(name, count)| {
                let item = self.data.item_named(name)?.to_owned();
                Some((item, count.unwrap_or(1)))
            })
            .collect();
        let Some(items) = items else { return };
        let whole = cancel.is_some() && bag.rows.len() < BAG_ROWS;
        events.push(if whole {
            GameEvent::PocketObserved { pocket, items }
        } else {
            GameEvent::PocketRowsObserved { pocket, items }
        });
    }

    /// Named rows of the numerical Pokédex list: seen, and caught when
    /// marked (an unmarked row is not evidence of "not caught": the marks
    /// are drawn after the names).
    fn pokedex_rows(&self, list: &PokedexListObservation) -> Vec<GameEvent> {
        list.rows
            .iter()
            .filter(|(name, _)| !name.trim().chars().all(|c| c == '-' || c == '?'))
            .filter_map(|(name, caught)| {
                let species = self.data.species_named(name)?.to_owned();
                Some(if *caught {
                    GameEvent::SpeciesCaught { species }
                } else {
                    GameEvent::SpeciesSeen { species }
                })
            })
            .collect()
    }

    /// What the screen shows now. Text still printing keeps the last page.
    fn view_of(&self, o: &Observation, state: &GameState) -> ViewState {
        let text = match &o.dialogue {
            None => None,
            Some(d) if d.ready_for_a() || d.stable_frames >= PAGE_PRINTED_FRAMES => {
                Some(d.lines.clone()).filter(|l| l.iter().any(|l| !l.trim().is_empty()))
            }
            Some(_) => state.view.text.clone(),
        };
        let menu = o.menu.as_ref().map(|m| MenuView {
            rows: o.menu_lines.clone(),
            cursor: m.cursor_row,
        });
        let opponent = o.battle.as_ref().and_then(|b| {
            let name = b.opponent_name.clone()?;
            Some(OpponentView {
                species: self.data.species_named(&name).map(str::to_owned),
                name,
                level: b.opponent_level,
                hp: b.opponent_hp,
                caught: b.opponent_caught,
                shiny: b.opponent_shiny,
            })
        });
        ViewState {
            text,
            menu,
            opponent,
            npcs: self.field.visible(),
            map_popup: o.map_popup.clone(),
        }
    }
}

/// `PartyObserved` for `slot` with the fields `fill` sets.
fn party_observed(slot: u8, fill: impl FnOnce(&mut Fields)) -> GameEvent {
    let mut e = Fields::default();
    fill(&mut e);
    GameEvent::PartyObserved {
        slot,
        species: e.species,
        nickname: e.nickname,
        level: e.level,
        hp: e.hp,
        status: e.status,
        held_item: e.held_item,
    }
}

#[derive(Default)]
struct Fields {
    species: Option<String>,
    nickname: Option<String>,
    level: Option<u8>,
    hp: Option<(u16, u16)>,
    status: Option<Status>,
    held_item: Option<Option<String>>,
}

/// One party menu panel as facts, checked the way the party audit checks
/// the summary: a readable nickname, HP within a plausible maximum for the
/// level (at least level + 10, or 1 for Shedinja), a status the HP agrees
/// with. `None` when nothing on the panel passes.
fn party_row_event(slot: u8, row: &PartyRowObservation, state: &GameState) -> Option<GameEvent> {
    let nickname = row
        .nickname
        .clone()
        .filter(|n| !n.trim().is_empty() && !n.contains('?'));
    let level = row.level.or_else(|| {
        state
            .party
            .value
            .as_ref()?
            .get(usize::from(slot))?
            .level
            .value
    });
    let hp = row.hp.filter(|&(cur, max)| {
        cur <= max && max > 0 && level.is_none_or(|l| max >= u16::from(l) + 10 || max == 1)
    });
    let status = match (row.status, hp) {
        (_, Some((0, _))) => Some(Status::Fainted),
        // A FNT icon over HP left is a misreading.
        (Some(Status::Fainted), Some(_)) => None,
        (status, _) => status,
    };
    if nickname.is_none() && row.level.is_none() && hp.is_none() && status.is_none() {
        return None;
    }
    Some(party_observed(slot, |e| {
        e.nickname = nickname;
        e.level = row.level;
        e.hp = hp;
        e.status = status;
    }))
}

fn member_hp(state: &GameState, slot: u8) -> Option<(u16, u16)> {
    state.party.value.as_ref()?.get(usize::from(slot))?.hp.value
}

/// "BULBASAUR was poisoned!" → (`BULBASAUR`, Poisoned); not for "Foe …"
/// or "Wild …".
fn status_text(page: &str) -> Option<(&str, Status)> {
    const CHANGES: [(&str, Status); 12] = [
        (" woke up", Status::Healthy),
        (" was defrosted", Status::Healthy),
        (" thawed out", Status::Healthy),
        (" was cured", Status::Healthy),
        (" is paralyzed", Status::Paralyzed),
        (" was paralyzed", Status::Paralyzed),
        (" is badly poisoned", Status::BadlyPoisoned),
        (" was badly poisoned", Status::BadlyPoisoned),
        (" was poisoned", Status::Poisoned),
        (" fell asleep", Status::Asleep),
        (" was burned", Status::Burned),
        (" was frozen", Status::Frozen),
    ];
    let (at, status) = CHANGES
        .iter()
        .find_map(|(text, status)| page.find(text).map(|at| (at, *status)))?;
    let mut words = page[..at].rsplit(' ');
    let name = words.next()?;
    let foe = words.next().is_some_and(|w| w == "Foe" || w == "Wild");
    (!foe && !name.is_empty()).then_some((name, status))
}

/// A newly obtained Pokémon, known from game data: its default moves at
/// `level`, with full PP.
pub fn obtained_mon(data: &GameData, species: &str, level: u8) -> PartyMon {
    let mut mon = PartyMon {
        species: Knowledge::derived(species.to_owned(), 0),
        level: Knowledge::derived(level, 0),
        ..PartyMon::default()
    };
    for (i, mv) in data
        .default_moves(species, level)
        .into_iter()
        .enumerate()
        .take(4)
    {
        let max = data.move_(&mv).map_or(0, |m| m.pp);
        mon.moves[i] = Some(MoveSlot {
            mv: Knowledge::derived(mv, 0),
            pp: Knowledge::derived((max, max), 0),
        });
    }
    mon
}

/// Where a caught Pokémon went: `SentToPc` when the PC pages said so or
/// the party is full, else `PartyMonDerived` into the next party slot.
/// With an unknown party and no PC page, nothing (`SpeciesCaught` was
/// emitted from the "Gotcha!" page).
fn caught_events(data: &GameData, state: &GameState, catch: &Catch) -> Vec<GameEvent> {
    let Some(level) = catch.level else {
        return Vec::new();
    };
    let party = state.party.value.as_ref().map(Vec::len);
    if catch.to_pc || party.is_some_and(|n| n >= 6) {
        let slot = catch
            .box_index
            .and_then(|i| state.pc.boxes.get(usize::from(i)))
            .and_then(|b| b.value.as_ref())
            .and_then(|list| (0..30u8).find(|s| list.iter().all(|m| m.slot != *s)))
            .unwrap_or(0);
        vec![GameEvent::SentToPc {
            box_index: catch.box_index,
            mon: BoxMon {
                slot,
                species: Knowledge::derived(catch.species.clone(), 0),
                level: Knowledge::derived(level, 0),
                nickname: Knowledge::unknown(),
            },
        }]
    } else if let Some(n) = party {
        vec![GameEvent::PartyMonDerived {
            slot: n as u8,
            mon: Box::new(obtained_mon(data, &catch.species, level)),
        }]
    } else {
        Vec::new()
    }
}

/// The pocket whose title reads like `title` (`?` wildcards; unique match).
pub fn pocket_from_title(title: &str) -> Option<Pocket> {
    const POCKETS: [(Pocket, &str); 5] = [
        (Pocket::Items, "ITEMS"),
        (Pocket::KeyItems, "KEY ITEMS"),
        (Pocket::PokeBalls, "POKé BALLS"),
        (Pocket::TmCase, "TM CASE"),
        (Pocket::BerryPouch, "BERRY POUCH"),
    ];
    let mut found = POCKETS.iter().filter(|(_, name)| names::fits(name, title));
    let (pocket, _) = found.next()?;
    found.next().is_none().then_some(*pocket)
}

/// Decomp flags of the eight badges, in gym order.
pub const BADGE_FLAGS: [&str; 8] = [
    "FLAG_BADGE01_GET",
    "FLAG_BADGE02_GET",
    "FLAG_BADGE03_GET",
    "FLAG_BADGE04_GET",
    "FLAG_BADGE05_GET",
    "FLAG_BADGE06_GET",
    "FLAG_BADGE07_GET",
    "FLAG_BADGE08_GET",
];

/// What the Trainer Card's front establishes: every badge flag (drawn →
/// true, not drawn → false), the caught count when its POKéDEX row read
/// (the card prints only that one; `seen` stays unknown), and money.
pub fn trainer_card_events(card: &TrainerCardObservation) -> Vec<GameEvent> {
    let mut events: Vec<GameEvent> = BADGE_FLAGS
        .iter()
        .enumerate()
        .map(|(i, flag)| GameEvent::FlagObserved {
            flag: (*flag).to_owned(),
            value: card.badges.contains(&(i as u8 + 1)),
        })
        .collect();
    events.extend(
        card.pokedex_count
            .map(|caught| GameEvent::PokedexCountObserved { seen: None, caught }),
    );
    events.extend(card.money.map(|amount| GameEvent::MoneyObserved { amount }));
    events
}

#[cfg(test)]
mod tests;
