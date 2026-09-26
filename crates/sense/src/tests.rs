use std::path::Path;

use pokebot_state::{
    diff, BattleObservation, DefaultReducer, DialogueKind, DialogueObservation, Direction,
    EventRecord, FrameMetrics, Knowledge, Observed, PartyMon, PartyRowObservation, Region,
    StateChange, StateReducer,
};

use super::*;

fn data() -> Option<Arc<GameData>> {
    GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
        .ok()
        .map(Arc::new)
}

fn bare(frame: u64, screen: ScreenState) -> Observation {
    Observation::bare(
        frame,
        Observed {
            value: screen,
            detector: "test".into(),
        },
        FrameMetrics::default(),
    )
}

fn page(frame: u64, text: &str) -> Observation {
    let mut o = bare(frame, ScreenState::Dialogue);
    o.dialogue = Some(DialogueObservation {
        kind: DialogueKind::MessageBox,
        region: Region::new(8, 118, 224, 36),
        waiting_for_input: true,
        arrow: None,
        stable_frames: 20,
        text_cells: Vec::new(),
        lines: text.split('\n').map(str::to_owned).collect(),
        help: false,
    });
    o
}

fn hud(frame: u64, name: &str, level: u8, hp: (u16, u16)) -> Observation {
    let mut o = bare(frame, ScreenState::BattleText);
    o.battle = Some(BattleObservation {
        menu: None,
        player_name: Some(name.into()),
        player_level: Some(level),
        player_hp_numbers: Some(hp),
        opponent_name: Some("PIDGEY".into()),
        opponent_level: Some(3),
        player_hp: Some(500),
        opponent_hp: Some(1000),
        move_pp: None,
        move_names: Vec::new(),
        opponent_caught: Some(false),
        opponent_shiny: None,
    });
    o
}

fn with_party() -> GameState {
    let mon = PartyMon {
        species: Knowledge::observed("SPECIES_BULBASAUR".into(), 1),
        nickname: Knowledge::observed("BULBASAUR".into(), 1),
        level: Knowledge::observed(11, 1),
        hp: Knowledge::observed((30, 30), 1),
        ..PartyMon::default()
    };
    let other = PartyMon {
        species: Knowledge::observed("SPECIES_PIDGEY".into(), 1),
        nickname: Knowledge::observed("BIRDY".into(), 1),
        level: Knowledge::observed(5, 1),
        hp: Knowledge::observed((19, 19), 1),
        ..PartyMon::default()
    };
    GameState {
        party: Knowledge::observed(vec![mon, other], 1),
        ..GameState::default()
    }
}

/// Runs observations through sensor and reducer; returns the state and
/// every change, in order.
fn run(
    sensor: &mut Sensor,
    mut state: GameState,
    frames: impl IntoIterator<Item = Observation>,
) -> (GameState, Vec<StateChange>) {
    let mut changes = Vec::new();
    for o in frames {
        let events: Vec<EventRecord> = sensor
            .observe(&o, &state)
            .into_iter()
            .map(|event| EventRecord {
                frame_id: o.frame_id,
                event,
            })
            .collect();
        let next = DefaultReducer.reduce(&state, &events);
        changes.extend(diff(&state, &next));
        state = next;
    }
    (state, changes)
}

#[test]
fn a_level_up_page_updates_the_named_member_once() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (10..40).map(|f| page(f, "BIRDY grew to\nLv6!"));
    let (state, changes) = run(&mut s, with_party(), frames);
    assert_eq!(state.party.value.unwrap()[1].level.value, Some(6));
    assert_eq!(
        changes
            .iter()
            .filter(|c| matches!(c, StateChange::PartyLevelChanged { .. }))
            .count(),
        1
    );
}

#[test]
fn a_found_item_counts_once_per_box() {
    let Some(d) = data() else { return };
    let mut state = with_party();
    state.bag.pockets.insert(
        Pocket::Items,
        Knowledge::observed(vec![("ITEM_POTION".into(), 1)], 1),
    );
    let mut s = Sensor::new(d);
    // The page stays up for a while, the box closes, and the same page
    // shows again (a second item ball).
    let frames = (10..30)
        .map(|f| page(f, "RED found a POTION!"))
        .chain((30..35).map(|f| bare(f, ScreenState::Unknown)))
        .chain((35..50).map(|f| page(f, "RED found a POTION!")));
    let (state, changes) = run(&mut s, state, frames);
    assert_eq!(
        state.bag.pockets[&Pocket::Items].value,
        Some(vec![("ITEM_POTION".into(), 3)])
    );
    assert!(changes.contains(&StateChange::ItemCountChanged {
        pocket: Pocket::Items,
        item: "ITEM_POTION".into(),
        from: Some(1),
        to: Some(2)
    }));
    assert!(changes.contains(&StateChange::TextShown {
        lines: vec!["RED found a POTION!".into()]
    }));
}

