"""Tests for tools/world/compile_events.py.

The fixture tests run anywhere; the tests on the real decompilation skip
when data/pret-pokefirered is missing (like the ROM-backed Rust tests).
"""

import sys
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import compile_events as ce  # noqa: E402

ROOT = HERE.parents[2]
PRET = ROOT / "data/pret-pokefirered"
FIXTURE = HERE / "fixture_scripts.inc"


@pytest.fixture(scope="module")
def parsed():
    return ce.parse_file(FIXTURE)


def commands(parsed, label):
    return [(c.name, c.args) for c in parsed.labels[label]]


def test_labels_and_commands(parsed):
    assert commands(parsed, "Fixture_OnTransition") == [
        ("setworldmapflag", ["FLAG_WORLD_MAP_PALLET_TOWN"]),
        ("end", []),
    ]
    assert commands(parsed, "Fixture_EventScript_Scene") == [
        ("lockall", []),
        ("msgbox", ["Fixture_Text_Hello"]),
        ("releaseall", []),
        ("end", []),
    ]
    leader = commands(parsed, "Fixture_EventScript_Leader")
    assert leader[1] == (
        "trainerbattle_single",
        ["TRAINER_LEADER_BROCK", "Fixture_Text_Intro", "Fixture_Text_Defeat", "Fixture_EventScript_Defeated", "NO_MUSIC"],
    )


def test_equ_aliases_resolve(parsed):
    assert commands(parsed, "Fixture_EventScript_Lady")[1] == ("goto_if_eq", ["VAR_TEMP_2", "TRUE", "Fixture_EventScript_LadyReady"])


def test_map_script_tables(parsed):
    assert commands(parsed, "Fixture_MapScripts") == [
        ("map_script", ["MAP_SCRIPT_ON_TRANSITION", "Fixture_OnTransition"]),
        ("map_script", ["MAP_SCRIPT_ON_FRAME_TABLE", "Fixture_OnFrame"]),
    ]
    assert commands(parsed, "Fixture_OnFrame") == [
        ("map_script_2", ["VAR_MAP_SCENE_PALLET_TOWN_OAK", "2", "Fixture_EventScript_Scene"]),
    ]


def test_data_blocks(parsed):
    assert parsed.data["Fixture_Items"] == ["ITEM_POKE_BALL", "ITEM_POTION", "ITEM_NONE"]


def test_version_conditionals_keep_firered(parsed):
    assert commands(parsed, "Fixture_EventScript_Version")[0] == ("setvar", ["VAR_TEMP_1", "SPECIES_SCYTHER"])


def test_text_pages_and_placeholders(parsed):
    assert parsed.texts["Fixture_Text_Hello"] == "Hello, {PLAYER}!\\nWild POKéMON live in tall grass!\\p{STR_VAR_1} used {STR_VAR_2}!$"
    assert ce.text_pages(parsed.texts["Fixture_Text_Hello"]) == [
        ["Hello, *!", "Wild POKéMON live in tall grass!"],
        ["* used *!"],
    ]
    assert ce.text_pages(parsed.texts["Fixture_Text_Intro"]) == [["So, you're here.", "I'm BROCK!"]]
    assert ce.text_pages("{PKMN} {COLOR RED}{KUN}x{PAUSE 30}$") == [["POKéMON x"]]


# -- IR on the fixture --


@pytest.fixture(scope="module")
def compiler(parsed):
    return ce.Compiler(parsed.labels, parsed.data)


