//! Type effectiveness as the game applies it (skipped without the data).

use pokebot_gamedata::GameData;

/// A single-type species lists its type twice; the game applies the second
/// only when it differs (`battle_script_commands.c`: `type1 != type2`). The
/// Switch's Lv35 VENUSAUR scored RAZOR LEAF at a quarter against
/// CHARMELEON, used TACKLE into SMOKESCREEN and lost to the S.S. Anne
/// rival.
#[test]
fn a_type_listed_twice_counts_once() {
    let Ok(data) = GameData::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json"),
    ) else {
        return;
    };
    let fire = ["TYPE_FIRE".to_owned(), "TYPE_FIRE".to_owned()];
    assert_eq!(data.effectiveness("TYPE_GRASS", &fire), 5);
    assert_eq!(data.effectiveness("TYPE_WATER", &fire), 20);
    // Two types still multiply: GRASS against GRASS/POISON is 0.25.
    let venusaur = ["TYPE_GRASS".to_owned(), "TYPE_POISON".to_owned()];
    assert_eq!(data.effectiveness("TYPE_GRASS", &venusaur), 2);
}
