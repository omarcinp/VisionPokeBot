//! Trainer prize money against pages read in fleet games.

use std::path::Path;

use pokebot_gamedata::GameData;

fn data() -> Option<GameData> {
    GameData::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"))
        .ok()
        .filter(|d| !d.trainer_class_money.classes.is_empty())
}

/// "GOLD got ¥1400 for winning!" after Brock (ONIX Lv14, LEADER ×25) and
/// "GOLD got ¥220" after Camper Liam (SANDSHREW Lv11, CAMPER ×5): 4 × the
/// last Pokémon's level × the class's multiplier.
#[test]
fn prizes_match_the_pages_read() {
    let Some(d) = data() else { return };
    assert_eq!(d.prize("TRAINER_LEADER_BROCK"), Some(1400));
    assert_eq!(d.prize("TRAINER_CAMPER_LIAM"), Some(220));
    assert_eq!(d.prize("TRAINER_NOBODY"), None);
}