def test_leader_paths_taken_branch_first(compiler):
    paths = compiler.compile("Fixture_EventScript_Leader")["paths"]
    assert len(paths) == 5
    # `goto_if_eq VAR_RESULT, FALSE, NoRoom` is taken first: the no-room
    # paths precede the paths that continue.
    fight_no_room, fight, give_no_room, give, post = paths
    assert fight["when"] == [
        {"trainer": "TRAINER_LEADER_BROCK", "defeated": False},
        {"bagspace": "ITEM_TM39", "count": 1, "has": True},
    ]
    assert fight["does"] == [
        {"battle": "TRAINER_LEADER_BROCK", "intro": "Fixture_Text_Intro", "defeat": "Fixture_Text_Defeat"},
        {"defeated": "TRAINER_LEADER_BROCK"},
        {"set": "FLAG_DEFEATED_BROCK"},
        {"set": "FLAG_BADGE01_GET"},
        {"var": "VAR_MAP_SCENE_PEWTER_CITY", "eq": 1},
        {"say": "Fixture_Text_TakeThis"},
        {"give": "ITEM_TM39", "count": 1, "text": "Fixture_Text_ReceivedTM"},
        {"set": "FLAG_GOT_TM39_FROM_BROCK"},
    ]
    assert "opaque" not in fight  # famechecker/set_gym_trainers are ignored
    assert fight_no_room["when"][-1] == {"bagspace": "ITEM_TM39", "count": 1, "has": False}
    assert give_no_room["does"][-1] == {"say": "Fixture_Text_NoRoom"}
    assert give["when"] == [
        {"trainer": "TRAINER_LEADER_BROCK", "defeated": True},
        {"flag": "FLAG_GOT_TM39_FROM_BROCK", "is": False},
        {"bagspace": "ITEM_TM39", "count": 1, "has": True},
    ]
    assert post["when"] == [
        {"trainer": "TRAINER_LEADER_BROCK", "defeated": True},
        {"flag": "FLAG_GOT_TM39_FROM_BROCK", "is": True},
    ]
    assert post["does"] == [{"say": "Fixture_Text_PostBattle"}]


def test_yesno_call_and_loop_cut(compiler):
    paths = compiler.compile("Fixture_EventScript_Lady")["paths"]
    assert [p["when"] for p in paths] == [
        [{"var": "VAR_TEMP_2", "eq": 1}],
        [{"var": "VAR_TEMP_2", "ne": 1}, {"answer": "yes"}],
        [{"var": "VAR_TEMP_2", "ne": 1}, {"answer": "no"}],
    ]
    # The YES path jumps back to the entry: cut after one iteration.
    assert paths[1]["does"] == [{"say": "Fixture_Text_Hmm"}, {"say": "Fixture_Text_Yes"}]
    # The inlined call's effect lands on the NO path.
    assert paths[2]["does"] == [{"say": "Fixture_Text_Hmm"}, {"set": "FLAG_TEMP_2"}]


def test_a_yes_no_tested_twice_is_one_question(compiler):
    # `goto_if_eq VAR_RESULT, YES` then `goto_if_eq VAR_RESULT, NO`: the
    # second test is decided by the first answer, not a second question
    # (and not a third path where the answer is neither).
    paths = compiler.compile("Fixture_EventScript_Choice")["paths"]
    assert [p["when"] for p in paths] == [[{"answer": "yes"}], [{"answer": "no"}]]
    assert paths[1]["does"] == [{"say": "Fixture_Text_Hmm"}, {"say": "Fixture_Text_Ready"}]


def test_facing_tests_are_decided_once_directions_are_numbers(parsed, tmp_path):
    header = tmp_path / "include/constants/global.h"
    header.parent.mkdir(parents=True)
    header.write_text("#define DIR_NONE 0\n#define DIR_SOUTH 1\n#define DIR_NORTH 2\n")
    saved = dict(ce.CONSTANTS)
    try:
        ce.load_direction_constants(tmp_path)
        assert ce.CONSTANTS["DIR_NORTH"] == 2 and ce.CONSTANTS["DIR_SOUTH"] == 1
        paths = ce.Compiler(parsed.labels, parsed.data).compile("Fixture_EventScript_Facing")["paths"]
    finally:
        ce.CONSTANTS.clear()
        ce.CONSTANTS.update(saved)
    # North, south, neither: the second pair of tests follows the first.
    assert len(paths) == 3
    facing = [[(c.get("eq"), c.get("ne")) for c in p["when"]] for p in paths]
    assert facing[0] == [(2, None)]
    assert facing[1] == [(None, 2), (1, None)]


def test_var_env_resolves_givemon(compiler):
    paths = compiler.compile("Fixture_EventScript_Version")["paths"]
    assert paths[0]["does"][-1] == {"givemon": "SPECIES_SCYTHER", "level": 25}


def test_mart_items(compiler):
    paths = compiler.compile("Fixture_EventScript_Clerk")["paths"]
    assert paths[0]["does"] == [{"mart": ["ITEM_POKE_BALL", "ITEM_POTION"]}]


