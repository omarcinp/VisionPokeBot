//! What running a compiled script path teaches the belief: its effects as
//! tracked events (spec §3.2, first row), and the index from text labels
//! back to the scripts and paths that print them.

use std::collections::BTreeMap;

use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, GameState, Pocket};
use pokebot_world::events::{Effect, Events, Val};
use pokebot_world::places::Places;
use pokebot_world::predicate::{BeliefView, Truth};
use pokebot_world::route::requirement_of;

/// Label → (script, path index) of every path whose text includes the
/// label: `say`, a battle's intro/defeat/victory, a gift's "received" text.
#[derive(Debug, Default)]
pub struct LabelIndex {
    by_label: BTreeMap<String, Vec<(String, usize)>>,
}

impl LabelIndex {
    pub fn build(events: &Events) -> LabelIndex {
        let mut by_label: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
        for (name, script) in &events.scripts {
            for (i, path) in script.paths.iter().enumerate() {
                for label in path_labels(&path.does) {
                    let entry = by_label.entry(label.to_owned()).or_default();
                    if entry.last() != Some(&(name.clone(), i)) {
                        entry.push((name.clone(), i));
                    }
                }
            }
        }
        LabelIndex { by_label }
    }

    /// The (script, path) pairs printing `label`.
    pub fn paths_for(&self, label: &str) -> &[(String, usize)] {
        self.by_label.get(label).map_or(&[], Vec::as_slice)
    }

    /// The scripts printing every label in `labels` (sorted).
    pub fn scripts_for(&self, labels: &[String]) -> Vec<String> {
        let mut out: Option<Vec<String>> = None;
        for label in labels {
            let mut scripts: Vec<String> = self
                .paths_for(label)
                .iter()
                .map(|(s, _)| s.clone())
                .collect();
            scripts.sort();
            scripts.dedup();
            out = Some(match out {
                None => scripts,
                Some(prev) => prev.into_iter().filter(|s| scripts.contains(s)).collect(),
            });
        }
        out.unwrap_or_default()
    }
}

/// The text labels a path prints, in order.
pub fn path_labels(does: &[Effect]) -> Vec<&str> {
    let mut out = Vec::new();
    for effect in does {
        match effect {
            Effect::Say { say } => out.push(say.as_str()),
            Effect::Battle {
                intro,
                defeat,
                victory,
                ..
            } => {
                out.extend(intro.as_deref());
                out.extend(defeat.as_deref());
                out.extend(victory.as_deref());
            }
            Effect::Give { text, .. } => out.extend(text.as_deref()),
            _ => {}
        }
    }
    out
}

/// Why an effect produced no event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped(pub String);