#[test]
fn hud_hp_counts_once_the_bar_stops() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    // The HP drains one point a frame, then holds.
    let frames = (0..8u16)
        .map(|i| hud(100 + u64::from(i), "BULBASAUR", 11, (30 - i, 30)))
        .chain((108..130).map(|f| hud(f, "BULBASAUR", 11, (23, 30))));
    let (state, changes) = run(&mut s, with_party(), frames);
    assert_eq!(state.party.value.unwrap()[0].hp.value, Some((23, 30)));
    let hp: Vec<&StateChange> = changes
        .iter()
        .filter(|c| matches!(c, StateChange::PartyHpChanged { .. }))
        .collect();
    assert_eq!(
        hp,
        [&StateChange::PartyHpChanged {
            slot: 0,
            from: Some((30, 30)),
            to: Some((23, 30))
        }]
    );
    assert!(state.pokedex.seen[&"SPECIES_PIDGEY".to_owned()].value == Some(true));
    assert!(changes.contains(&StateChange::OpponentAppeared {
        species: Some("SPECIES_PIDGEY".into()),
        level: Some(3)
    }));
}

#[test]
fn the_hud_shows_an_evolution_before_the_text_does() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..20).map(|f| hud(f, "IVYSAUR", 16, (40, 44)));
    let mut state = with_party();
    state.party.value.as_mut().unwrap()[0].nickname = Knowledge::unknown();
    let (state, _) = run(&mut s, state, frames);
    let lead = &state.party.value.unwrap()[0];
    assert_eq!(lead.species.value.as_deref(), Some("SPECIES_IVYSAUR"));
    assert_eq!(lead.level.value, Some(16));
}

fn summary(frame: u64, page: SummaryPage) -> Observation {
    let mut o = bare(frame, ScreenState::PartyMenu);
    o.summary = Some(SummaryObservation {
        details: Default::default(),
        page,
        nickname: "BIRDY".into(),
        level: Some(5),
        species: Some("PIDGEY".into()),
        held_item: Some("NONE".into()),
        hp: Some((12, 19)),
        status: Some(Status::Healthy),
        moves: vec![
            ("TACKLE".into(), Some((30, 35))),
            ("SAND-ATTACK".into(), Some((15, 15))),
        ],
    });
    o
}

fn party_menu(frame: u64, selected: u8) -> Observation {
    let mut o = bare(frame, ScreenState::PartyMenu);
    o.party_menu = Some(PartyMenuObservation {
        count: 2,
        selected: Some(selected),
        ..PartyMenuObservation::default()
    });
    o
}

fn row(name: &str, level: u8, hp: (u16, u16), status: Status) -> PartyRowObservation {
    PartyRowObservation {
        nickname: Some(name.into()),
        level: Some(level),
        hp: Some(hp),
        status: Some(status),
    }
}

fn party_rows(frame: u64, members: Vec<PartyRowObservation>) -> Observation {
    let mut o = party_menu(frame, 0);
    let menu = o.party_menu.as_mut().unwrap();
    menu.count = members.len() as u8;
    menu.members = members;
    o
}

/// Live (Switch, one member): "BULBASAUR Lv14 35/ 37" on the party menu
/// became the member's level and HP without opening its summary.
#[test]
fn party_menu_panels_update_each_member() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let members = vec![
        row("BULBASAUR", 12, (25, 33), Status::Healthy),
        row("BIRDY", 6, (0, 21), Status::Fainted),
    ];
    let frames = (0..20).map(|f| party_rows(f, members.clone()));
    let (state, changes) = run(&mut s, with_party(), frames);
    let party = state.party.value.unwrap();
    assert_eq!(party[0].level.value, Some(12));
    assert_eq!(party[0].hp.value, Some((25, 33)));
    assert_eq!(party[0].status.value, Some(Status::Healthy));
    assert_eq!(party[1].hp.value, Some((0, 21)));
    assert_eq!(party[1].status.value, Some(Status::Fainted));
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, StateChange::PartyLevelChanged { slot: 0, .. })),
        "{changes:?}"
    );
}

/// Readings the party audit would reject change nothing: HP above its
/// maximum, a maximum too small for the level, a name with an unread
/// glyph, and panels that don't hold still.
#[test]
fn implausible_or_unsteady_panels_are_ignored() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let bad = vec![
        PartyRowObservation {
            nickname: Some("BULB?SAUR".into()),
            level: None,
            hp: Some((40, 30)),
            status: None,
        },
        row("BIRDY", 5, (9, 9), Status::Healthy),
    ];
    let (state, _) = run(
        &mut s,
        with_party(),
        (0..20).map(|f| party_rows(f, bad.clone())),
    );
    let party = state.party.value.unwrap();
    assert_eq!(party[0].nickname.value.as_deref(), Some("BULBASAUR"));
    assert_eq!(party[0].hp.value, Some((30, 30)));
    assert_eq!(party[1].hp.value, Some((19, 19)));
    // Flickering HP: never the same for long enough.
    let mut s = Sensor::new(data().unwrap());
    let frames = (0..40).map(|f| {
        let hp = if f % 2 == 0 { (20, 30) } else { (26, 30) };
        party_rows(f, vec![row("BULBASAUR", 11, hp, Status::Healthy)])
    });
    let (state, _) = run(&mut s, with_party(), frames);
    assert_eq!(state.party.value.unwrap()[0].hp.value, Some((30, 30)));
}