def test_specialvar_quantities_are_typed(compiler):
    paths = compiler.compile("Fixture_EventScript_Aide")["paths"]
    whens = [p["when"] for p in paths]
    # VAR_0x8006 (caught) copied into VAR_0x8009 keeps its meaning.
    assert whens[0] == [{"pokedex": "caught", "lt": 10}]
    # GetPokedexCount returns whether the National Dex is enabled; with
    # VAR_0x8004 = 1 the counts are National.
    assert whens[1] == [
        {"pokedex": "caught", "ge": 10},
        {"flag": "FLAG_SYS_NATIONAL_DEX", "is": True},
        {"pokedex": "caught", "national": True, "lt": 60},
    ]
    # `compare` + `goto_if_ge` on the seen copy.
    assert whens[3] == [
        {"pokedex": "caught", "ge": 10},
        {"flag": "FLAG_SYS_NATIONAL_DEX", "is": False},
        {"pokedex": "seen", "ge": 20},
    ]
    # Overwriting the copy with a constant drops the meaning (and decides
    # the branch statically: only the taken path exists).
    plenty = [p for p in paths if p["when"][-1] == {"pokedex": "seen", "ge": 20}]
    assert len(plenty) == 1 and plenty[0]["does"][-1] == {"say": "Fixture_Text_NoRoom"}
    give = [p for p in paths if {"give": "ITEM_HM05", "count": 1} in p["does"]]
    assert [p["when"][-1] for p in give] == [{"pokedex": "seen", "lt": 20}]
    assert not any("var" in c for p in paths for c in p["when"])


def test_money_party_coins_typed(compiler):
    paths = compiler.compile("Fixture_EventScript_Vendor")["paths"]
    whens = [p["when"] for p in paths]
    assert whens[0] == [{"money": "player", "lt": 500}]
    assert whens[1] == [{"money": "player", "ge": 500}, {"party": "size", "eq": 6}]
    assert whens[2][-1] == {"coins": "player", "ge": 9990}
    assert whens[3][-1] == {"in_party": "SPECIES_MAGIKARP", "is": False}
    assert whens[4][-1] == {"party": "non_egg", "eq": 1}
    assert whens[5][-1] == {"pokedex_complete": "kanto", "is": True}
    assert whens[6][-1] == {"pokedex_complete": "kanto", "is": False}
    assert compiler.typed_sites["pokedex_complete kanto"] == 1
    assert compiler.typed_sites["money player"] == 1


# -- the real decompilation --


@pytest.fixture(scope="module")
def world():
    if not (PRET / "data/maps").is_dir():
        pytest.skip("data/pret-pokefirered missing (run tools/world/build.sh)")
    return ce.load_world(PRET)


@pytest.fixture(scope="module")
def events(world):
    return ce.compile_events(world)[0]


def test_brock_three_paths(events):
    script = events["scripts"]["PewterCity_Gym_EventScript_Brock"]
    assert script["kind"] == "object" and script["map"] == "PewterCity_Gym"
    paths = script["paths"]
    by_when = {json_key(p["when"]): p for p in paths}
    fight = by_when[json_key([
        {"trainer": "TRAINER_LEADER_BROCK", "defeated": False},
        {"bagspace": "ITEM_TM39", "count": 1, "has": True},
    ])]
    does = fight["does"]
    assert {"battle": "TRAINER_LEADER_BROCK", "intro": "PewterCity_Gym_Text_BrockIntro", "defeat": "PewterCity_Gym_Text_BrockDefeat"} in does
    for effect in [
        {"set": "FLAG_DEFEATED_BROCK"}, {"set": "FLAG_BADGE01_GET"},
        {"var": "VAR_MAP_SCENE_PEWTER_CITY", "eq": 1},
        {"set": "FLAG_HIDE_PEWTER_CITY_GYM_GUIDE"},
        {"clear": "FLAG_HIDE_PEWTER_CITY_RUNNING_SHOES_GUY"},
        {"say": "PewterCity_Gym_Text_TakeThisWithYou"},
        {"give": "ITEM_TM39", "count": 1, "text": "PewterCity_Gym_Text_ReceivedTM39FromBrock"},
        {"set": "FLAG_GOT_TM39_FROM_BROCK"},
        {"say": "PewterCity_Gym_Text_ExplainTM39"},
    ]:
        assert effect in does
    give = by_when[json_key([
        {"trainer": "TRAINER_LEADER_BROCK", "defeated": True},
        {"flag": "FLAG_GOT_TM39_FROM_BROCK", "is": False},
        {"bagspace": "ITEM_TM39", "count": 1, "has": True},
    ])]
    assert {"give": "ITEM_TM39", "count": 1, "text": "PewterCity_Gym_Text_ReceivedTM39FromBrock"} in give["does"]
    post = by_when[json_key([
        {"trainer": "TRAINER_LEADER_BROCK", "defeated": True},
        {"flag": "FLAG_GOT_TM39_FROM_BROCK", "is": True},
    ])]
    assert post["does"] == [{"say": "PewterCity_Gym_Text_BrockPostBattle"}]