/// The event(s) `effect` implies once its path ran, or why none does.
///
/// | Effect | Event |
/// |---|---|
/// | `set` / `clear` flag | `FlagTracked { value: true / false }` |
/// | `defeated` trainer | `FlagTracked` on the trainer id (trainer flags) |
/// | `var` `eq` | `VarTracked` |
/// | `var` `add` / `sub` | `VarTracked` from the tracked value, if known |
/// | `give` / `take` item | `ItemsChanged` (pocket from the game data) |
/// | `warp` to a map | `MapVisited` (map id resolved to its name) |
/// | `heal` | `Healed` |
/// | `respawn` heal location | `RespawnSet` (from `places.json`) |
/// | `money` | `MoneyChanged` |
/// | `givemon` of a known species and level, party known and not full | `PartyMonDerived` in the next slot |
/// | anything else | skipped (`say`, `battle`, objects, eggs…) |
pub fn translate(
    effect: &Effect,
    state: &GameState,
    data: &GameData,
    places: Option<&Places>,
    map_name: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<GameEvent>, Skipped> {
    let skip = |what: &str| Err(Skipped(what.to_owned()));
    match effect {
        Effect::Set { set } => Ok(vec![GameEvent::FlagTracked {
            flag: set.clone(),
            value: true,
        }]),
        Effect::Clear { clear } => Ok(vec![GameEvent::FlagTracked {
            flag: clear.clone(),
            value: false,
        }]),
        Effect::Defeated { defeated } => Ok(vec![GameEvent::FlagTracked {
            flag: defeated.clone(),
            value: true,
        }]),
        Effect::Undefeated { undefeated } => Ok(vec![GameEvent::FlagTracked {
            flag: undefeated.clone(),
            value: false,
        }]),
        Effect::Var { var, change } => {
            let value = if let Some(eq) = &change.eq {
                int(eq).ok_or_else(|| Skipped(format!("var {var} = symbol {eq:?}")))?
            } else {
                let current = state.world.var(var).value.ok_or_else(|| {
                    Skipped(format!("var {var} changed but its value is unknown"))
                })?;
                let delta = |v: &Option<Val>| v.as_ref().and_then(int).unwrap_or(0);
                i64::from(current) + delta(&change.add) - delta(&change.sub)
            };
            let value = u16::try_from(value)
                .map_err(|_| Skipped(format!("var {var} = {value} out of range")))?;
            Ok(vec![GameEvent::VarTracked {
                var: var.clone(),
                value,
            }])
        }
        // The game prints "RED received/obtained/found X!" for every item
        // a script gives, and the runtime's sensor counts it from the text.
        Effect::Give { give, .. } => skip(&format!("give {give} (the sensor reads it)")),
        Effect::Take { take, count } => item_event(data, take, -*count, "script took it"),
        Effect::Warp { warp, .. } => match map_name(warp) {
            Some(map) => Ok(vec![GameEvent::MapVisited { map }]),
            None => skip(&format!("warp to unknown map {warp}")),
        },
        Effect::Heal { heal: true } => Ok(vec![GameEvent::Healed]),
        Effect::Heal { heal: false } => skip("heal: false"),
        Effect::Respawn { respawn } => match places.and_then(|p| p.heal_spot(respawn)) {
            Some(spot) => Ok(vec![GameEvent::RespawnSet {
                map: spot.map.clone(),
                x: spot.x,
                y: spot.y,
            }]),
            None => skip(&format!("respawn {respawn}: no such heal spot")),
        },
        Effect::Money { money } => match int(money) {
            Some(delta) => Ok(vec![GameEvent::MoneyChanged {
                delta,
                reason: "script".into(),
            }]),
            None => skip("money: symbol"),
        },
        Effect::Say { .. } => skip("say"),
        Effect::Battle { battle, .. } => {
            skip(&format!("battle {battle} (the battle tool learns it)"))
        }
        Effect::GiveMon { givemon, level } => {
            let (Val::Sym(species), Some(level)) = (givemon, level.as_int()) else {
                return skip("givemon: species or level not a constant");
            };
            let Some(party) = state.party.value.as_ref() else {
                return skip("givemon: party unknown");
            };
            if party.len() >= 6 {
                return skip("givemon: party full (sent to the PC)");
            }
            let level = u8::try_from(level).map_err(|_| Skipped("givemon: level".into()))?;
            Ok(vec![GameEvent::PartyMonDerived {
                slot: party.len() as u8,
                mon: Box::new(crate::party::starter_mon(data, species, level)),
            }])
        }
        Effect::GiveEgg { .. } => skip("giveegg"),
        Effect::Wild { .. } => skip("wild"),
        Effect::SetWarp { .. } => skip("set_warp"),
        Effect::EscapeWarp { .. } => skip("escape_warp"),
        Effect::MoveObject { .. } => skip("move_object"),
        Effect::AddObject { .. } => skip("add_object"),
        Effect::RemoveObject { .. } => skip("remove_object"),
        Effect::Mart { .. } => skip("mart"),
        Effect::Coins { .. } => skip("coins"),
        Effect::Metatile { .. } => skip("metatile"),
        Effect::MovePlayer { .. } => skip("move_player"),
        Effect::Other(map) => {
            let keys: Vec<&str> = map.keys().map(String::as_str).collect();
            skip(&format!("unknown effect {}", keys.join(",")))
        }
    }
}

fn int(v: &Val) -> Option<i64> {
    v.as_int()
}

fn item_event(
    data: &GameData,
    item: &str,
    delta: i64,
    reason: &str,
) -> Result<Vec<GameEvent>, Skipped> {
    let pocket = data
        .items
        .get(item)
        .and_then(|i| i.pocket.as_deref())
        .and_then(Pocket::from_decomp)
        .ok_or_else(|| Skipped(format!("item {item}: unknown pocket")))?;
    let delta =
        i32::try_from(delta).map_err(|_| Skipped(format!("item {item} x{delta} out of range")))?;
    Ok(vec![GameEvent::ItemsChanged {
        pocket,
        item: item.to_owned(),
        delta,
        reason: reason.to_owned(),
    }])
}

/// The scenes a path sets off by itself: a map's `on_frame` script whose
/// var the path leaves at the scene's value on the map it ends on (Oak's
/// trigger walks the player into the lab and arms its starter scene), each
/// with its one path, in order. The planner chains the same scenes into the
/// path's effects (`pokebot_planner::intents::frame_scene`).
pub fn chained_scenes(
    events: &Events,
    map_name: &dyn Fn(&str) -> Option<String>,
    script: &str,
    path: usize,
) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut at = (script.to_owned(), path);
    for _ in 0..3 {
        let Some(s) = events.script(&at.0) else { break };
        let Some(p) = s.paths.get(at.1) else { break };
        let end = p
            .does
            .iter()
            .rev()
            .find_map(|e| match e {
                Effect::Warp { warp, .. } => map_name(warp),
                _ => None,
            })
            .or_else(|| s.map.clone());
        let Some(end) = end else { break };
        let Some(frames) = events.map_scripts.get(&end) else {
            break;
        };
        let mut set: Vec<(&str, i64)> = Vec::new();
        for e in &p.does {
            if let Effect::Var { var, change } = e {
                if let Some(v) = change.eq.as_ref().and_then(Val::as_int) {
                    set.retain(|(n, _)| *n != var.as_str());
                    set.push((var.as_str(), v));
                }
            }
        }
        let next = frames.on_frame.iter().find_map(|f| {
            let v = f.value.as_int()?;
            if !set.contains(&(f.var.as_str(), v)) {
                return None;
            }
            let label = f.script.as_deref()?;
            (events.script(label)?.paths.len() == 1).then(|| (label.to_owned(), 0))
        });
        match next {
            Some(n) if !out.contains(&n) => {
                out.push(n.clone());
                at = n;
            }
            _ => break,
        }
    }
    out
}