#[test]
fn summary_pages_fill_in_the_member() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..20)
        .map(|f| party_menu(f, 1))
        .chain((20..40).map(|f| summary(f, SummaryPage::Skills)))
        .chain((40..60).map(|f| summary(f, SummaryPage::Moves)));
    let (state, changes) = run(&mut s, with_party(), frames);
    let m = &state.party.value.unwrap()[1];
    assert_eq!(m.hp.value, Some((12, 19)));
    assert_eq!(
        m.moves[0].as_ref().unwrap().pp.value,
        Some((30, 35)),
        "{changes:?}"
    );
    assert_eq!(
        m.moves[1].as_ref().unwrap().mv.value.as_deref(),
        Some("MOVE_SAND_ATTACK")
    );
}

#[test]
fn a_summary_outside_the_party_screens_is_not_a_member() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let mut o = summary(0, SummaryPage::Skills);
    o.player = None;
    let mut overworld = bare(0, ScreenState::Unknown);
    overworld.player = Some(pokebot_state::PoseObservation {
        pose: pokebot_state::PlayerPose {
            map: "PalletTown".into(),
            x: 1,
            y: 1,
        },
        score: 900,
    });
    let frames = std::iter::once(overworld).chain((1..40).map(|f| summary(f, SummaryPage::Skills)));
    let (state, _) = run(&mut s, with_party(), frames);
    assert_eq!(state.party.value.unwrap()[1].hp.value, Some((19, 19)));
}

fn bag(frame: u64, rows: &[(&str, Option<u16>)]) -> Observation {
    let mut o = bare(frame, ScreenState::Bag);
    o.bag = Some(BagObservation {
        pocket: "ITEMS".into(),
        rows: rows.iter().map(|(n, c)| ((*n).to_owned(), *c)).collect(),
        cursor: Some(0),
        prompt: None,
    });
    o
}

#[test]
fn a_bag_pocket_is_whole_only_when_it_ends_before_the_list_fills() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let short = [("POTION", Some(3)), ("ANTIDOTE", Some(1)), ("CANCEL", None)];
    let (state, _) = run(
        &mut s,
        GameState::default(),
        (0..10).map(|f| bag(f, &short)),
    );
    let items = &state.bag.pockets[&Pocket::Items];
    assert_eq!(items.source, pokebot_state::KnowledgeSource::Observed);
    assert_eq!(
        items.value,
        Some(vec![("ITEM_POTION".into(), 3), ("ITEM_ANTIDOTE".into(), 1)])
    );
    // Six rows: maybe scrolled, so only the rows seen are known.
    let long = [
        ("POTION", Some(2)),
        ("ANTIDOTE", Some(1)),
        ("PARLYZ HEAL", Some(1)),
        ("AWAKENING", Some(1)),
        ("BURN HEAL", Some(1)),
        ("ICE HEAL", Some(1)),
    ];
    let mut s = Sensor::new(data().unwrap());
    let (state, changes) = run(&mut s, state, (10..20).map(|f| bag(f, &long)));
    let items = &state.bag.pockets[&Pocket::Items];
    assert_eq!(items.value.as_ref().unwrap().len(), 6);
    assert!(changes.contains(&StateChange::ItemCountChanged {
        pocket: Pocket::Items,
        item: "ITEM_POTION".into(),
        from: Some(3),
        to: Some(2)
    }));
}

#[test]
fn transitions_are_ignored() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..20).map(|f| {
        let mut o = page(f, "RED found a POTION!");
        o.screen.value = ScreenState::Transition;
        o
    });
    let (_, changes) = run(&mut s, with_party(), frames);
    assert!(changes.is_empty());
}

fn located(frame: u64) -> Observation {
    let mut o = bare(frame, ScreenState::Unknown);
    o.player = Some(pokebot_state::PoseObservation {
        pose: pokebot_state::PlayerPose {
            map: "Route22".into(),
            x: 5,
            y: 5,
        },
        score: 900,
    });
    o
}

/// A wild PIDGEY Lv6 on the HUD, caught, and the pages that follow.
fn catch_frames(pages: &[&str]) -> Vec<Observation> {
    let mut frames: Vec<Observation> = (0..20).map(|f| hud(f, "BULBASAUR", 11, (30, 30))).collect();
    for o in &mut frames {
        o.battle.as_mut().unwrap().opponent_level = Some(6);
    }
    let mut f = 20;
    for text in pages {
        for _ in 0..3 {
            let mut o = page(f, text);
            o.screen.value = ScreenState::BattleText;
            o.dialogue.as_mut().unwrap().kind = DialogueKind::BattleText;
            frames.push(o);
            f += 1;
        }
    }
    frames.extend((f..f + 3).map(located));
    frames
}