def json_key(value):
    import json
    return json.dumps(value, sort_keys=True)


def test_pokedex_aides_are_typed(events):
    gates = {
        "Route2_EastBuilding_EventScript_Aide": ("ITEM_HM05", 10),
        "Route11_EastEntrance_2F_EventScript_Aide": ("ITEM_ITEMFINDER", 30),
        "Route15_WestEntrance_2F_EventScript_Aide": ("ITEM_EXP_SHARE", 50),
        "Route16_NorthEntrance_2F_EventScript_Aide": ("ITEM_AMULET_COIN", 40),
        "Route10_PokemonCenter_1F_EventScript_Aide": ("ITEM_EVERSTONE", 20),
    }
    for label, (item, need) in gates.items():
        paths = events["scripts"][label]["paths"]
        give = [p for p in paths if any(e.get("give") == item for e in p["does"])]
        assert len(give) == 1, label
        assert {"pokedex": "caught", "ge": need} in give[0]["when"], label
        assert not any("var" in c or "special" in c for p in paths for c in p["when"]), label
    scene = events["scripts"]["PalletTown_EventScript_OakRatingScene"]["paths"]
    assert {"pokedex": "caught", "ge": 60} in [c for p in scene for c in p["when"]]
    # Oak's rating: the National Dex check is the flag behind
    # IsNationalPokedexEnabled, completion is HasAllMons.
    oak = events["scripts"]["PalletTown_ProfessorOaksLab_EventScript_ProfOak"]["paths"]
    conds = [c for p in oak for c in p["when"]]
    assert {"flag": "FLAG_SYS_NATIONAL_DEX", "is": False} in conds
    assert {"pokedex_complete": "national", "is": True} in conds


def test_sign_lady_branches(events):
    script = events["scripts"]["PalletTown_EventScript_SignLady"]
    firsts = [p["when"][0] for p in script["paths"]]
    assert {"var": "VAR_MAP_SCENE_PALLET_TOWN_SIGN_LADY", "eq": 2} in firsts
    assert all(p["when"][0]["var"] == "VAR_MAP_SCENE_PALLET_TOWN_SIGN_LADY" for p in script["paths"])
    seconds = [p["when"][1] for p in script["paths"] if len(p["when"]) > 1]
    assert {"var": "VAR_MAP_SCENE_PALLET_TOWN_SIGN_LADY", "eq": 1} in seconds


def test_cut_tree(events):
    script = events["scripts"]["EventScript_CutTree"]
    paths = script["paths"]
    no_badge = [p for p in paths if p["when"] == [{"flag": "FLAG_BADGE02_GET", "is": False}]]
    assert len(no_badge) == 1 and no_badge[0]["does"] == [{"say": "Text_TreeCanBeCutDown"}]
    with_badge = [p for p in paths if p["when"][0] == {"flag": "FLAG_BADGE02_GET", "is": True}]
    conds = [json_key(c) for p in with_badge for c in p["when"]]
    assert json_key({"move": "MOVE_CUT", "known": False}) in conds
    assert json_key({"move": "MOVE_CUT", "known": True}) in conds
    assert json_key({"answer": "yes"}) in conds and json_key({"answer": "no"}) in conds
    cut = [p for p in with_badge if {"answer": "yes"} in p["when"]][0]
    assert {"remove_object": "VAR_LAST_TALKED"} in cut["does"]


