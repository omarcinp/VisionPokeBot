//! Real screenshots through perception → confirmed events → persisted state.
use pokebot_core::NormalizedFrame;
use pokebot_sense::Sensor;
use pokebot_state::{DefaultReducer, EventRecord, GameState, StateReducer};
use pokebot_vision::{text::Font, FireRedPerception, PerceptionSystem};
use std::{path::Path, sync::Arc, time::Instant};

#[test]
fn visible_details_and_badges_reach_the_state_and_checkpoint() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(data) = pokebot_gamedata::GameData::load(root.join("data/world/gamedata.json")) else {
        return;
    };
    let font = Arc::new(Font::load(root.join("data/world/font_normal.json")).unwrap());
    let small = Arc::new(Font::load(root.join("data/world/font_small.json")).unwrap());
    let mut vision = FireRedPerception::default()
        .with_font(font)
        .with_small_font(small);
    let mut sensor = Sensor::new(Arc::new(data));
    let mut state = GameState::default();
    let mut frame_id = 0;
    for name in [
        "emu-party-menu.png",
        "emu-summary-info.png",
        "emu-summary-skills.png",
        "emu-trainer-card.png",
        "emu-save-badges.png",
    ] {
        let image = pokebot_video::png::load(root.join("captures/fixtures").join(name)).unwrap();
        for _ in 0..20 {
            frame_id += 1;
            let frame = NormalizedFrame::new(frame_id, Instant::now(), image.clone()).unwrap();
            let observation = vision.observe(&frame);
            let events: Vec<_> = sensor
                .observe(&observation, &state)
                .into_iter()
                .map(|event| EventRecord { frame_id, event })
                .collect();
            state = DefaultReducer.reduce(&state, &events);
        }
    }
    let mon = &state.party.value.as_ref().unwrap()[0];
    assert_eq!(mon.details.trainer_id.value.as_deref(), Some("62540"));
    assert_eq!(mon.details.original_trainer.value.as_deref(), Some("RED"));
    assert_eq!(mon.details.attack.value, Some(33));
    assert_eq!(mon.details.defense.value, Some(30));
    assert_eq!(mon.details.sp_attack.value, Some(37));
    assert_eq!(mon.details.sp_defense.value, Some(32));
    assert_eq!(mon.details.speed.value, Some(34));
    assert_eq!(mon.details.exp_points.value, Some(3878));
    assert_eq!(mon.details.next_level.value, Some(697));
    assert_eq!(mon.details.ability.value.as_deref(), Some("OVERGROW"));
    assert_eq!(mon.history.len(), 10);
    assert_eq!(
        state.progression.badges.value,
        Some(vec!["BOULDERBADGE".into()])
    );
    assert_eq!(state.progression.badge_count.value, Some(1));
    let snapshot = serde_json::to_vec(&state.saved_knowledge()).unwrap();
    let restored = DefaultReducer.reduce(
        &GameState::default(),
        &[EventRecord {
            frame_id: 1,
            event: pokebot_state::GameEvent::CheckpointRestored {
                knowledge: Box::new(serde_json::from_slice(&snapshot).unwrap()),
            },
        }],
    );
    let restored_party = restored.party.value.as_ref().unwrap();
    assert_eq!(
        restored_party.len(),
        state.party.value.as_ref().unwrap().len()
    );
    assert_eq!(restored_party[0].details, mon.details);
    assert_eq!(restored_party[0].history, mon.history);
    assert_eq!(restored.progression, state.progression);
}