#[test]
fn a_catch_joins_the_party_when_the_battle_is_over() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let (state, changes) = run(
        &mut s,
        with_party(),
        catch_frames(&[
            "Gotcha! PIDGEY was caught!",
            "Give a nickname to the captured PIDGEY?",
        ]),
    );
    let party = state.party.value.unwrap();
    assert_eq!(party.len(), 3);
    assert_eq!(party[2].species.value.as_deref(), Some("SPECIES_PIDGEY"));
    assert_eq!(party[2].level.value, Some(6));
    assert!(party[2].moves[0].is_some(), "default moves");
    assert!(changes.contains(&StateChange::PokedexCaught {
        species: "SPECIES_PIDGEY".into()
    }));
    assert!(changes.contains(&StateChange::PartyMonAdded {
        slot: 2,
        species: Some("SPECIES_PIDGEY".into())
    }));
}

#[test]
fn a_catch_the_pc_pages_name_goes_to_that_box() {
    let Some(d) = data() else { return };
    let mut state = with_party();
    state.pc.boxes[1] = Knowledge::observed(Vec::new(), 1);
    let mut s = Sensor::new(d);
    let (state, changes) = run(
        &mut s,
        state,
        catch_frames(&[
            "Gotcha! PIDGEY was caught!",
            "PIDGEY was transferred to someone's PC.",
            "It was placed in BOX “BOX2.”",
        ]),
    );
    assert_eq!(state.party.value.unwrap().len(), 2, "not in the party");
    assert_eq!(state.pc.boxes[1].value.as_ref().unwrap().len(), 1);
    assert!(changes.iter().any(|c| matches!(
        c,
        StateChange::PcBoxChanged { box_index: 1, added, .. } if added == &[(0, Some("SPECIES_PIDGEY".into()))]
    )));
}

#[test]
fn a_full_party_sends_the_catch_to_the_pc() {
    let Some(d) = data() else { return };
    let mut state = with_party();
    let lead = state.party.value.as_ref().unwrap()[1].clone();
    state.party.value.as_mut().unwrap().resize(6, lead);
    state.pc.boxes[0] = Knowledge::observed(Vec::new(), 1);
    let mut s = Sensor::new(d);
    // The box page went unread: still the PC, never a 7th slot.
    let (state, _) = run(&mut s, state, catch_frames(&["Gotcha! PIDGEY was caught!"]));
    assert_eq!(state.party.value.unwrap().len(), 6);
}

#[test]
fn the_known_moves_list_is_the_learning_members() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let list = |f: u64| {
        let mut o = bare(f, ScreenState::LearnMove);
        o.move_list = Some(pokebot_state::MoveListObservation {
            moves: ["TACKLE", "GUST", "SAND-ATTACK", "QUICK ATTACK", "WHIRLWIND"]
                .map(String::from)
                .to_vec(),
            selected: Some(0),
        });
        o
    };
    let frames = (0..5)
        .map(|f| page(f, "BIRDY is trying to\nlearn WHIRLWIND."))
        .chain((5..20).map(list));
    let (state, _) = run(&mut s, with_party(), frames);
    let moves: Vec<Option<String>> = state.party.value.unwrap()[1]
        .moves
        .iter()
        .map(|m| m.as_ref().and_then(|m| m.mv.value.clone()))
        .collect();
    assert_eq!(
        moves,
        [
            "MOVE_TACKLE",
            "MOVE_GUST",
            "MOVE_SAND_ATTACK",
            "MOVE_QUICK_ATTACK"
        ]
        .map(|m| Some(m.to_owned()))
        .to_vec()
    );
}

/// A page as the battle text box reads it, on `screen`.
fn battle_page(frame: u64, screen: ScreenState, text: &str) -> Observation {
    let mut o = page(frame, text);
    o.screen.value = screen;
    if let Some(d) = &mut o.dialogue {
        d.kind = DialogueKind::BattleText;
    }
    o
}

/// Switch goal run (frame 87910, and 87930 beside the level-up stats
/// window): the battle box reads the page as two lines, "LV." spelt out.
#[test]
fn a_battle_level_up_page_as_read_updates_the_member() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames =
        (10..30).map(|f| battle_page(f, ScreenState::BattleText, "BULBASAUR grew to\nLV. 15!"));
    let (state, _) = run(&mut s, with_party(), frames);
    assert_eq!(state.party.value.unwrap()[0].level.value, Some(15));
}