def test_route3_trainers_battle(events, world):
    objects = [o for o in events["objects"] if o["map"] == "Route3" and o["trainer_type"]]
    assert len(objects) >= 8
    for o in objects:
        paths = events["scripts"][o["script"]]["paths"]
        battles = [e for p in paths for e in p["does"] if "battle" in e]
        assert battles and all(b["battle"].startswith("TRAINER_") for b in battles), o


def test_map_scripts_and_triggers(events):
    pallet = events["map_scripts"]["PalletTown"]
    assert pallet["on_transition"] == ["PalletTown_OnTransition"]
    assert pallet["on_frame"] == [{"var": "VAR_MAP_SCENE_PALLET_TOWN_OAK", "value": 2, "script": "PalletTown_EventScript_OakRatingScene"}]
    transition = events["scripts"]["PalletTown_OnTransition"]
    assert transition["kind"] == "map"
    assert {"set": "FLAG_WORLD_MAP_PALLET_TOWN"} in transition["paths"][0]["does"]
    moved = [e for p in transition["paths"] for e in p["does"] if "move_object" in e]
    assert {"move_object": 1, "x": 5, "y": 15} in moved
    triggers = [t for t in events["triggers"] if t["map"] == "PalletTown" and t["x"] == 12 and t["y"] == 1]
    assert triggers == [{"map": "PalletTown", "x": 12, "y": 1, "when": [{"var": "VAR_MAP_SCENE_PALLET_TOWN_OAK", "eq": 0}], "script": "PalletTown_EventScript_OakTriggerLeft"}]
    assert events["scripts"]["PalletTown_EventScript_OakTriggerLeft"]["kind"] == "trigger"


def test_oaks_parcel_scene_is_compiled_whole(events):
    # Oak's object script branches on the facing again and again: with
    # the directions as numbers the parcel path (the Pokédex) fits.
    oak = events["scripts"]["PalletTown_ProfessorOaksLab_EventScript_ProfOak"]
    assert not oak.get("truncated")
    dex = [p for p in oak["paths"] if {"set": "FLAG_SYS_POKEDEX_GET"} in p["does"]]
    assert dex and all({"var": "VAR_MAP_SCENE_VIRIDIAN_CITY_MART", "ge": 1} in p["when"] for p in dex)
    ball = events["scripts"]["PalletTown_ProfessorOaksLab_EventScript_BulbasaurBall"]["paths"]
    answers = sorted({json_key([c for c in p["when"] if "answer" in c]) for p in ball})
    assert json_key([{"answer": "yes"}, {"answer": "no"}]) in answers
    assert json_key([{"answer": "yes"}, {"answer": "no"}, {"answer": "no"}]) not in answers


def test_objects_carry_area_and_flag(events):
    lady = [o for o in events["objects"] if o["map"] == "PalletTown" and o["local_id"] == 1][0]
    assert lady["moves"] == "WANDER_AROUND" and (lady["range_x"], lady["range_y"]) == (1, 4)
    assert lady["hidden_by"] is None
    oak = [o for o in events["objects"] if o["map"] == "PalletTown" and o["local_id"] == 3][0]
    assert oak["hidden_by"] == "FLAG_HIDE_OAK_IN_PALLET_TOWN" and oak["script"] is None


def test_dialogue(world):
    dialogue = ce.compile_dialogue(world)
    assert dialogue["labels"]["Text_TreeCanBeCutDown"] == [["This tree looks like it can be CUT", "down!"]]
    assert "Text_TreeCanBeCutDown" in dialogue["index"]["This tree looks like it can be CUT"]
    assert dialogue["labels"]["PalletTown_Text_PlayersHouse"] == [["*'s house"]]


# -- places and obtain --


@pytest.fixture(scope="module")
def places(world, events):
    return ce.compile_places(world, events, ROOT / "data/world")