/// The events of running path `path` of `script` to completion:
/// `ScriptPathRun`, then its effects translated ([`translate`]); the
/// skipped effects are returned as log lines.
pub fn path_events(
    events: &Events,
    script: &str,
    path: usize,
    state: &GameState,
    data: &GameData,
    places: Option<&Places>,
    map_name: &dyn Fn(&str) -> Option<String>,
) -> (Vec<GameEvent>, Vec<String>) {
    let mut out = vec![GameEvent::ScriptPathRun {
        script: script.to_owned(),
        path,
    }];
    let mut log = Vec::new();
    let Some(p) = events.script(script).and_then(|s| s.paths.get(path)) else {
        log.push(format!("{script}[{path}]: no such path"));
        return (out, log);
    };
    for effect in &p.does {
        match translate(effect, state, data, places, map_name) {
            Ok(events) => out.extend(events),
            Err(Skipped(why)) => log.push(format!("{script}[{path}]: skipped {why}")),
        }
    }
    (out, log)
}

/// What entering `map` does by itself, silently: its `on_load` and
/// `on_transition` scripts run on every arrival (Pallet Town marks itself
/// on the Town Map, the bedroom sets where a blackout lands). For each
/// script, the paths `belief` doesn't rule out; when one is left, its run
/// and its effects; when several, the effects they all have. Scenes with
/// dialogue (`on_frame`) are the conversation's to recognise.
pub fn entry_events(
    events: &Events,
    map: &str,
    belief: &dyn BeliefView,
    state: &GameState,
    data: &GameData,
    places: Option<&Places>,
    map_name: &dyn Fn(&str) -> Option<String>,
) -> Vec<GameEvent> {
    let Some(ms) = events.map_scripts.get(map) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for label in ms.on_load.iter().chain(&ms.on_transition) {
        let Some(script) = events.script(label) else {
            continue;
        };
        let possible: Vec<usize> = script
            .paths
            .iter()
            .enumerate()
            .filter(|(_, p)| match requirement_of(&p.when) {
                Some(req) => !req.iter().any(|q| belief.eval(q) == Truth::False),
                None => true,
            })
            .map(|(i, _)| i)
            .collect();
        let runs: Vec<Vec<GameEvent>> = possible
            .iter()
            .map(|&i| path_events(events, label, i, state, data, places, map_name).0)
            .collect();
        match runs.as_slice() {
            [] => {}
            [one] => out.extend(one.iter().cloned()),
            [first, rest @ ..] => out.extend(
                first
                    .iter()
                    .filter(|e| !matches!(e, GameEvent::ScriptPathRun { .. }))
                    .filter(|e| rest.iter().all(|r| r.contains(e)))
                    .cloned(),
            ),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pokebot_state::{Knowledge, PartyMon};
    use pokebot_world::World;

    fn world() -> Option<(World, GameData)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world");
        let world = World::load(&dir).ok()?;
        world.events()?;
        let data = GameData::load(dir.join("gamedata.json")).ok()?;
        Some((world, data))
    }

    /// Oak's trigger walks the player into the lab with its scene var at
    /// the starter scene's value: the scene plays too.
    #[test]
    fn a_path_that_arms_a_maps_scene_sets_it_off() {
        let Some((world, _)) = world() else { return };
        let events = world.events().unwrap();
        let name = |id: &str| world.name_of(id).map(str::to_owned);
        assert_eq!(
            chained_scenes(events, &name, "PalletTown_EventScript_OakTriggerLeft", 0),
            vec![(
                "PalletTown_ProfessorOaksLab_ChooseStarterScene".to_owned(),
                0
            )]
        );
        // The Mart's parcel scene arms nothing on arrival.
        assert!(chained_scenes(
            events,
            &name,
            "ViridianCity_Mart_EventScript_ParcelScene",
            0
        )
        .is_empty());
    }

    /// Arriving in the bedroom runs its silent transition script: where a
    /// blackout lands is set. A script whose paths the belief can't tell
    /// apart gives only what they all do (Pallet Town marks itself on the
    /// Town Map whatever the sign lady does).
    #[test]
    fn entering_a_map_books_its_silent_entry_scripts() {
        let Some((world, data)) = world() else { return };
        let events = world.events().unwrap();
        let name = |id: &str| world.name_of(id).map(str::to_owned);
        let mut state = GameState::default();
        state.world.vars.insert(
            "VAR_MAP_SCENE_PALLET_TOWN_PLAYERS_HOUSE_2F".into(),
            Knowledge::derived(0, 0),
        );
        let belief = crate::belief_view::StateBelief(&state);
        let got = entry_events(
            events,
            "PalletTown_PlayersHouse_2F",
            &belief,
            &state,
            &data,
            world.places(),
            &name,
        );
        assert!(
            got.iter()
                .any(|e| matches!(e, GameEvent::RespawnSet { map, .. } if map == "PalletTown")),
            "{got:?}"
        );
        let unknown = GameState::default();
        let belief = crate::belief_view::StateBelief(&unknown);
        let got = entry_events(
            events,
            "PalletTown",
            &belief,
            &unknown,
            &data,
            world.places(),
            &name,
        );
        assert!(got.contains(&GameEvent::FlagTracked {
            flag: "FLAG_WORLD_MAP_PALLET_TOWN".into(),
            value: true
        }));
        assert!(!got
            .iter()
            .any(|e| matches!(e, GameEvent::ScriptPathRun { .. })));
    }

    /// A gift of a Pokémon joins the party (known empty or not full) as
    /// the species at the script's level, with its default moves.
    #[test]
    fn a_given_pokemon_joins_the_party() {
        let Some((world, data)) = world() else { return };
        let _ = world;
        let effect = Effect::GiveMon {
            givemon: Val::Sym("SPECIES_SQUIRTLE".into()),
            level: Val::Int(5),
        };
        let name = |_: &str| None;
        let mut state = GameState::default();
        assert!(
            translate(&effect, &state, &data, None, &name).is_err(),
            "party unknown"
        );
        state.party = Knowledge::derived(Vec::new(), 0);
        match translate(&effect, &state, &data, None, &name)
            .unwrap()
            .as_slice()
        {
            [GameEvent::PartyMonDerived { slot: 0, mon }] => {
                assert_eq!(mon.species.value.as_deref(), Some("SPECIES_SQUIRTLE"));
                assert_eq!(mon.level.value, Some(5));
                assert!(mon.moves[0].is_some());
            }
            other => panic!("{other:?}"),
        }
        state.party = Knowledge::derived(vec![PartyMon::default(); 6], 0);
        assert!(
            translate(&effect, &state, &data, None, &name).is_err(),
            "party full"
        );
    }
}