/// The evolution scene is its own screen now; its closing page still
/// evolves the member (switch-goal-15 frame 128470).
#[test]
fn the_evolution_page_on_the_evolution_screen_evolves_the_member() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (10..30).map(|f| {
        battle_page(
            f,
            ScreenState::Evolution,
            "Congratulations! Your BULBASAUR\nevolved into IVYSAUR!",
        )
    });
    let (state, _) = run(&mut s, with_party(), frames);
    let lead = &state.party.value.unwrap()[0];
    assert_eq!(lead.species.value.as_deref(), Some("SPECIES_IVYSAUR"));
}
/// Entering Route 3 from Pewter: the popup's name is in the view while the
/// box is down and comes out as one `MapNameShown`; the popup sliding away
/// clears the view without another change.
#[test]
fn a_map_popup_is_shown_once() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..40).map(|f| {
        let mut o = bare(f, ScreenState::Unknown);
        o.map_popup = (5..30).contains(&f).then(|| "ROUTE 3".to_owned());
        o
    });
    let mut state = GameState::default();
    let mut changes = Vec::new();
    let mut shown = false;
    for o in frames {
        let (next, c) = run(&mut s, state, [o]);
        shown |= next.view.map_popup.as_deref() == Some("ROUTE 3");
        state = next;
        changes.extend(c);
    }
    assert!(shown);
    assert_eq!(state.view.map_popup, None);
    let named: Vec<_> = changes
        .iter()
        .filter(|c| matches!(c, StateChange::MapNameShown { .. }))
        .collect();
    assert_eq!(
        named,
        [&StateChange::MapNameShown {
            name: "ROUTE 3".into()
        }]
    );
}

/// A located field frame on Pewter City with the player at `at`, showing
/// `sprites` (x, y, local id) and the objects perception saw absent.
fn field(
    frame: u64,
    at: (i32, i32),
    sprites: &[(i32, i32, Option<u32>)],
    absent: &[u32],
) -> Observation {
    let mut o = bare(frame, ScreenState::Unknown);
    o.player = Some(pokebot_state::PoseObservation {
        pose: pokebot_state::PlayerPose {
            map: "PewterCity".into(),
            x: at.0,
            y: at.1,
        },
        score: 990,
    });
    o.sprites = sprites
        .iter()
        .map(|&(x, y, local_id)| pokebot_state::SpriteObservation {
            x,
            y,
            local_id,
            facing: None,
        })
        .collect();
    o.objects_absent = absent.to_vec();
    o
}

fn npc_changes(changes: &[StateChange]) -> Vec<&StateChange> {
    changes
        .iter()
        .filter(|c| {
            matches!(
                c,
                StateChange::SpriteAppeared { .. }
                    | StateChange::SpriteMoved { .. }
                    | StateChange::SpriteLeft { .. }
                    | StateChange::NpcAppeared { .. }
                    | StateChange::NpcMoved { .. }
                    | StateChange::NpcGone { .. }
            )
        })
        .collect()
}

#[test]
fn a_standing_npc_is_seen_once() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..60).map(|f| field(f, (20, 20), &[(22, 20, Some(3))], &[]));
    let (state, changes) = run(&mut s, GameState::default(), frames);
    let npc = state.world.npc("PewterCity", 3).unwrap();
    assert_eq!(npc.pos.value, Some((22, 20)));
    assert_eq!(npc.present.value, Some(true));
    assert_eq!(state.view.npcs.len(), 1);
    let changes = npc_changes(&changes);
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert!(changes
        .iter()
        .any(|c| matches!(c, StateChange::NpcAppeared { at: (22, 20), .. })));
    assert!(changes
        .iter()
        .any(|c| matches!(c, StateChange::SpriteAppeared { .. })));
}

/// A step takes 16 frames with the sprite on neither tile (perception only
/// reports tile-aligned sprites): the NPC moves, it doesn't leave and
/// come back.
#[test]
fn a_step_is_a_move() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..40)
        .map(|f| field(f, (20, 20), &[(22, 20, Some(3))], &[]))
        .chain((40..56).map(|f| field(f, (20, 20), &[], &[])))
        .chain((56..90).map(|f| field(f, (20, 20), &[(23, 20, Some(3))], &[])));
    let (state, changes) = run(&mut s, GameState::default(), frames);
    assert_eq!(
        state.world.npc("PewterCity", 3).unwrap().pos.value,
        Some((23, 20))
    );
    let changes = npc_changes(&changes);
    assert!(changes.iter().any(|c| matches!(
        c,
        StateChange::NpcMoved {
            from: (22, 20),
            to: (23, 20),
            ..
        }
    )));
    assert!(changes
        .iter()
        .any(|c| matches!(c, StateChange::SpriteMoved { .. })));
    assert!(!changes.iter().any(|c| matches!(
        c,
        StateChange::SpriteLeft { .. } | StateChange::NpcGone { .. }
    )));
}