def test_places(places):
    pewter = [h for h in places["heal_spots"] if h["map"] == "PewterCity"]
    assert pewter == [{"id": "HEAL_LOCATION_PEWTER_CITY", "map": "PewterCity", "x": 17, "y": 26, "respawn_map": "PewterCity_PokemonCenter_1F"}]
    pallet = [f for f in places["fly_spots"] if f["flag"] == "FLAG_WORLD_MAP_PALLET_TOWN"]
    assert pallet == [{"flag": "FLAG_WORLD_MAP_PALLET_TOWN", "map": "PalletTown", "x": 6, "y": 8}]
    route4 = [f for f in places["fly_spots"] if f["flag"] == "FLAG_WORLD_MAP_ROUTE4_POKEMON_CENTER_1F"]
    assert route4 and route4[0]["map"] == "Route4"
    assert "ITEM_POKE_BALL" in places["marts"]["ViridianCity_Mart"]
    trees = [g for g in places["gates"] if g["map"] == "Route2" and g["kind"] == "cut_tree"]
    assert trees and trees[0]["requires"] == {"move": "MOVE_CUT", "badge": "FLAG_BADGE02_GET"}
    assert all(isinstance(g["x"], int) and isinstance(g["y"], int) for g in places["gates"])
    if places["water"]:
        assert places["water"]["Route19"] > 100 and "PalletTown" in places["water"]


@pytest.fixture(scope="module")
def obtain(world, events):
    return ce.compile_obtain(world, events)


def test_obtain_covers_kanto(obtain):
    species = obtain["species"]
    kanto = [s for s in species if species[s]["id"] <= 151]
    assert len(kanto) == 151
    for s in kanto:
        assert species[s]["methods"] or species[s]["reasons"], s
    rattata = [m for m in species["SPECIES_RATTATA"]["methods"] if m["method"] == "wild" and m["map"] == "Route1"]
    assert rattata and rattata[0]["slot"] == "land" and rattata[0]["rate"] > 0
    assert species["SPECIES_ALAKAZAM"]["reasons"] == ["needs_link"]
    alakazam = species["SPECIES_ALAKAZAM"]["methods"]
    assert alakazam == [{"method": "evolve", "from": "SPECIES_KADABRA", "how": "trade", "needs_link": True}]
    eevee = [m for m in species["SPECIES_EEVEE"]["methods"] if m["method"] == "gift"]
    assert eevee and eevee[0]["map"] == "CeladonCity_Condominiums_RoofRoom"
    mr_mime = [m for m in species["SPECIES_MR_MIME"]["methods"] if m["method"] == "trade"]
    assert mr_mime == [{"method": "trade", "give": "SPECIES_ABRA", "script": "Route2_House_EventScript_Reyley", "map": "Route2_House"}]
    ivysaur = [m for m in species["SPECIES_IVYSAUR"]["methods"] if m["method"] == "evolve"]
    assert ivysaur == [{"method": "evolve", "from": "SPECIES_BULBASAUR", "how": "level", "param": 16}]
    raichu = [m for m in species["SPECIES_RAICHU"]["methods"] if m["method"] == "evolve"]
    assert raichu[0]["how"] == "item" and raichu[0]["param"] == "ITEM_THUNDER_STONE"
    scyther = [m for m in species["SPECIES_SCYTHER"]["methods"] if m["method"] == "prize"]
    assert scyther and scyther[0]["coins"] == 5500
    omanyte = [m for m in species["SPECIES_OMANYTE"]["methods"] if m["method"] == "fossil"]
    assert omanyte and omanyte[0]["item"] == "ITEM_HELIX_FOSSIL"
    zapdos = [m for m in species["SPECIES_ZAPDOS"]["methods"] if m["method"] == "static"]
    assert zapdos and zapdos[0]["map"] == "PowerPlant" and zapdos[0]["level"] == 50
    starters = [m for m in species["SPECIES_CHARMANDER"]["methods"] if m["method"] == "gift"]
    assert starters and starters[0]["map"] == "PalletTown_ProfessorOaksLab"
    breed = [m for m in species["SPECIES_PICHU"]["methods"] if m["method"] == "breed"]
    assert breed and "SPECIES_PIKACHU" in breed[0]["baby_of"]
    assert species["SPECIES_MEW"]["reasons"] == ["event_only"]
    assert species["SPECIES_VULPIX"]["reasons"] == ["other_version"]
    assert species["SPECIES_NINETALES"]["reasons"] == ["other_version"]
