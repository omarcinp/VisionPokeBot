//! What running a compiled script path teaches the belief: its effects as
//! tracked events (spec §3.2, first row), and the index from text labels
//! back to the scripts and paths that print them.

use std::collections::BTreeMap;

use pokebot_gamedata::GameData;
use pokebot_state::{GameEvent, GameState, Pocket};
use pokebot_world::events::{Effect, Events, Val};
use pokebot_world::places::Places;

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
/// | anything else | skipped (`say`, `battle`, objects, gifts of Pokémon…) |
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
        Effect::Give { give, count, .. } => item_event(data, give, *count, "script gave it"),
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
        Effect::GiveMon { .. } => skip("givemon"),
        Effect::GiveEgg { .. } => skip("giveegg"),
        Effect::Wild { .. } => skip("wild"),
        Effect::SetWarp { .. } => skip("set_warp"),
        Effect::EscapeWarp { .. } => skip("escape_warp"),
        Effect::MoveObject { .. } => skip("move_object"),
        Effect::AddObject { .. } => skip("add_object"),
        Effect::RemoveObject { .. } => skip("remove_object"),
        Effect::Mart { .. } => skip("mart"),
        Effect::Coins { .. } => skip("coins"),
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