/// Walking frames aren't located and say nothing about the field; a
/// sprite the camera scrolled away from leaves the view at once.
#[test]
fn unlocated_frames_keep_the_view_and_scrolling_drops_sprites() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..30)
        .map(|f| field(f, (20, 20), &[(22, 20, Some(3))], &[]))
        .chain((30..100).map(|f| bare(f, ScreenState::Unknown)));
    let (state, _) = run(&mut s, GameState::default(), frames);
    assert_eq!(state.view.npcs.len(), 1);
    let frames = (100..110).map(|f| field(f, (40, 20), &[], &[]));
    let (state, changes) = run(&mut s, state, frames);
    assert!(state.view.npcs.is_empty());
    assert_eq!(npc_changes(&changes).len(), 1, "{changes:?}");
    // What the belief knows stays.
    assert_eq!(
        state.world.npc("PewterCity", 3).unwrap().pos.value,
        Some((22, 20))
    );
}

/// A sprite no object can be takes longer to count (flowers animate for
/// 64 of their 80 frames before perception knows).
#[test]
fn an_unnamed_sprite_counts_later() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..60).map(|f| field(f, (20, 20), &[(22, 21, None)], &[]));
    let (state, _) = run(&mut s, GameState::default(), frames);
    assert!(state.view.npcs.is_empty());
    let frames = (60..100).map(|f| field(f, (20, 20), &[(22, 21, None)], &[]));
    let (state, _) = run(&mut s, state, frames);
    assert_eq!(state.view.npcs.len(), 1);
    assert_eq!(state.view.npcs[0].local_id, None);
}

/// An empty reach counts after a second of steady sightings, once; a
/// sighting in between starts over.
#[test]
fn an_npc_is_absent_after_its_reach_stays_empty() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..50)
        .map(|f| field(f, (20, 20), &[], &[5]))
        .chain(std::iter::once(field(50, (20, 20), &[], &[])))
        .chain((51..100).map(|f| field(f, (20, 20), &[], &[5])));
    let (state, changes) = run(&mut s, GameState::default(), frames);
    assert!(state.world.npc("PewterCity", 5).is_none(), "{changes:?}");
    let frames = (100..200).map(|f| field(f, (20, 20), &[], &[5]));
    let (state, changes) = run(&mut s, state, frames);
    assert_eq!(
        state.world.npc("PewterCity", 5).unwrap().present.value,
        Some(false)
    );
    assert_eq!(npc_changes(&changes).len(), 1);
    assert!(matches!(
        npc_changes(&changes)[0],
        StateChange::NpcGone { local_id: 5, .. }
    ));
}

/// A battle hides the field; the NPC seen again after it is where the
/// belief already has it (only the view changes).
#[test]
fn a_battle_clears_the_view_not_the_belief() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let frames = (0..30)
        .map(|f| field(f, (20, 20), &[(22, 20, Some(3))], &[]))
        .chain((30..60).map(|f| hud(f, "BULBASAUR", 11, (30, 30))))
        .chain((60..90).map(|f| field(f, (20, 20), &[(22, 20, Some(3))], &[])));
    let (_, changes) = run(&mut s, with_party(), frames);
    let changes = npc_changes(&changes);
    let count = |f: fn(&&&StateChange) -> bool| changes.iter().filter(f).count();
    assert_eq!(count(|c| matches!(c, StateChange::NpcAppeared { .. })), 1);
    assert_eq!(
        count(|c| matches!(c, StateChange::SpriteAppeared { .. })),
        2
    );
    assert_eq!(count(|c| matches!(c, StateChange::SpriteLeft { .. })), 1);
}

/// A facing counts once it holds; a one-frame misreading doesn't turn the
/// NPC, a lasting one does.
#[test]
fn an_npc_turns_when_its_facing_holds() {
    let Some(d) = data() else { return };
    let mut s = Sensor::new(d);
    let facing = |f: u64, facing| {
        let mut o = field(f, (20, 20), &[(22, 20, Some(3))], &[]);
        o.sprites[0].facing = facing;
        o
    };
    let frames = (0..40)
        .map(|f| facing(f, Some(Direction::Left)))
        .chain(std::iter::once(facing(40, Some(Direction::Right))))
        .chain((41..60).map(|f| facing(f, None)))
        .chain((60..90).map(|f| facing(f, Some(Direction::Down))));
    let (state, changes) = run(&mut s, GameState::default(), frames);
    let turns: Vec<_> = changes
        .iter()
        .filter_map(|c| match c {
            StateChange::NpcTurned { to, .. } => Some(*to),
            _ => None,
        })
        .collect();
    assert_eq!(turns, [Some(Direction::Left), Some(Direction::Down)]);
    assert_eq!(state.view.npcs[0].facing, Some(Direction::Down));
}

fn world() -> Option<Arc<pokebot_world::World>> {
    pokebot_world::World::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world"))
        .ok()
        .map(Arc::new)
}

fn pose(map: &str, x: i32, y: i32) -> pokebot_state::PlayerPose {
    pokebot_state::PlayerPose {
        map: map.into(),
        x,
        y,
    }
}

/// A frame every Center matches: Pewter's and Viridian's, tile (7, 4).
fn lookalike(frame: u64) -> Observation {
    let mut o = bare(frame, ScreenState::Unknown);
    o.pose_candidates = [
        "PewterCity_PokemonCenter_1F",
        "ViridianCity_PokemonCenter_1F",
    ]
    .map(|map| pokebot_state::PoseObservation {
        pose: pose(map, 7, 4),
        score: 990,
    })
    .to_vec();
    o
}

fn committed(at: Option<pokebot_state::PlayerPose>) -> GameState {
    let mut state = with_party();
    state.player.pose = at.map_or(Knowledge::unknown(), |p| Knowledge::observed(p, 1));
    state
}

#[test]
fn lookalike_maps_are_told_apart_by_the_committed_pose() {
    let (Some(d), Some(w)) = (data(), world()) else {
        return;
    };
    // Still in the same Center (after a fade or a battle).
    let mut s = Sensor::new(Arc::clone(&d)).with_world(Arc::clone(&w));
    let state = committed(Some(pose("PewterCity_PokemonCenter_1F", 7, 6)));
    let (state, _) = run(&mut s, state, [lookalike(10)]);
    assert_eq!(
        state.player.pose.value,
        Some(pose("PewterCity_PokemonCenter_1F", 7, 4))
    );
    assert_eq!(
        state.player.pose.source,
        pokebot_state::KnowledgeSource::Derived
    );
    assert!(state.player.candidates.is_empty());
    // Walked in from the town: its warps lead to its own Center only.
    let mut s = Sensor::new(Arc::clone(&d)).with_world(Arc::clone(&w));
    let state = committed(Some(pose("ViridianCity", 26, 27)));
    let (state, _) = run(&mut s, state, [lookalike(10)]);
    assert_eq!(
        state.player.pose.value,
        Some(pose("ViridianCity_PokemonCenter_1F", 7, 4))
    );
    // Nothing committed: the respawn point (a white-out).
    let mut s = Sensor::new(Arc::clone(&d)).with_world(Arc::clone(&w));
    let mut state = committed(None);
    // As the nurse's script records it: the spot outside the Center.
    state.world.respawn = Knowledge::observed(
        pokebot_state::HealSpot {
            map: "PewterCity".into(),
            x: 13,
            y: 26,
        },
        1,
    );
    let (state, _) = run(&mut s, state, [lookalike(10)]);
    assert_eq!(
        state.player.pose.value,
        Some(pose("PewterCity_PokemonCenter_1F", 7, 4))
    );
}

#[test]
fn lookalike_maps_nothing_points_at_are_announced_once() {
    let (Some(d), Some(w)) = (data(), world()) else {
        return;
    };
    let mut s = Sensor::new(d).with_world(w);
    let state = committed(Some(pose("Route22", 5, 5)));
    let (state, changes) = run(&mut s, state, [lookalike(10), lookalike(40), lookalike(70)]);
    assert_eq!(state.player.candidates.len(), 2);
    assert_eq!(
        state.player.pose.source,
        pokebot_state::KnowledgeSource::Assumed
    );
    let announced = changes
        .iter()
        .filter(|c| matches!(c, StateChange::LocationAmbiguous { .. }))
        .count();
    assert_eq!(announced, 1, "{changes:?}");
    // Walking out names the map: confirmed.
    let mut outside = bare(100, ScreenState::Unknown);
    outside.player = Some(pokebot_state::PoseObservation {
        pose: pose("PewterCity", 13, 26),
        score: 995,
    });
    let mut extractor = pokebot_state::EventExtractor::new(1);
    let at = pokebot_state::FrameArrival {
        frame_id: 100,
        delivered: 100,
        captured_at: std::time::Instant::now(),
    };
    let events: Vec<EventRecord> = extractor.observe(&outside, at);
    let after = DefaultReducer.reduce(&state, &events);
    assert!(after.player.candidates.is_empty());
    assert!(diff(&state, &after).iter().any(|c| matches!(
        c,
        StateChange::LocationConfirmed { pose: Some(p) } if p.map == "PewterCity"
    )));
}

#[test]
fn summary_details_are_confirmed_and_become_state_changes_and_history() {
    let Some(data) = data() else { return };
    let mut sensor = Sensor::new(data);
    let frames = (0..20).map(|f| party_menu(f, 1)).chain((20..70).map(|f| {
        let mut o = summary(f, SummaryPage::Skills);
        o.summary.as_mut().unwrap().details = pokebot_state::SummaryDetails {
            attack: Some(if f < 45 { 10 } else { 12 }),
            ability: Some("KEEN EYE".into()),
            exp_points: Some(100),
            next_level: Some(25),
            ..Default::default()
        };
        o
    }));
    let (state, changes) = run(&mut sensor, with_party(), frames);
    let m = &state.party.value.as_ref().unwrap()[1];
    assert_eq!(m.details.attack.value, Some(12));
    assert_eq!(
        m.history.len(),
        5,
        "repeated frames must not duplicate history"
    );
    assert_eq!(m.history.last().unwrap().from.as_deref(), Some("10"));
    assert_eq!(
        changes
            .iter()
            .filter(
                |c| matches!(c, StateChange::PartyDetailChanged { field, .. } if field == "attack")
            )
            .count(),
        2
    );
    assert!(state.party.value.as_ref().unwrap()[0].history.is_empty());
}

#[test]
fn save_badge_count_requires_stable_frames_and_card_overrides_assumptions() {
    let Some(data) = data() else { return };
    let mut sensor = Sensor::new(data);
    let frames = (0..20).map(|f| {
        let mut o = bare(f, ScreenState::Dialogue);
        o.save_badge_count = Some(if f == 0 { 8 } else { 7 });
        o
    });
    let (state, _) = run(&mut sensor, GameState::default(), frames);
    assert_eq!(state.progression.badge_count.value, Some(7));
    assert_eq!(
        state.progression.badge_inference.source,
        pokebot_state::KnowledgeSource::Assumed
    );
    assert!(state.world.flags.is_empty());
    let frames = (20..40).map(|f| {
        let mut o = bare(f, ScreenState::Unknown);
        o.trainer_card = Some(TrainerCardObservation {
            badges: vec![1, 3],
            pokedex_count: None,
            money: None,
        });
        o
    });
    let (state, _) = run(&mut sensor, state, frames);
    assert_eq!(
        state.progression.badges.value.unwrap(),
        vec!["BOULDERBADGE", "THUNDERBADGE"]
    );
    assert_eq!(state.progression.badge_count.value, Some(2));
    assert!(state.progression.badge_inference.value.is_none());
}

#[test]
fn visible_party_reordering_keeps_the_history_with_the_named_member() {
    let Some(data) = data() else { return };
    let mut sensor = Sensor::new(data);
    let mut state = with_party();
    let party = state.party.value.as_mut().unwrap();
    party[0].observe_details(
        &pokebot_state::SummaryDetails {
            attack: Some(20),
            ..Default::default()
        },
        1,
    );
    party[1].observe_details(
        &pokebot_state::SummaryDetails {
            attack: Some(10),
            ..Default::default()
        },
        1,
    );
    let frames = (10..35).map(|f| {
        let mut o = party_menu(f, 0);
        o.party_menu.as_mut().unwrap().members = vec![
            row("BIRDY", 5, (19, 19), Status::Healthy),
            row("BULBASAUR", 11, (30, 30), Status::Healthy),
        ];
        o
    });
    let (state, _) = run(&mut sensor, state, frames);
    let party = state.party.value.unwrap();
    assert_eq!(party[0].nickname.value.as_deref(), Some("BIRDY"));
    assert_eq!(party[0].details.attack.value, Some(10));
    assert_eq!(party[0].history[0].to, "10");
    assert_eq!(party[1].nickname.value.as_deref(), Some("BULBASAUR"));
    assert_eq!(party[1].details.attack.value, Some(20));
}

/// The Switch run recorded Bill's PC's Cell Separator path when the PC
/// only showed its idle message: the belief had Bill back as himself
/// (his hide flag tracked clear) and Bill helped. Bill staying away from
/// every tile he can stand on retracts the path, also when that belief
/// arrives with a checkpoint after his absence was first seen.
#[test]
fn an_absent_object_retracts_the_path_that_showed_it() {
    let (Some(d), Some(w)) = (data(), world()) else {
        return;
    };
    if w.events().is_none() {
        return;
    }
    let pc = "Route25_SeaCottage_EventScript_Computer";
    let mut s = Sensor::new(d).with_world(w);
    let frame = |f: u64| {
        let mut o = field(f, (4, 6), &[], &[1]);
        o.player.as_mut().unwrap().pose.map = "Route25_SeaCottage".into();
        o
    };
    // Bill is seen gone before the belief knows anything.
    let (mut state, _) = run(&mut s, GameState::default(), (0..100).map(frame));
    assert!(state.world.flags.is_empty());
    // Then the checkpoint's word comes in.
    let flags = &mut state.world.flags;
    flags.insert(
        "FLAG_HELPED_BILL_IN_SEA_COTTAGE".into(),
        Knowledge::tracked(true, None),
    );
    flags.insert(
        "FLAG_HIDE_BILL_HUMAN_SEA_COTTAGE".into(),
        Knowledge::tracked(false, None),
    );
    // Observed elsewhere: not the path's to take away.
    flags.insert("FLAG_GOT_OAKS_PARCEL".into(), Knowledge::observed(true, 1));
    state.world.record_path(pc, 7);
    let (state, _) = run(&mut s, state, (100..110).map(frame));
    let flag = |name: &str| state.world.flags.get(name).cloned();
    assert_eq!(flag("FLAG_HELPED_BILL_IN_SEA_COTTAGE"), None);
    assert_eq!(
        flag("FLAG_HIDE_BILL_HUMAN_SEA_COTTAGE").and_then(|k| k.value),
        Some(true)
    );
    assert!(flag("FLAG_GOT_OAKS_PARCEL").is_some());
    assert!(state.world.paths_run.is_empty());
}
